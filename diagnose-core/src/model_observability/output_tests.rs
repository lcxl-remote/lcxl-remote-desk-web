use super::*;
use crate::chat::{ModelTurn, StopReason};
use crate::terminal_ai_assistant::{parse_assistant_answer, parse_assistant_answer_observed};
use crate::terminal_complete::{parse_completions, parse_completions_observed};

#[derive(Default)]
struct Recorder(Mutex<Vec<ObservationEvent>>);
impl ObservabilitySeam for Recorder {
    fn submit(&self, event: ObservationEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn context(recorder: Arc<dyn ObservabilitySeam>, id: &str, purpose: Purpose) -> ObservationContext {
    ObservationContext::new(
        id.into(),
        1_000,
        Attribution {
            provider_id: "provider".into(),
            model_id: "model".into(),
            model_name: "configured".into(),
            configuration_revision: "2".into(),
            contract_revision: "1".into(),
            surface: Surface::Terminal,
            purpose,
            origin: Origin::User,
            configuration_scope: ConfigurationScope::Local,
            protocol: Protocol::OpenAiChatCompletions,
        },
        recorder,
    )
}

fn assert_output(recorder: &Recorder, expected: OutputOutcome, private: &str) {
    let events = recorder.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].is_bounded());
    assert_eq!(events[0].phase, ObservationPhase::Output);
    let ObservationPayload::Call(output) = &events[0].payload else {
        panic!("output fact");
    };
    assert_eq!(output.output, expected);
    assert!(events[0].relation.is_none());
    if !private.is_empty() {
        assert!(!serde_json::to_string(&*events).unwrap().contains(private));
    }
}

#[test]
fn lazy_context_capture_is_one_shot_and_never_rebinds_after_consumption() {
    let capture = ObservationCapture::default();
    let recorder = Arc::new(Recorder::default());
    let first = context(recorder.clone(), "actual_call", Purpose::Completion);
    let other = context(recorder.clone(), "later_call", Purpose::Completion);
    capture.publish(first.clone());
    capture.clone().publish(other.clone());
    assert_eq!(capture.take().unwrap().call_id, first.call_id);
    assert!(capture.take().is_none());
    capture.publish(other);
    assert!(capture.take().is_none());
    assert!(recorder.0.lock().unwrap().is_empty());

    let inactive = ObservationCapture::default();
    assert!(inactive.take().is_none());
    inactive.publish(first);
    assert!(inactive.take().is_none());
}

#[test]
fn capture_contention_returns_immediately_without_changing_business_or_the_recorded_call() {
    let capture = ObservationCapture::default();
    let first = context(Arc::new(Recorder::default()), "first", Purpose::Completion);
    capture.publish(first);
    let locked = capture.state.lock().unwrap();
    capture.clone().publish(context(
        Arc::new(Recorder::default()),
        "wrong",
        Purpose::Completion,
    ));
    assert!(capture.take().is_none());
    drop(locked);
    assert_eq!(capture.take().unwrap().call_id, "first");
}

#[test]
fn completion_output_uses_actual_parse_validity_not_the_number_of_visible_candidates() {
    for (text, stop, expected) in [
        (
            r#"{"completions":[]}"#,
            StopReason::EndTurn,
            OutputOutcome::Accepted,
        ),
        (
            r#"{"completions":[{"command":"different prefix"}]}"#,
            StopReason::EndTurn,
            OutputOutcome::Accepted,
        ),
        (
            r#"{"completions":[{"command":"cat /etc/shadow"}]}"#,
            StopReason::EndTurn,
            OutputOutcome::Accepted,
        ),
        (
            r#"{"completions":[{"command":"cat notes.txt","note":"private completion"}]}"#,
            StopReason::EndTurn,
            OutputOutcome::Accepted,
        ),
        (
            r#"{"completions":[{"command":99}]}"#,
            StopReason::EndTurn,
            OutputOutcome::InvalidStructuredOutput,
        ),
        (
            "private malformed response",
            StopReason::EndTurn,
            OutputOutcome::InvalidStructuredOutput,
        ),
        ("", StopReason::EndTurn, OutputOutcome::EmptyResponse),
        (
            r#"{"completions":[]}"#,
            StopReason::MaxTokens,
            OutputOutcome::OutputTruncated,
        ),
    ] {
        let recorder = Arc::new(Recorder::default());
        let context = context(recorder.clone(), "completion", Purpose::Completion);
        let turn = ModelTurn {
            text: text.into(),
            stop_reason: stop,
            ..Default::default()
        };
        let original = serde_json::to_vec(&turn).unwrap();
        let plain = parse_completions(text, "cat ", "bash");
        let observed = parse_completions_observed(&turn, "cat ", "bash", Some(&context), 1_001);
        assert_eq!(
            serde_json::to_value(observed).unwrap(),
            serde_json::to_value(plain).unwrap()
        );
        assert_eq!(serde_json::to_vec(&turn).unwrap(), original);
        assert_output(&recorder, expected, text);
    }
}

#[test]
fn assistant_degraded_display_does_not_become_structured_output_success() {
    for (text, expected) in [
        (
            r#"{"explanation_md":"private explanation","suggestions":[]}"#,
            OutputOutcome::Accepted,
        ),
        (
            "private explanation\n```json\n{\"suggestions\":[]}\n```",
            OutputOutcome::Accepted,
        ),
        (
            "private degraded explanation",
            OutputOutcome::InvalidStructuredOutput,
        ),
        ("", OutputOutcome::EmptyResponse),
    ] {
        let recorder = Arc::new(Recorder::default());
        let context = context(recorder.clone(), "assistant", Purpose::Agent);
        let turn = ModelTurn {
            text: text.into(),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        };
        let original = parse_assistant_answer(text, "bash");
        let observed = parse_assistant_answer_observed(&turn, "bash", Some(&context), 1_001);
        assert_eq!(observed.1, original.1);
        assert_eq!(
            serde_json::to_value(observed.0).unwrap(),
            serde_json::to_value(original.0).unwrap()
        );
        assert_output(&recorder, expected, text);
    }
}

#[test]
fn approval_probe_requires_the_expected_verdict_even_when_the_decision_json_is_valid() {
    use crate::approval_review::*;
    for probe in approval_probe_cases() {
        for correct in [true, false] {
            let verdict = if correct {
                probe.expected_verdict
            } else {
                match probe.expected_verdict {
                    ApprovalVerdict::Approve => ApprovalVerdict::Deny,
                    ApprovalVerdict::Deny => ApprovalVerdict::Approve,
                }
            };
            let turn = ModelTurn {
                text: serde_json::json!({
                    "candidate_id": probe.candidate.candidate_id,
                    "verdict": verdict.as_str(),
                    "reason_code": "fixture_review",
                    "reason": "private probe reason",
                    "evidence_event_ids": [probe.candidate.context.evidence[0].event_id]
                })
                .to_string(),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            };
            assert!(reviewer_model_decision(&probe.candidate, &turn).is_ok());
            let recorder = Arc::new(Recorder::default());
            let mut context = context(recorder.clone(), "probe", Purpose::Probe);
            context.attribution.surface = Surface::Probe;
            context.attribution.configuration_scope = ConfigurationScope::Candidate;
            let result = validate_approval_probe_observed(&probe, &turn, Some(&context), 1_001);
            assert_eq!(
                result,
                if correct {
                    Ok(())
                } else {
                    Err(ApprovalProbeError::Verdict)
                }
            );
            assert_output(
                &recorder,
                if correct {
                    OutputOutcome::Accepted
                } else {
                    OutputOutcome::InvalidStructuredOutput
                },
                "private probe reason",
            );
        }
    }
}
