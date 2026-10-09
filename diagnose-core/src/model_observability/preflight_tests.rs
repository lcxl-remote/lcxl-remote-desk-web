use super::aggregate::{Count, Rate, contribution};
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Recorder(Mutex<Vec<ObservationEvent>>);
impl ObservabilitySeam for Recorder {
    fn submit(&self, event: ObservationEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn unresolved(recorder: Arc<dyn ObservabilitySeam>) -> ObservationContext {
    ObservationContext::new(
        "unresolved_call".into(),
        1_000,
        Attribution::unresolved(Purpose::Completion, Surface::Terminal, Origin::User),
        recorder,
    )
}

fn resolved(recorder: Arc<dyn ObservabilitySeam>) -> ObservationContext {
    let mut context = unresolved(recorder);
    context.call_id = "resolved_call".into();
    context.attribution.provider_id = "actual_provider".into();
    context.attribution.model_id = "actual_model".into();
    context.attribution.model_name = "Actual model".into();
    context.attribution.configuration_revision = "3.4".into();
    context.attribution.configuration_scope = ConfigurationScope::Personal;
    context.attribution.protocol = Protocol::AnthropicMessages;
    context
}

#[test]
fn unresolved_preflight_has_no_model_or_attempt_and_cannot_pollute_quality_rates() {
    let recorder = Arc::new(Recorder::default());
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    {
        let calls = fallback_calls.clone();
        let recorder = recorder.clone();
        let mut guard = PreflightObservation::new(None).with_fallback(move || {
            calls.fetch_add(1, Ordering::Relaxed);
            Some(unresolved(recorder))
        });
        guard.reason(NotStartedReason::Configuration);
        assert_eq!(fallback_calls.load(Ordering::Relaxed), 0);
    }
    assert_eq!(fallback_calls.load(Ordering::Relaxed), 1);
    let events = recorder.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert!(event.is_bounded());
    assert_eq!(event.started_at_ms, 1_000);
    assert!(!event.attribution.model_identity_known());
    assert!(
        event.attribution.model_name.is_empty()
            && event.attribution.configuration_revision.is_empty()
    );
    assert_eq!(
        event.attribution.configuration_scope,
        ConfigurationScope::Unknown
    );
    assert_eq!(event.attribution.protocol, Protocol::Unknown);
    let ObservationPayload::Call(call) = &event.payload else {
        panic!("unsent call only");
    };
    assert_eq!(call.outcome, RequestOutcome::NotStarted);
    assert_eq!(
        call.not_started_reason,
        Some(NotStartedReason::Configuration)
    );
    assert_eq!(call.output, OutputOutcome::NotEvaluated);
    let totals = contribution(&event.payload, false);
    assert_eq!(totals.get(Count::NotStarted), 1);
    for rate in [
        Rate::RequestFailure,
        Rate::AttemptFailure,
        Rate::OutputRejection,
        Rate::InputRejection,
    ] {
        assert_eq!(totals.rate(rate), Some((0, 0)));
    }
}

#[test]
fn actual_context_binding_discards_the_unknown_fallback_before_any_adapter_fact() {
    let recorder = Arc::new(Recorder::default());
    let context = resolved(recorder.clone());
    {
        let mut guard = PreflightObservation::new(None)
            .with_fallback(|| panic!("resolved request used unknown fallback"));
        guard.bind(Some(context.clone()));
        let mut request = RequestObservation::new(context, 1, 0);
        request.outbound(1_001);
        request.finish(RequestOutcome::Returned, OutputOutcome::NotEvaluated, 1_002);
    }
    let events = recorder.0.lock().unwrap();
    assert!(
        events
            .iter()
            .all(|event| event.attribution.model_id == "actual_model")
    );
    let terminal_calls: Vec<_> = events
        .iter()
        .filter(|event| {
            event.phase == ObservationPhase::Terminal
                && matches!(&event.payload, ObservationPayload::Call(_))
        })
        .collect();
    assert_eq!(terminal_calls.len(), 1);
    assert!(
        matches!(&terminal_calls[0].payload, ObservationPayload::Call(call) if call.outcome == RequestOutcome::Returned)
    );
}

#[test]
fn early_known_preflight_rejection_keeps_identity_without_executing_fallback() {
    let recorder = Arc::new(Recorder::default());
    {
        let mut guard = PreflightObservation::new(Some(resolved(recorder.clone())))
            .with_fallback(|| panic!("existing context was replaced"));
        guard.reason(NotStartedReason::RequestValidation);
    }
    let events = recorder.0.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].attribution.model_id, "actual_model");
    assert!(matches!(&events[0].payload, ObservationPayload::Call(call)
        if call.not_started_reason == Some(NotStartedReason::RequestValidation)));
}

#[test]
fn unavailable_binding_and_fallback_panic_do_not_escape_or_invent_a_call() {
    let mut unavailable =
        PreflightObservation::new(None).with_fallback(|| panic!("disabled binding ran fallback"));
    unavailable.bind(None);
    drop(unavailable);
    assert!(
        std::panic::catch_unwind(|| {
            drop(
                PreflightObservation::new(None).with_fallback(|| panic!("observation unavailable")),
            );
        })
        .is_ok()
    );
}

#[test]
fn nested_preflight_guards_keep_one_unsent_fact_and_never_wait_for_snapshot_contention() {
    let recorder = Arc::new(Recorder::default());
    let context = resolved(recorder.clone());
    {
        let _outer = PreflightObservation::new(Some(context.clone()));
        let mut inner = PreflightObservation::new(Some(context.clone()));
        inner.reason(NotStartedReason::Budget);
    }
    assert_eq!(recorder.0.lock().unwrap().len(), 1);
    let another = resolved(recorder.clone());
    let locked = another.snapshot.lock().unwrap();
    drop(PreflightObservation::new(Some(another.clone())));
    drop(locked);
    assert_eq!(recorder.0.lock().unwrap().len(), 1);
}

#[test]
fn adapter_preflight_reason_is_terminal_once_and_drop_before_outbound_has_no_attempt() {
    for reason in [
        NotStartedReason::Cancelled,
        NotStartedReason::Policy,
        NotStartedReason::RequestValidation,
    ] {
        let recorder = Arc::new(Recorder::default());
        {
            let mut request = RequestObservation::new(resolved(recorder.clone()), 1, 0);
            request.not_started(reason, 1_001);
            request.not_started(NotStartedReason::Unknown, 1_002);
            request.finish(
                RequestOutcome::ObservationIncomplete,
                OutputOutcome::NotEvaluated,
                1_003,
            );
        }
        let events = recorder.0.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.phase == ObservationPhase::Terminal)
                .count(),
            1
        );
        assert!(
            events
                .iter()
                .all(|event| !matches!(&event.payload, ObservationPayload::Attempt(_)))
        );
        assert!(
            matches!(&events.last().unwrap().payload, ObservationPayload::Call(call)
            if call.outcome == RequestOutcome::NotStarted && call.not_started_reason == Some(reason))
        );
    }
    let recorder = Arc::new(Recorder::default());
    drop(RequestObservation::new(resolved(recorder.clone()), 1, 0));
    let events = recorder.0.lock().unwrap();
    assert!(
        events
            .iter()
            .all(|event| !matches!(&event.payload, ObservationPayload::Attempt(_)))
    );
    assert!(
        matches!(&events.last().unwrap().payload, ObservationPayload::Call(call)
        if call.outcome == RequestOutcome::NotStarted && call.not_started_reason == Some(NotStartedReason::Unknown))
    );
}
