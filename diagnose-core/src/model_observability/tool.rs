//! Observes existing validation branches without inspecting argument values.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use super::{
    CorrectionStatus, InputConclusion, InputIssue, ObservationAlias, ObservationContext,
    ObservationPayload, ObservationPhase, ObservationRelation, PermissionOutcome, Stage,
    StageOutcome, ToolSnapshot,
};
use crate::{
    chat::{ModelTurnError, ToolCall, ToolSpec},
    seam::TurnSink,
};

const MAX_OBSERVED_TOOLS: usize = 256;
const MAX_PROVIDER_ID_BYTES: usize = 256;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

struct Entry {
    // Ephemeral adapter lookup only; never serialized or submitted.
    provider_call_id: Option<String>,
    snapshot: ToolSnapshot,
    sequence: u32,
    began: Instant,
}

struct State {
    context: ObservationContext,
    entries: Vec<Entry>,
    source_message_id: Option<String>,
}

/// Disabled observation allocates no per-tool state. Borrow contention drops the
/// observation rather than affecting the caller or any nested sink callback.
#[derive(Clone, Default)]
pub struct ToolBatch(Option<Arc<Mutex<State>>>);

impl ToolBatch {
    pub fn new(
        context: Option<ObservationContext>,
        calls: &[ToolCall],
        advertised: &BTreeMap<String, ToolSpec>,
    ) -> Self {
        let Some(context) = context else {
            return Self::default();
        };
        let entries = calls
            .iter()
            .take(MAX_OBSERVED_TOOLS)
            .enumerate()
            .map(|(ordinal, call)| {
                let key = advertised
                    .get(&call.name)
                    .map(|tool| tool.name.clone())
                    .unwrap_or_else(|| "unknown".into());
                let snapshot = ToolSnapshot {
                    ordinal: ordinal as u32,
                    tool_key: key,
                    stages: [
                        Stage::Exposure,
                        Stage::Protocol,
                        Stage::Json,
                        Stage::Schema,
                        Stage::Reference,
                        Stage::Preflight,
                        Stage::Permission,
                        Stage::Dispatch,
                        Stage::Completion,
                    ]
                    .into_iter()
                    .map(|stage| (stage, StageOutcome::NotReached))
                    .collect(),
                    conclusion: InputConclusion::Unknown,
                    issue: InputIssue::None,
                    schema_path: None,
                    permission: PermissionOutcome::NotReached,
                    correction_of: None,
                    correction_status: CorrectionStatus::Uncorrelated,
                    correction_input: None,
                    argument_bytes: call.arguments_json.len().min(u32::MAX as usize) as u32,
                    stage_duration_ms: None,
                };
                context.emit(
                    context.tool_id(snapshot.ordinal),
                    ObservationPhase::Emitted,
                    0,
                    now_ms(),
                    ObservationPayload::Tool(snapshot.clone()),
                );
                Entry {
                    provider_call_id: (call.id.len() <= MAX_PROVIDER_ID_BYTES)
                        .then(|| call.id.clone()),
                    snapshot,
                    sequence: 0,
                    began: Instant::now(),
                }
            })
            .collect();
        Self(Some(Arc::new(Mutex::new(State {
            context,
            entries,
            source_message_id: None,
        }))))
    }

    fn update(&self, ordinal: usize, change: impl FnOnce(&mut ToolSnapshot)) {
        self.update_related(ordinal, change, None);
    }

    fn update_related(
        &self,
        ordinal: usize,
        change: impl FnOnce(&mut ToolSnapshot),
        relation: Option<ObservationRelation>,
    ) {
        let Some(state) = &self.0 else {
            return;
        };
        let emission = {
            let Ok(mut state) = state.try_lock() else {
                return;
            };
            let context = state.context.clone();
            let Some(entry) = state.entries.get_mut(ordinal) else {
                return;
            };
            change(&mut entry.snapshot);
            entry.sequence = entry.sequence.saturating_add(1);
            entry.snapshot.stage_duration_ms =
                Some(entry.began.elapsed().as_millis().min(u64::MAX as u128) as u64);
            (context, entry.sequence, entry.snapshot.clone())
        };
        emission.0.emit_related(
            emission.0.tool_id(emission.2.ordinal),
            ObservationPhase::Stage,
            emission.1,
            now_ms(),
            ObservationPayload::Tool(emission.2),
            relation,
        );
    }

    pub fn stage(&self, ordinal: usize, stage: Stage, outcome: StageOutcome) {
        self.update(ordinal, |tool| {
            tool.stages.insert(stage, outcome);
            if stage == Stage::Preflight
                && outcome == StageOutcome::Passed
                && tool.conclusion != InputConclusion::Rejected
            {
                // Alias mapping starts reference validation. The typed runtime
                // preflight confirms the required source/type/version checks.
                if tool.stages.get(&Stage::Reference) == Some(&StageOutcome::Attempted) {
                    tool.stages.insert(Stage::Reference, StageOutcome::Passed);
                }
                tool.conclusion = InputConclusion::Accepted;
            }
        });
    }

    pub fn input(&self, ordinal: usize) -> ToolObservation {
        ToolObservation {
            batch: self.clone(),
            ordinal,
        }
    }

    pub fn bind_message(&self, message_id: &str) {
        if let Some(state) = &self.0
            && let Ok(mut state) = state.try_lock()
        {
            if state
                .source_message_id
                .as_deref()
                .is_some_and(|original| original != message_id)
            {
                return;
            }
            if ObservationAlias::source_message(message_id, 0).is_some() {
                state.source_message_id = Some(message_id.into());
            }
        }
        let count = self
            .0
            .as_ref()
            .and_then(|state| state.try_lock().ok().map(|state| state.entries.len()))
            .unwrap_or_default();
        for ordinal in 0..count {
            if let Some(alias) = ObservationAlias::source_message(message_id, ordinal as u32) {
                self.update_related(ordinal, |_| {}, Some(ObservationRelation::Bind(alias)));
            }
        }
    }

    pub fn reject(
        &self,
        ordinal: usize,
        stage: Stage,
        issue: InputIssue,
        schema_path: Option<&str>,
    ) {
        self.update(ordinal, |tool| {
            tool.stages.insert(stage, StageOutcome::Failed);
            tool.conclusion = if issue == InputIssue::SchemaUnavailable {
                InputConclusion::Unknown
            } else {
                InputConclusion::Rejected
            };
            tool.issue = issue;
            tool.schema_path = schema_path
                .filter(|path| super::safe_schema_path(path))
                .map(str::to_owned);
        });
    }

    pub fn permission(&self, ordinal: usize, outcome: PermissionOutcome) {
        self.update(ordinal, |tool| {
            tool.permission = outcome;
            tool.stages.insert(
                Stage::Permission,
                match outcome {
                    PermissionOutcome::Approved | PermissionOutcome::Narrowed => {
                        StageOutcome::Passed
                    }
                    PermissionOutcome::Denied
                    | PermissionOutcome::Revoked
                    | PermissionOutcome::PolicyRejected => StageOutcome::Failed,
                    PermissionOutcome::Waiting => StageOutcome::Attempted,
                    PermissionOutcome::NotReached
                    | PermissionOutcome::Unavailable
                    | PermissionOutcome::Expired
                    | PermissionOutcome::Cancelled => StageOutcome::NotReached,
                },
            );
        });
    }

    fn ordinal(&self, provider_call_id: &str) -> Option<usize> {
        let state = self.0.as_ref()?.try_lock().ok()?;
        let mut found = state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.provider_call_id.as_deref() == Some(provider_call_id));
        let ordinal = found.next()?.0;
        found.next().is_none().then_some(ordinal)
    }

    pub fn protocol(&self, error: Option<&ModelTurnError>) {
        let count = self
            .0
            .as_ref()
            .and_then(|state| state.try_lock().ok().map(|s| s.entries.len()))
            .unwrap_or_default();
        match error {
            None => {
                for ordinal in 0..count {
                    self.stage(ordinal, Stage::Protocol, StageOutcome::Passed);
                    // The existing classifier already parsed every argument once.
                    self.stage(ordinal, Stage::Json, StageOutcome::Passed);
                }
            }
            Some(ModelTurnError::InvalidToolArguments { tool_call_id, .. }) => {
                if let Some(ordinal) = self.ordinal(tool_call_id) {
                    self.stage(ordinal, Stage::Protocol, StageOutcome::Passed);
                    self.reject(ordinal, Stage::Json, InputIssue::InvalidJson, None);
                }
            }
            Some(_) => {
                for ordinal in 0..count {
                    self.reject(ordinal, Stage::Protocol, InputIssue::InvalidProtocol, None);
                }
            }
        }
    }

    pub(super) fn correction_candidate(&self) -> Option<super::correction::CorrectionCandidate> {
        let state = self.0.as_ref()?.try_lock().ok()?;
        let [entry] = state.entries.as_slice() else {
            return None;
        };
        (entry.snapshot.tool_key != "unknown").then(|| super::correction::CorrectionCandidate {
            object_id: state.context.tool_id(entry.snapshot.ordinal),
            tool_key: entry.snapshot.tool_key.clone(),
        })
    }

    /// A single rejected input cannot share its opportunity with dispatched or
    /// successful internal reads. Only a saved feedback batch creates a fact.
    pub fn correction_feedback(&self, fence: Option<super::correction::CorrectionFence>) {
        let Some(fence) = fence else {
            return;
        };
        let source = self.0.as_ref().and_then(|state| {
            let state = state.try_lock().ok()?;
            let [entry] = state.entries.as_slice() else {
                return None;
            };
            if entry.snapshot.conclusion != InputConclusion::Rejected
                || entry.snapshot.tool_key == "unknown"
                || entry.snapshot.stages.get(&Stage::Dispatch) != Some(&StageOutcome::NotReached)
            {
                return None;
            }
            ObservationAlias::source_message(
                state.source_message_id.as_deref()?,
                entry.snapshot.ordinal,
            )
        });
        if let Some(source) = source {
            self.update_related(
                0,
                |tool| tool.correction_status = CorrectionStatus::AwaitingResponse,
                Some(ObservationRelation::Correction {
                    source,
                    fact: super::correction::CorrectionFact::Feedback { fence },
                }),
            );
        }
    }

    pub fn observe_sink<'a>(&self, inner: &'a mut dyn TurnSink) -> ObservedTurnSink<'a> {
        ObservedTurnSink {
            inner,
            batch: self.clone(),
        }
    }
}

/// Process-local handle only. Runtimes submit an alias when a durable work ID is
/// known; all association writes happen in the isolated observation worker.
#[derive(Clone, Default)]
pub struct ToolObservation {
    batch: ToolBatch,
    ordinal: usize,
}

/// One serial tool invocation's transient handle. The provider-issued ID is
/// bounded and remains in memory; it is never an observation storage key.
#[derive(Default)]
pub struct ToolObservationSlot(Mutex<Option<(String, ToolObservation)>>);

impl ToolObservationSlot {
    pub fn set(&self, call: &ToolCall, observation: ToolObservation) {
        if let Ok(mut current) = self.0.try_lock() {
            *current =
                (call.id.len() <= MAX_PROVIDER_ID_BYTES).then(|| (call.id.clone(), observation));
        }
    }
    pub fn get(&self, call: &ToolCall) -> ToolObservation {
        self.get_by_call_id(&call.id)
    }
    pub fn get_by_call_id(&self, call_id: &str) -> ToolObservation {
        self.0
            .try_lock()
            .ok()
            .and_then(|value| {
                value
                    .as_ref()
                    .filter(|(id, _)| id == call_id)
                    .map(|(_, value)| value.clone())
            })
            .unwrap_or_default()
    }
}

impl ToolObservation {
    pub fn stage(&self, stage: Stage, outcome: StageOutcome) {
        self.batch.stage(self.ordinal, stage, outcome);
    }
    pub fn reject(&self, stage: Stage, issue: InputIssue) {
        self.batch.reject(self.ordinal, stage, issue, None);
    }
    pub fn permission(&self, outcome: PermissionOutcome) {
        self.batch.permission(self.ordinal, outcome);
    }
    pub fn input_error(&self, error: &desk_agent_protocol::AgentError) {
        match error.kind {
            desk_agent_protocol::AgentErrorKind::InvalidInput => {
                self.reject(Stage::Preflight, InputIssue::Semantic)
            }
            desk_agent_protocol::AgentErrorKind::PermissionDenied
            | desk_agent_protocol::AgentErrorKind::RiskBlocked => {
                self.permission(PermissionOutcome::PolicyRejected)
            }
            _ => {}
        }
    }
    /// Use only the actual input-validation result, before storage or dispatch.
    /// Infrastructure errors leave the validation conclusion unknown.
    pub fn preflight_result<T>(&self, result: &Result<T, desk_agent_protocol::AgentError>) {
        match result {
            Ok(_) => self.stage(Stage::Preflight, StageOutcome::Passed),
            Err(error) => self.input_error(error),
        }
    }
    pub fn bind(&self, alias: ObservationAlias) {
        self.batch
            .update_related(self.ordinal, |_| {}, Some(ObservationRelation::Bind(alias)));
    }
}

pub struct ObservedTurnSink<'a> {
    inner: &'a mut dyn TurnSink,
    batch: ToolBatch,
}

impl TurnSink for ObservedTurnSink<'_> {
    fn on_text_delta(&mut self, value: &str) {
        self.inner.on_text_delta(value);
    }
    fn on_partial_committed(&mut self) {
        self.inner.on_partial_committed();
    }
    fn on_turn_retracted(
        &mut self,
        reason: desk_agent_protocol::content_safety::StreamRetractionReason,
        error: Option<desk_agent_protocol::AgentError>,
    ) {
        self.inner.on_turn_retracted(reason, error);
    }
    fn on_tool_started(&mut self, name: &str, id: &str, arguments: &str) {
        // UI progress precedes validation on several internal tools. Only the
        // native dispatch boundary can observe actual operation dispatch.
        self.inner.on_tool_started(name, id, arguments);
    }
    fn on_awaiting_approval(&mut self, name: &str, id: &str, arguments: &str) {
        // The runtime records waiting after it creates an actual request.
        self.inner.on_awaiting_approval(name, id, arguments);
    }
    fn on_tool_finished(&mut self, id: &str, ok: bool, output: &str, background: Option<&str>) {
        // A usable tool result does not prove a device operation was verified.
        if let Some(ordinal) = self.batch.ordinal(id) {
            self.batch
                .stage(ordinal, Stage::Completion, StageOutcome::Attempted);
        }
        self.inner.on_tool_finished(id, ok, output, background);
    }
    fn on_visual_evidence(
        &mut self,
        evidence: &desk_agent_protocol::visual_evidence::VisualEvidenceFrame,
    ) {
        self.inner.on_visual_evidence(evidence);
    }
    fn on_document_preview(
        &mut self,
        preview: &desk_agent_protocol::document_conversion::DocumentPreviewFrame,
    ) {
        self.inner.on_document_preview(preview);
    }
    fn on_permission_requested(&mut self, id: &str, count: usize) {
        self.inner.on_permission_requested(id, count);
    }
    fn on_answer_committed(&mut self, text: &str) {
        self.inner.on_answer_committed(text);
    }
    fn on_answer_observation(&mut self, context: ObservationContext) {
        self.inner.on_answer_observation(context);
    }
    fn on_context_trimmed(&mut self, id: &str) {
        self.inner.on_context_trimmed(id);
    }
    fn on_context_adjusted(&mut self, id: &str, kind: crate::model_context::ContextNoticeKind) {
        self.inner.on_context_adjusted(id, kind);
    }
    fn on_context_compacted(&mut self, id: &str, generation: u32, count: u32) {
        self.inner.on_context_compacted(id, generation, count);
    }
    fn on_turn_discarded(&mut self) {
        self.inner.on_turn_discarded();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_observability::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ObservationEvent>>);
    impl ObservabilitySeam for Recorder {
        fn submit(&self, event: ObservationEvent) {
            self.0.lock().unwrap().push(event);
        }
    }
    fn context(recorder: Arc<dyn ObservabilitySeam>, id: &str) -> ObservationContext {
        ObservationContext::new(
            id.into(),
            1_000,
            Attribution {
                provider_id: "provider".into(),
                model_id: "model".into(),
                model_name: "model".into(),
                configuration_revision: "1".into(),
                contract_revision: "1".into(),
                surface: Surface::Assistant,
                purpose: Purpose::Agent,
                origin: Origin::User,
                configuration_scope: ConfigurationScope::Local,
                protocol: Protocol::OpenAiChatCompletions,
            },
            recorder,
        )
    }
    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "server_tool".into(),
            arguments_json: "{\"secret\":\"do-not-store\"}".into(),
        }
    }
    fn definitions() -> BTreeMap<String, ToolSpec> {
        BTreeMap::from([(
            "server_tool".into(),
            ToolSpec {
                name: "server_tool".into(),
                description: "registered".into(),
                parameters_schema: serde_json::json!({"type":"object"}),
            },
        )])
    }

    fn last_tool(recorder: &Recorder) -> ToolSnapshot {
        let events = recorder.0.lock().unwrap();
        let ObservationPayload::Tool(tool) = &events.last().unwrap().payload else {
            panic!("tool event required")
        };
        tool.clone()
    }

    fn observed(call: &ToolCall) -> (Arc<Recorder>, ToolBatch) {
        let recorder = Arc::new(Recorder::default());
        let advertised = BTreeMap::from([(
            call.name.clone(),
            ToolSpec {
                name: call.name.clone(),
                description: String::new(),
                parameters_schema: serde_json::json!({"type":"object"}),
            },
        )]);
        let batch = ToolBatch::new(
            Some(context(recorder.clone(), "call")),
            std::slice::from_ref(call),
            &advertised,
        );
        (recorder, batch)
    }

    #[test]
    fn preflight_errors_do_not_turn_policy_or_infrastructure_failures_into_bad_parameters() {
        use desk_agent_protocol::{AgentError, AgentErrorKind};
        for (kind, conclusion, permission) in [
            (
                AgentErrorKind::InvalidInput,
                InputConclusion::Rejected,
                PermissionOutcome::NotReached,
            ),
            (
                AgentErrorKind::PermissionDenied,
                InputConclusion::Unknown,
                PermissionOutcome::PolicyRejected,
            ),
            (
                AgentErrorKind::RiskBlocked,
                InputConclusion::Unknown,
                PermissionOutcome::PolicyRejected,
            ),
            (
                AgentErrorKind::TransportError,
                InputConclusion::Unknown,
                PermissionOutcome::NotReached,
            ),
            (
                AgentErrorKind::Internal,
                InputConclusion::Unknown,
                PermissionOutcome::NotReached,
            ),
            (
                AgentErrorKind::HostAtCapacity,
                InputConclusion::Unknown,
                PermissionOutcome::NotReached,
            ),
            (
                AgentErrorKind::UnsupportedCapability,
                InputConclusion::Unknown,
                PermissionOutcome::NotReached,
            ),
            (
                AgentErrorKind::Timeout,
                InputConclusion::Unknown,
                PermissionOutcome::NotReached,
            ),
        ] {
            let (recorder, batch) = observed(&call("private-provider-id"));
            batch.input(0).preflight_result::<()>(&Err(AgentError {
                kind,
                message: "private-error-message".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }));
            let tool = last_tool(&recorder);
            assert_eq!(tool.conclusion, conclusion);
            assert_eq!(tool.permission, permission);
            assert_eq!(tool.stages[&Stage::Dispatch], StageOutcome::NotReached);
            let encoded = serde_json::to_string(&*recorder.0.lock().unwrap()).unwrap();
            assert!(
                !encoded.contains("private-provider-id")
                    && !encoded.contains("private-error-message")
                    && !encoded.contains("do-not-store")
            );
        }
        let (recorder, batch) = observed(&call("valid"));
        batch.input(0).preflight_result(&Ok::<(), AgentError>(()));
        assert_eq!(last_tool(&recorder).conclusion, InputConclusion::Accepted);
        assert_eq!(
            last_tool(&recorder).permission,
            PermissionOutcome::NotReached
        );
    }

    #[test]
    fn ui_callbacks_do_not_claim_dispatch_approval_or_verified_completion() {
        #[derive(Default)]
        struct Sink {
            started: usize,
            waiting: usize,
            finished: usize,
        }
        impl TurnSink for Sink {
            fn on_text_delta(&mut self, _delta: &str) {}
            fn on_tool_started(&mut self, _name: &str, _id: &str, _arguments: &str) {
                self.started += 1;
            }
            fn on_awaiting_approval(&mut self, _name: &str, _id: &str, _arguments: &str) {
                self.waiting += 1;
            }
            fn on_tool_finished(
                &mut self,
                _id: &str,
                _ok: bool,
                _output: &str,
                _background: Option<&str>,
            ) {
                self.finished += 1;
            }
        }
        let provider = call("private-provider-id");
        let (recorder, batch) = observed(&provider);
        let mut sink = Sink::default();
        {
            let mut observed = batch.observe_sink(&mut sink);
            observed.on_tool_started(&provider.name, &provider.id, &provider.arguments_json);
            observed.on_awaiting_approval(&provider.name, &provider.id, &provider.arguments_json);
            batch
                .input(0)
                .reject(Stage::Preflight, InputIssue::Semantic);
            observed.on_tool_finished(&provider.id, false, "private-output", None);
        }
        assert_eq!((sink.started, sink.waiting, sink.finished), (1, 1, 1));
        let tool = last_tool(&recorder);
        assert_eq!(tool.conclusion, InputConclusion::Rejected);
        assert_eq!(tool.permission, PermissionOutcome::NotReached);
        assert_eq!(tool.stages[&Stage::Dispatch], StageOutcome::NotReached);
        assert_eq!(tool.stages[&Stage::Completion], StageOutcome::Attempted);
        batch.bind_message("server-assistant-message");
        batch.correction_feedback(Some(
            crate::model_observability::correction::CorrectionFence {
                input_revision: 1,
                focus_revision: 1,
                source_input_id: "server-input".into(),
                goal: None,
                provider_id: "provider".into(),
                model_id: "model".into(),
                configuration_revision: "1".into(),
                contract_revision: "1".into(),
            },
        ));
        assert!(recorder.0.lock().unwrap().iter().any(|event| matches!(
            &event.relation,
            Some(ObservationRelation::Correction {
                fact: crate::model_observability::correction::CorrectionFact::Feedback { .. },
                ..
            })
        )));
        let encoded = serde_json::to_string(&*recorder.0.lock().unwrap()).unwrap();
        assert!(!encoded.contains("private-output"));
    }

    fn active_session() -> crate::session::PersistedAgentSession {
        let scope = desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session = crate::session::PersistedAgentSession::new(
            "chat",
            "1",
            "device",
            1,
            scope.clone(),
            "2026-09-09T00:00:00Z",
        );
        session.surface = crate::session::AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session
            .begin_turn(
                "turn",
                Some("input".into()),
                Some("browser".into()),
                1,
                scope,
                "2026-09-09T00:00:00Z",
            )
            .unwrap();
        session
    }

    #[test]
    fn schedule_parser_observes_actual_validation_without_finalizing_reference_checks() {
        let mut session = active_session();
        let mut input = ToolCall {
            id: "provider-schedule-call".into(),
            name: crate::schedule::management_tools::LIST.into(),
            arguments_json: "{}".into(),
        };
        let (recorder, batch) = observed(&input);
        assert!(matches!(
            crate::schedule::management_tools::parse(&session, &input, &batch.input(0)),
            Ok(crate::schedule::management_tools::Action::List {
                after: 0,
                limit: 10
            })
        ));
        let snapshot = last_tool(&recorder);
        assert_eq!(snapshot.conclusion, InputConclusion::Unknown);
        assert_eq!(snapshot.stages[&Stage::Preflight], StageOutcome::Attempted);
        assert_eq!(snapshot.stages[&Stage::Dispatch], StageOutcome::NotReached);
        for invalid in [r#"{"limit":21}"#, r#"{"owner":2}"#, r#"{"after":-1}"#] {
            input.arguments_json = invalid.into();
            let (recorder, batch) = observed(&input);
            assert!(
                crate::schedule::management_tools::parse(&session, &input, &batch.input(0))
                    .is_err()
            );
            assert_eq!(last_tool(&recorder).conclusion, InputConclusion::Rejected);
        }
        input.arguments_json = "{}".into();
        session.trigger_origin = crate::session::TriggerOrigin::ScheduledContinuation;
        let (recorder, batch) = observed(&input);
        assert!(
            crate::schedule::management_tools::parse(&session, &input, &batch.input(0)).is_err()
        );
        assert_eq!(
            last_tool(&recorder).permission,
            PermissionOutcome::PolicyRejected
        );
        assert_eq!(last_tool(&recorder).conclusion, InputConclusion::Unknown);
    }

    #[test]
    fn alias_mapping_does_not_preempt_a_later_reference_rejection() {
        use crate::model_observability::aggregate::{Count, MergeResult, contribution, merge};
        for reject in [true, false] {
            let (recorder, batch) = observed(&call("provider-call"));
            batch.stage(0, Stage::Reference, StageOutcome::Attempted);
            if reject {
                batch
                    .input(0)
                    .reject(Stage::Reference, InputIssue::ReferenceExpired);
            } else {
                batch.input(0).stage(Stage::Preflight, StageOutcome::Passed);
            }
            let events = recorder.0.lock().unwrap();
            let mut merged = events[0].clone();
            for event in &events[1..] {
                assert_ne!(merge(&mut merged, event), MergeResult::Conflict);
            }
            let counts = contribution(&merged.payload, false);
            assert_eq!(counts.get(Count::ReferenceFailed), u64::from(reject));
            assert_eq!(counts.get(Count::ReferencePassed), u64::from(!reject));
            assert_eq!(counts.get(Count::ReferenceAttempted), 0);
            assert_eq!(counts.get(Count::InputRejected), u64::from(reject));
            assert_eq!(counts.get(Count::InputAccepted), u64::from(!reject));
            assert_eq!(counts.get(Count::Tools), 1);
        }
    }

    #[test]
    fn delegation_parser_reports_policy_separately_and_waits_for_durable_reference_checks() {
        let mut session = active_session();
        let mut call = ToolCall {
            id: "call-id".into(),
            name: crate::subagent::tools::STATUS.into(),
            arguments_json: r#"{"task_id":"private-task"}"#.into(),
        };
        let (recorder, batch) = observed(&call);
        assert!(matches!(
            crate::subagent::tools::parse_observed(&session, &call, &batch.input(0)),
            Ok(crate::subagent::tools::Operation::Status { .. })
        ));
        assert_eq!(last_tool(&recorder).conclusion, InputConclusion::Unknown);
        assert_eq!(
            last_tool(&recorder).stages[&Stage::Preflight],
            StageOutcome::Attempted
        );
        call.arguments_json = r#"{"task_id":""}"#.into();
        let (recorder, batch) = observed(&call);
        assert!(crate::subagent::tools::parse_observed(&session, &call, &batch.input(0)).is_err());
        assert_eq!(last_tool(&recorder).conclusion, InputConclusion::Rejected);
        session.surface = crate::session::AgentSessionSurface::TerminalAiAssistant;
        let (recorder, batch) = observed(&call);
        assert!(crate::subagent::tools::parse_observed(&session, &call, &batch.input(0)).is_err());
        assert_eq!(
            last_tool(&recorder).permission,
            PermissionOutcome::PolicyRejected
        );
        assert_eq!(last_tool(&recorder).conclusion, InputConclusion::Unknown);
    }

    #[test]
    fn attachment_paging_distinguishes_bad_selection_expired_cursor_and_unavailable_source() {
        use crate::conversation_attachment::{
            AttachmentMetadata, Availability, ContentKind, digest,
            read::{ReadMode, ReadRequest, read_page_observed},
        };
        let content = "private-source-line-one\nprivate-source-line-two\n";
        let metadata = AttachmentMetadata {
            attachment_id: "attachment".into(),
            conversation_id: "chat".into(),
            actor_id: "1".into(),
            device_id: "device".into(),
            message_id: "original-result".into(),
            tool_call_id: "provider-call".into(),
            part: "body".into(),
            kind: ContentKind::Text,
            media_type: "text/plain".into(),
            original_bytes: content.len() as u64,
            size_bytes: content.len() as u64,
            original_sha256: digest(content.as_bytes()),
            sha256: digest(content.as_bytes()),
            source_truncated: false,
            storage_truncated: false,
            created_at_unix_ms: 1,
            last_accessed_at_unix_ms: 1,
            availability: Availability::Available,
            image_source: None,
            source_envelope: None,
        };
        let request = ReadRequest {
            attachment_id: metadata.attachment_id.clone(),
            selection: ReadMode::Read {
                start_line: None,
                end_line: None,
            },
            limit: 100,
            max_bytes: 4,
            cursor: None,
        };
        let input = ToolCall {
            id: "private-provider-id".into(),
            name: crate::conversation_attachment::READ_ATTACHMENT_TOOL.into(),
            arguments_json: r#"{"attachment_id":"attachment"}"#.into(),
        };
        let (recorder, batch) = observed(&input);
        let page =
            read_page_observed(&metadata, content.as_bytes(), &request, &batch.input(0)).unwrap();
        let source = serde_json::to_string(&metadata).unwrap();
        let expected = crate::conversation_attachment::read::read_page(
            &metadata,
            content.as_bytes(),
            &request,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&page).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(serde_json::to_string(&metadata).unwrap(), source);
        let snapshot = last_tool(&recorder);
        assert_eq!(snapshot.conclusion, InputConclusion::Accepted);
        assert_eq!(snapshot.stages[&Stage::Reference], StageOutcome::Passed);
        assert_eq!(snapshot.stages[&Stage::Dispatch], StageOutcome::NotReached);
        assert_eq!(snapshot.permission, PermissionOutcome::NotReached);

        let mut bad_range = request.clone();
        bad_range.selection = ReadMode::Read {
            start_line: Some(0),
            end_line: None,
        };
        let mut bad_cursor = request.clone();
        bad_cursor.cursor = Some("invalid-cursor%".into());
        let mut stale_cursor = request.clone();
        stale_cursor.cursor = Some(page.cursor.unwrap());
        stale_cursor.selection = ReadMode::Read {
            start_line: Some(2),
            end_line: None,
        };
        for (request, stage, issue) in [
            (bad_range, Stage::Preflight, InputIssue::Semantic),
            (bad_cursor, Stage::Reference, InputIssue::UnknownReference),
            (stale_cursor, Stage::Reference, InputIssue::ReferenceExpired),
        ] {
            let (recorder, batch) = observed(&input);
            assert!(
                read_page_observed(&metadata, content.as_bytes(), &request, &batch.input(0))
                    .is_err()
            );
            let snapshot = last_tool(&recorder);
            assert_eq!(snapshot.conclusion, InputConclusion::Rejected);
            assert_eq!(snapshot.issue, issue);
            assert_eq!(snapshot.stages[&stage], StageOutcome::Failed);
            assert_eq!(snapshot.stages[&Stage::Dispatch], StageOutcome::NotReached);
        }
        let (recorder, batch) = observed(&input);
        assert!(
            read_page_observed(&metadata, b"corrupt-source", &request, &batch.input(0)).is_err()
        );
        assert_eq!(last_tool(&recorder).issue, InputIssue::ReferenceUnavailable);
        let encoded = serde_json::to_string(&*recorder.0.lock().unwrap()).unwrap();
        assert!(
            !encoded.contains("private-source")
                && !encoded.contains("private-provider-id")
                && !encoded.contains("invalid-cursor")
        );
    }

    #[test]
    fn unavailable_or_panicking_observation_preserves_schedule_business_result() {
        struct Panics;
        impl ObservabilitySeam for Panics {
            fn submit(&self, _event: ObservationEvent) {
                panic!("metrics unavailable");
            }
        }
        let session = active_session();
        let call = ToolCall {
            id: "call-id".into(),
            name: crate::schedule::management_tools::LIST.into(),
            arguments_json: r#"{"limit":3}"#.into(),
        };
        let advertised = BTreeMap::from([(
            call.name.clone(),
            ToolSpec {
                name: call.name.clone(),
                description: String::new(),
                parameters_schema: serde_json::json!({"type":"object"}),
            },
        )]);
        let batch = ToolBatch::new(
            Some(context(Arc::new(Panics), "call")),
            std::slice::from_ref(&call),
            &advertised,
        );
        for observation in [batch.input(0), ToolObservation::default()] {
            assert!(matches!(
                crate::schedule::management_tools::parse(&session, &call, &observation),
                Ok(crate::schedule::management_tools::Action::List { after: 0, limit: 3 })
            ));
        }
    }

    #[test]
    fn duplicate_provider_ids_do_not_select_an_arbitrary_input() {
        let recorder = Arc::new(Recorder::default());
        let batch = ToolBatch::new(
            Some(context(recorder.clone(), "call")),
            &[call("duplicate"), call("duplicate")],
            &definitions(),
        );
        batch.protocol(Some(&ModelTurnError::InvalidToolArguments {
            tool_call_id: "duplicate".into(),
            detail: "secret".into(),
        }));
        assert!(batch.ordinal("duplicate").is_none());
        let encoded = serde_json::to_string(&*recorder.0.lock().unwrap()).unwrap();
        assert!(!encoded.contains("do-not-store"));
        assert!(!encoded.contains("duplicate"));
        assert!(!encoded.contains("secret"));
    }

    #[test]
    fn transient_handles_drop_on_contention_and_do_not_persist_provider_ids_or_error_text() {
        let recorder = Arc::new(Recorder::default());
        let provider = call("private-provider-id");
        let batch = ToolBatch::new(
            Some(context(recorder.clone(), "call")),
            std::slice::from_ref(&provider),
            &definitions(),
        );
        let slot = ToolObservationSlot::default();
        slot.set(&provider, batch.input(0));
        slot.get(&provider)
            .input_error(&desk_agent_protocol::AgentError {
                kind: desk_agent_protocol::AgentErrorKind::PermissionDenied,
                message: "private-error-text".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            });
        let events = recorder.0.lock().unwrap();
        let ObservationPayload::Tool(tool) = &events.last().unwrap().payload else {
            panic!("tool event required")
        };
        assert_eq!(tool.conclusion, InputConclusion::Unknown);
        assert_eq!(tool.permission, PermissionOutcome::PolicyRejected);
        drop(events);
        let before = recorder.0.lock().unwrap().len();
        let lock = slot.0.lock().unwrap();
        slot.get(&provider)
            .stage(Stage::Preflight, StageOutcome::Passed);
        slot.set(&provider, batch.input(0));
        drop(lock);
        slot.get(&call("another-id"))
            .stage(Stage::Preflight, StageOutcome::Passed);
        assert_eq!(recorder.0.lock().unwrap().len(), before);
        slot.get_by_call_id("different-private-id")
            .stage(Stage::Preflight, StageOutcome::Passed);
        assert_eq!(recorder.0.lock().unwrap().len(), before);
        slot.get_by_call_id(&provider.id)
            .stage(Stage::Preflight, StageOutcome::Passed);
        assert_eq!(recorder.0.lock().unwrap().len(), before + 1);
        let encoded = serde_json::to_string(&*recorder.0.lock().unwrap()).unwrap();
        assert!(
            !encoded.contains("private-provider-id")
                && !encoded.contains("private-error-text")
                && !encoded.contains("do-not-store")
        );
    }
}
