use super::*;
use crate::model_observability::{
    Attribution, ConfigurationScope, InputConclusion, InputIssue, ObservabilitySeam,
    ObservationContext, ObservationEvent, ObservationPayload, Origin, PermissionOutcome, Protocol,
    Purpose, Stage, StageOutcome, Surface, tool::ToolBatch,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(super) struct Recorder(pub(super) Mutex<Vec<ObservationEvent>>);
impl ObservabilitySeam for Recorder {
    fn submit(&self, event: ObservationEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn batch(call: &ToolCall, recorder: Arc<Recorder>) -> ToolBatch {
    observed(
        call,
        recorder,
        crate::wait_tools::wait_tool_registry().remove(0).spec,
    )
}

pub(super) fn observed(call: &ToolCall, recorder: Arc<Recorder>, spec: ToolSpec) -> ToolBatch {
    let context = ObservationContext::new(
        "server-model-call".into(),
        1_000,
        Attribution {
            provider_id: "provider".into(),
            model_id: "model".into(),
            model_name: "Model".into(),
            configuration_revision: "1".into(),
            contract_revision: "1".into(),
            surface: Surface::Assistant,
            purpose: Purpose::Agent,
            origin: Origin::User,
            configuration_scope: ConfigurationScope::Local,
            protocol: Protocol::OpenAiChatCompletions,
        },
        recorder,
    );
    ToolBatch::new(
        Some(context),
        std::slice::from_ref(call),
        &BTreeMap::from([(spec.name.clone(), spec)]),
    )
}

#[tokio::test]
async fn wait_observes_the_real_reference_check_and_never_dispatches_another_operation() {
    for (arguments, conclusion, issue, wait_count) in [
        (
            r#"{"task_id":""}"#,
            InputConclusion::Rejected,
            InputIssue::Semantic,
            0,
        ),
        (
            r#"{"task_id":"other-task"}"#,
            InputConclusion::Rejected,
            InputIssue::UnknownReference,
            0,
        ),
        (
            r#"{"task_id":"exec_task9"}"#,
            InputConclusion::Accepted,
            InputIssue::None,
            1,
        ),
    ] {
        let store = seeded_executing();
        let mut session = store.inner.borrow().clone().unwrap();
        let model = ScriptModel {
            turns: RefCell::new([].into()),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let runtime = tools_with_waits(vec![], vec![WaitOutcome::StillRunning]);
        let registry = wait_reg();
        let clock = || "2026-06-20T00:01:00Z".into();
        let deps = exec_deps(&store, &model, &runtime, &registry, &clock);
        let call = ToolCall {
            id: "private-provider-call".into(),
            name: crate::wait_tools::WAIT_TOOL_NAME.into(),
            arguments_json: arguments.into(),
        };
        let recorder = Arc::new(Recorder::default());
        let batch = batch(&call, recorder.clone());
        let mut sink = NullTurnSink;
        let mut observed_sink = batch.observe_sink(&mut sink);
        let result = run_wait(
            &deps,
            &mut session,
            &call,
            &[],
            &mut || "server-feedback".into(),
            &mut None,
            &mut observed_sink,
            &batch.input(0),
        )
        .await
        .unwrap();
        assert!(result.is_none());
        assert_eq!(runtime.wait_calls.borrow().len(), wait_count);
        assert!(runtime.exec_calls.borrow().is_empty());
        let events = recorder.0.lock().unwrap();
        assert!(
            events
                .iter()
                .all(|event| matches!(&event.payload, ObservationPayload::Tool(_)))
        );
        let ObservationPayload::Tool(tool) = &events.last().unwrap().payload else {
            panic!("tool observation required")
        };
        assert_eq!(tool.conclusion, conclusion);
        assert_eq!(tool.issue, issue);
        assert_eq!(tool.permission, PermissionOutcome::NotReached);
        assert_eq!(tool.stages[&Stage::Dispatch], StageOutcome::NotReached);
        let encoded = serde_json::to_string(&*events).unwrap();
        assert!(
            !encoded.contains("private-provider-call")
                && !encoded.contains("exec_task9")
                && !encoded.contains("other-task")
        );
    }
}

pub(super) struct ObservedScript<'a> {
    pub(super) script: &'a ScriptModel,
    pub(super) recorder: Arc<Recorder>,
}

#[tokio::test]
async fn terminal_answer_defers_output_until_the_real_renderer_and_never_sends_the_handle() {
    use crate::model_observability::{ObservationPhase, OutputOutcome};
    use crate::terminal_ai_assistant::{
        AssistantStreamSink, parse_committed_assistant_answer_observed,
    };
    for (text, expected) in [
        (
            r#"{"explanation_md":"private terminal explanation","suggestions":[]}"#,
            OutputOutcome::Accepted,
        ),
        (
            "private degraded terminal explanation",
            OutputOutcome::InvalidStructuredOutput,
        ),
    ] {
        let store = MemSession::default();
        let mut session = PersistedAgentSession::new(
            "conv",
            "actor",
            "device",
            1,
            scope(),
            "2026-06-20T00:00:00Z",
        );
        session.adopt_client_metadata(
            None,
            crate::session::AgentSessionSurface::TerminalAiAssistant,
        );
        *store.inner.borrow_mut() = Some(session);
        let script = ScriptModel {
            turns: RefCell::new([answer(text)].into()),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let recorder = Arc::new(Recorder::default());
        let model = ObservedScript {
            script: &script,
            recorder: recorder.clone(),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "no tools".into(),
        };
        let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
        let frames = Rc::new(RefCell::new(Vec::new()));
        let target = frames.clone();
        let mut sink =
            AssistantStreamSink::new(move |frame| target.borrow_mut().push(frame), "request");
        let clock = || "2026-06-20T00:00:01Z".into();
        let result = run_agent_turn(
            &deps(&store, &model, &tools, &registry, &clock),
            claim(),
            ChatMessage::text("user", ChatRole::User, "private terminal requirement"),
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(result, LoopOutcome::Answered(text.into()));
        assert_eq!(script.requests.borrow().len(), 1);
        assert!(tools.calls.borrow().is_empty());
        assert!(frames.borrow().is_empty());
        assert!(
            !recorder
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.phase == ObservationPhase::Output)
        );
        let context = sink
            .take_answer_observation()
            .expect("actual final call handle");
        assert_eq!(context.attribution.surface, Surface::Terminal);
        assert!(sink.take_answer_observation().is_none());
        let (parsed, _) =
            parse_committed_assistant_answer_observed(text, "bash", Some(&context), 1_001);
        sink.emit_final(parsed);
        let events = recorder.0.lock().unwrap();
        let outputs: Vec<_> = events
            .iter()
            .filter(|event| event.phase == ObservationPhase::Output)
            .collect();
        assert_eq!(outputs.len(), 1);
        assert!(
            matches!(&outputs[0].payload, ObservationPayload::Call(call) if call.output == expected)
        );
        assert!(events.iter().all(|event| !matches!(
            &event.payload,
            ObservationPayload::Tool(_) | ObservationPayload::Operation(_)
        )));
        assert!(
            !serde_json::to_string(&*frames.borrow())
                .unwrap()
                .contains(&context.call_id)
        );
        assert!(
            !serde_json::to_string(&store.inner.borrow().as_ref().unwrap().conversation)
                .unwrap()
                .contains(&context.call_id)
        );
    }
}

#[tokio::test]
async fn terminal_answer_observation_callback_panic_cannot_change_the_loop_outcome() {
    struct BrokenSink;
    impl TurnSink for BrokenSink {
        fn on_text_delta(&mut self, _: &str) {}
        fn on_answer_observation(&mut self, _: ObservationContext) {
            panic!("observation unavailable");
        }
    }
    let store = MemSession::default();
    let mut session = PersistedAgentSession::new(
        "conv",
        "actor",
        "device",
        1,
        scope(),
        "2026-06-20T00:00:00Z",
    );
    session.adopt_client_metadata(
        None,
        crate::session::AgentSessionSurface::TerminalAiAssistant,
    );
    *store.inner.borrow_mut() = Some(session);
    let script = ScriptModel {
        turns: RefCell::new([answer("private final answer")].into()),
        requests: Rc::new(RefCell::new(vec![])),
    };
    let model = ObservedScript {
        script: &script,
        recorder: Arc::new(Recorder::default()),
    };
    let tools = RecordingTools {
        calls: Rc::new(RefCell::new(vec![])),
        reply: "no tools".into(),
    };
    let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
    let clock = || "2026-06-20T00:00:01Z".into();
    let outcome = run_agent_turn(
        &deps(&store, &model, &tools, &registry, &clock),
        claim(),
        ChatMessage::text("user", ChatRole::User, "private terminal requirement"),
        &mut BrokenSink,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::Answered("private final answer".into())
    );
    assert_eq!(script.requests.borrow().len(), 1);
}

pub(super) fn assert_protocol_recovery(
    recorder: &Recorder,
    reason: crate::model_observability::protocol_correction::ProtocolCorrectionReason,
    source_index: usize,
    next_index: usize,
    check: crate::model_observability::protocol_correction::ProtocolCorrectionCheck,
) {
    use crate::model_observability::{
        ObservationRelation, protocol_correction::ProtocolCorrectionFact,
    };
    let events = recorder.0.lock().unwrap();
    let facts: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let Some(ObservationRelation::ProtocolCorrection { fact, .. }) = &event.relation {
                Some(fact)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(facts.len(), 4);
    assert!(
        matches!(facts[0],ProtocolCorrectionFact::Feedback { reason:recorded,.. } if *recorded==reason)
    );
    assert!(
        matches!(facts[1],ProtocolCorrectionFact::Projected { source_call_id,next_call_id,.. }
        if source_call_id==&format!("server-model-call.{source_index}") && next_call_id==&format!("server-model-call.{next_index}"))
    );
    assert!(matches!(facts[2], ProtocolCorrectionFact::Returned { .. }));
    assert!(
        matches!(facts[3],ProtocolCorrectionFact::Response { check:recorded,.. } if *recorded==check)
    );
    assert!(events.iter().all(ObservationEvent::is_bounded));
    let encoded = serde_json::to_string(&facts).unwrap();
    assert!(
        !encoded.contains("execute the approved action")
            && !encoded.contains("inspect, then request permission")
    );
}

#[tokio::test]
async fn real_empty_and_truncated_recovery_branches_emit_only_the_first_projected_output_check() {
    use crate::model_observability::{
        ObservationRelation,
        protocol_correction::{
            ProtocolCorrectionCheck, ProtocolCorrectionFact, ProtocolCorrectionReason,
        },
    };
    for (stop, reason, second_rejected) in [
        (
            StopReason::EndTurn,
            ProtocolCorrectionReason::EmptyResponse,
            false,
        ),
        (
            StopReason::MaxTokens,
            ProtocolCorrectionReason::TruncatedOutput,
            false,
        ),
        (
            StopReason::EndTurn,
            ProtocolCorrectionReason::EmptyResponse,
            true,
        ),
    ] {
        let store = MemSession::default();
        let first = ModelTurn {
            stop_reason: stop,
            provider_meta: ProviderResponseMeta::without_reasoning(stop),
            ..Default::default()
        };
        let second = if second_rejected {
            first.clone()
        } else {
            answer("private recovered response")
        };
        let script = ScriptModel {
            turns: RefCell::new([first, second].into()),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let recorder = Arc::new(Recorder::default());
        let model = ObservedScript {
            script: &script,
            recorder: recorder.clone(),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "not invoked".into(),
        };
        let registry = vec![read_tool("sysinfo", Capability::SystemInfo)];
        let clock = || "t".to_string();
        let outcome = run_agent_turn(
            &deps(&store, &model, &tools, &registry, &clock),
            claim(),
            ChatMessage::text(
                "server-owner-message",
                ChatRole::User,
                "private owner requirement",
            ),
            &mut NullTurnSink,
        )
        .await;
        assert_eq!(outcome.is_err(), second_rejected);
        if !second_rejected {
            assert_eq!(
                outcome.unwrap(),
                LoopOutcome::Answered("private recovered response".into())
            );
        }
        assert_eq!(script.requests.borrow().len(), 2);
        assert!(tools.calls.borrow().is_empty());
        let events = recorder.0.lock().unwrap();
        let facts: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if let Some(ObservationRelation::ProtocolCorrection { fact, .. }) = &event.relation
                {
                    Some(fact)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(facts.len(), 4);
        assert!(
            matches!(facts[0],ProtocolCorrectionFact::Feedback { reason:recorded,.. } if *recorded==reason)
        );
        assert!(
            matches!(facts[1],ProtocolCorrectionFact::Projected { source_call_id,next_call_id,.. }
            if source_call_id=="server-model-call.0" && next_call_id=="server-model-call.1")
        );
        assert!(matches!(facts[2], ProtocolCorrectionFact::Returned { .. }));
        assert!(
            matches!(facts[3],ProtocolCorrectionFact::Response { check,.. }
            if *check==if second_rejected { ProtocolCorrectionCheck::Rejected } else { ProtocolCorrectionCheck::Passed })
        );
        assert!(events.iter().all(ObservationEvent::is_bounded));
        let encoded = serde_json::to_string(&*events).unwrap();
        assert!(
            !encoded.contains("private owner requirement")
                && !encoded.contains("private recovered response")
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.payload, ObservationPayload::Operation(_)))
        );
        let requests = script.requests.borrow();
        for request in requests.iter() {
            let messages = serde_json::to_string(&request.messages).unwrap();
            let tools = serde_json::to_string(&request.tools).unwrap();
            assert!(
                !messages.contains("protocol_correction") && !messages.contains("correction_group")
            );
            assert!(!tools.contains("protocol_correction") && !tools.contains("correction_group"));
        }
    }
}

#[async_trait(?Send)]
impl ModelSeam for ObservedScript<'_> {
    fn model_output_token_limit(&self, request: &ModelRequest) -> Result<i64, AgentError> {
        Ok(request
            .caller_output_hard_cap
            .unwrap_or(128_000)
            .min(128_000))
    }

    fn observation_context(
        &self,
        _use_case: crate::model_profile::ModelUseCase,
        origin: Origin,
    ) -> Option<ObservationContext> {
        Some(ObservationContext::new(
            format!("server-model-call.{}", self.script.requests.borrow().len()),
            1_000,
            Attribution {
                provider_id: "provider".into(),
                model_id: "model".into(),
                model_name: "Model".into(),
                configuration_revision: "1".into(),
                contract_revision: "1".into(),
                surface: Surface::Assistant,
                purpose: Purpose::Agent,
                origin,
                configuration_scope: ConfigurationScope::Local,
                protocol: Protocol::OpenAiChatCompletions,
            },
            self.recorder.clone(),
        ))
    }

    async fn context_policy(
        &self,
        requirements: crate::model_capability::ModelRequirements,
    ) -> Result<crate::model_context::PinnedContextPolicy, AgentError> {
        self.script.context_policy(requirements).await
    }

    async fn call(
        &self,
        request: crate::seam::ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError> {
        self.script.call(request, sink).await
    }
}

#[tokio::test]
async fn directory_loop_binds_only_the_published_proposal_and_never_infers_a_denial_from_polling() {
    use crate::model_observability::{
        ObservationAlias, ObservationRelation,
        aggregate::{Count, MergeResult, contribution, merge},
    };
    for decision in [Some(true), Some(false), None] {
        let run = crate::conversation_key::derive_conversation_key(
            "actor",
            "device",
            Some("client"),
            "fallback",
        );
        let store = MemSession {
            directory_review_decision: decision,
            ..Default::default()
        };
        let mut initial =
            PersistedAgentSession::new(&run, "actor", "device", 1, scope(), "2026-06-20T00:00:00Z");
        initial.adopt_client_metadata(
            Some("client"),
            crate::session::AgentSessionSurface::AiAssistant,
        );
        initial.input_revision = 1;
        initial.latest_input_seq = 1;
        *store.inner.borrow_mut() = Some(initial);
        let script=ScriptModel { turns:RefCell::new([
            tool_use_args("private-provider-call",crate::directory_tools::REQUEST_DIRECTORY,
                r#"{"path":"/tmp/private-directory","purpose":"private-directory-purpose"}"#),
            answer("directory decision received"),
        ].into()),requests:Rc::new(RefCell::new(vec![])) };
        let recorder = Arc::new(Recorder::default());
        let model = ObservedScript {
            script: &script,
            recorder: recorder.clone(),
        };
        let tools = RecordingTools {
            calls: Rc::new(RefCell::new(vec![])),
            reply: "no read or execution expected".into(),
        };
        let registry = crate::directory_tools::registry();
        let clock = || "2026-06-20T00:00:01Z".into();
        let heartbeat = DirectoryReviewHeartbeat {
            polls: &store.directory_review_polls,
            stop: decision.is_none(),
        };
        let mut dependencies = deps(&store, &model, &tools, &registry, &clock);
        dependencies.heartbeat = Some(&heartbeat);
        let mut params = claim();
        params.conversation_id = run.clone();
        let outcome = run_agent_turn(
            &dependencies,
            params,
            ChatMessage::text(
                "private-user-message",
                ChatRole::User,
                "private owner requirement",
            ),
            &mut NullTurnSink,
        )
        .await;
        assert_eq!(outcome.is_ok(), decision.is_some());
        assert_eq!(
            tools.calls.borrow().as_slice(),
            ["resolve-directory:/tmp/private-directory"]
        );
        let request_id = stable_lineage_id(
            "directory-proposal",
            &format!("{run}:1:private-provider-call"),
        );
        let alias = ObservationAlias::directory_request(&run, &request_id).unwrap();
        let events = recorder.0.lock().unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.relation == Some(ObservationRelation::Bind(alias.clone())))
                .count(),
            1
        );
        let input_events=events.iter().filter(|event|matches!(&event.payload,ObservationPayload::Tool(tool) if tool.tool_key==crate::directory_tools::REQUEST_DIRECTORY)).collect::<Vec<_>>();
        assert!(!input_events.is_empty());
        let mut restored = input_events[0].clone();
        for event in &input_events[1..] {
            assert_ne!(merge(&mut restored, event), MergeResult::Conflict);
        }
        let ObservationPayload::Tool(tool) = &restored.payload else {
            panic!("tool required")
        };
        // This transient seam has no backend observation publication. Missing
        // approval facts remain waiting even if the business poll returned false.
        assert_eq!(tool.permission, PermissionOutcome::Waiting);
        assert_eq!(tool.conclusion, InputConclusion::Accepted);
        assert_eq!(tool.stages[&Stage::Dispatch], StageOutcome::NotReached);
        let counts = contribution(&restored.payload, false);
        assert_eq!(counts.get(Count::Tools), 1);
        for key in [
            Count::InputRejected,
            Count::PermissionDenied,
            Count::PermissionCancelled,
            Count::OperationsDispatched,
        ] {
            assert_eq!(counts.get(key), 0);
        }
        let encoded = serde_json::to_string(&*events).unwrap();
        for secret in [
            &*run,
            &*request_id,
            "private-provider-call",
            "/tmp/private-directory",
            "private-directory-purpose",
            "private owner requirement",
        ] {
            assert!(!encoded.contains(secret));
        }
    }
}
