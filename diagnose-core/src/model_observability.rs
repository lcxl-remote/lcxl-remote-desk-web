//! Content-free observation contracts. Submission never controls business work.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::Instant,
};

use serde::{Deserialize, Serialize};

use crate::{chat::TokenUsage, model_profile::ModelUseCase};

pub const DEFINITION_VERSION: u32 = 1;
pub const EVENT_SCHEMA_VERSION: u32 = 1;
pub const MAX_EVENT_BYTES: usize = 8 * 1024;
pub const LATENCY_BUCKETS_MS: [u64; 14] = [
    50,
    100,
    250,
    500,
    1_000,
    2_500,
    5_000,
    10_000,
    30_000,
    60_000,
    120_000,
    300_000,
    600_000,
    u64::MAX,
];

macro_rules! closed_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}

pub mod aggregate;
pub mod capacity;
pub mod correction;
pub mod correction_group;
pub mod error_selector;
pub mod operations;
pub mod permission;
pub mod protocol_correction;
pub mod relation;
pub mod runtime;
pub mod tool;

#[cfg(test)]
mod output_tests;

#[cfg(test)]
mod preflight_tests;

pub use error_selector::ErrorSelector;
pub use relation::{ObservationAlias, ObservationRelation};

closed_enum!(Surface {
    Assistant,
    Diagnose,
    Terminal,
    Fleet,
    Support,
    Safety,
    Probe,
    Platform
});
closed_enum!(Purpose {
    Agent,
    Approval,
    Completion,
    ContextCompression,
    Safety,
    Probe,
    FleetNaturalLanguage,
    Support
});
closed_enum!(Origin {
    User,
    PermissionResume,
    WorkCompletion,
    Subagent,
    GoalContinuation,
    ScheduledTask,
    System,
    Unknown
});
closed_enum!(ConfigurationScope {
    Platform,
    Personal,
    Organization,
    Local,
    Candidate,
    Unknown
});
closed_enum!(Protocol {
    OpenAiChatCompletions,
    AnthropicMessages,
    Unknown
});
closed_enum!(RequestOutcome {
    NotStarted,
    Pending,
    Returned,
    HttpError,
    ProviderError,
    TransportError,
    Timeout,
    StreamError,
    Cancelled,
    ObservationIncomplete
});
closed_enum!(NotStartedReason {
    Configuration,
    RequestValidation,
    Budget,
    Permission,
    Policy,
    Cancelled,
    Admission,
    Unknown
});
closed_enum!(OutputOutcome {
    NotEvaluated,
    Accepted,
    InvalidProtocol,
    InvalidStructuredOutput,
    EmptyResponse,
    OutputTruncated,
    ContextLimit,
    PolicyRejected
});
closed_enum!(Stage {
    Exposure,
    Protocol,
    Json,
    Schema,
    Reference,
    Preflight,
    Permission,
    Dispatch,
    Completion
});
closed_enum!(StageOutcome {
    Passed,
    Failed,
    NotApplicable,
    NotReached,
    Attempted
});
closed_enum!(InputConclusion {
    Unknown,
    Accepted,
    Rejected
});
closed_enum!(InputIssue {
    None,
    UnknownTool,
    UnexposedTool,
    InvalidProtocol,
    InvalidJson,
    MissingField,
    UnknownField,
    Type,
    Enum,
    Pattern,
    Length,
    Combination,
    SchemaUnavailable,
    UnknownReference,
    ReferenceType,
    ReferenceExpired,
    ReferenceUnavailable,
    Semantic,
    Precondition,
    Unknown
});
closed_enum!(PermissionOutcome {
    NotReached,
    Waiting,
    Approved,
    Narrowed,
    Denied,
    Revoked,
    PolicyRejected,
    Unavailable,
    Expired,
    Cancelled
});
closed_enum!(OperationOutcome {
    Pending,
    Verified,
    Accepted,
    ChangedUnverified,
    Failed,
    Unknown,
    Cancelled,
    Rejected
});
closed_enum!(CorrectionStatus {
    Uncorrelated,
    AwaitingResponse,
    Linked,
    Ambiguous,
    NotComparable,
    Switched,
    NoResponse
});
closed_enum!(ObservationPhase {
    Started,
    Progress,
    Terminal,
    Output,
    Metered,
    Emitted,
    Stage,
    Dispatched,
    Completed,
    Runtime
});
closed_enum!(RuntimeCategory {
    Turn,
    Compression,
    Inventory,
    Projection,
    Safety,
    Support,
    Admission,
    Budget,
    Estimator,
    Fence,
    RemoteTool,
    Registration,
    Audit,
    Replay,
    BusinessProcessing
});
closed_enum!(ObjectKind {
    Call,
    Attempt,
    Tool,
    Operation,
    Runtime
});

impl Default for ObjectKind {
    fn default() -> Self {
        Self::Call
    }
}

impl From<ModelUseCase> for Purpose {
    fn from(value: ModelUseCase) -> Self {
        match value {
            ModelUseCase::Probe => Self::Probe,
            ModelUseCase::Safety => Self::Safety,
            ModelUseCase::Approval => Self::Approval,
            ModelUseCase::Agent => Self::Agent,
            ModelUseCase::Completion => Self::Completion,
            ModelUseCase::FleetNaturalLanguage => Self::FleetNaturalLanguage,
            ModelUseCase::ContextCompression => Self::ContextCompression,
        }
    }
}

impl From<crate::session::TriggerOrigin> for Origin {
    fn from(value: crate::session::TriggerOrigin) -> Self {
        use crate::session::TriggerOrigin as Trigger;
        match value {
            Trigger::User => Self::User,
            Trigger::PermissionDecision => Self::PermissionResume,
            Trigger::ScheduledContinuation | Trigger::ScheduledTask => Self::ScheduledTask,
            Trigger::GoalContinuation => Self::GoalContinuation,
            Trigger::DelegatedTask => Self::Subagent,
            Trigger::SubAgentCompletion
            | Trigger::ExecCompletion
            | Trigger::WorkCompletion { .. } => Self::WorkCompletion,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attribution {
    pub provider_id: String,
    pub model_id: String,
    pub model_name: String,
    pub configuration_revision: String,
    pub contract_revision: String,
    pub surface: Surface,
    pub purpose: Purpose,
    pub origin: Origin,
    pub configuration_scope: ConfigurationScope,
    pub protocol: Protocol,
}

impl Attribution {
    /// No model has been selected. Empty identities are absence, never aliases
    /// for a requested, previously selected or default configured model.
    pub fn unresolved(purpose: Purpose, surface: Surface, origin: Origin) -> Self {
        Self {
            provider_id: String::new(),
            model_id: String::new(),
            model_name: String::new(),
            configuration_revision: String::new(),
            contract_revision: DEFINITION_VERSION.to_string(),
            purpose,
            surface,
            origin,
            configuration_scope: ConfigurationScope::Unknown,
            protocol: Protocol::Unknown,
        }
    }

    pub fn model_identity_known(&self) -> bool {
        !self.provider_id.is_empty() && !self.model_id.is_empty()
    }

    pub fn is_bounded(&self) -> bool {
        [
            (&self.provider_id, 128),
            (&self.model_id, 128),
            (&self.model_name, 128),
            (&self.configuration_revision, 64),
            (&self.contract_revision, 64),
        ]
        .into_iter()
        .all(|(value, limit)| value.len() <= limit && !value.chars().any(char::is_control))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timing {
    pub admission_ms: Option<u64>,
    pub headers_ms: Option<u64>,
    pub first_content_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub output_check_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageReport {
    pub normalized: TokenUsage,
    pub cache_write_applicable: bool,
    pub estimated: bool,
}

impl UsageReport {
    pub fn basic_complete(&self) -> bool {
        matches!((self.normalized.input_tokens, self.normalized.output_tokens), (Some(i), Some(o)) if i >= 0 && o >= 0)
    }

    pub fn cache_pair(&self) -> Option<(u64, u64)> {
        let input = u64::try_from(self.normalized.input_tokens?).ok()?;
        let read = u64::try_from(self.normalized.cache_read_tokens?).ok()?;
        let write = if self.cache_write_applicable {
            u64::try_from(self.normalized.cache_write_tokens?).ok()?
        } else {
            0
        };
        Some((read, input.checked_add(read)?.checked_add(write)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallSnapshot {
    pub outcome: RequestOutcome,
    pub not_started_reason: Option<NotStartedReason>,
    pub output: OutputOutcome,
    pub http_status: Option<u16>,
    pub timing: Timing,
    pub usage: UsageReport,
    pub message_count: u32,
    pub advertised_tool_count: u32,
    pub generated_tool_count: Option<u32>,
    pub request_bytes: Option<u64>,
    pub response_bytes: Option<u64>,
    pub output_limit: Option<u64>,
    pub metered_at_ms: Option<i64>,
}

impl Default for CallSnapshot {
    fn default() -> Self {
        Self {
            outcome: RequestOutcome::NotStarted,
            not_started_reason: None,
            output: OutputOutcome::NotEvaluated,
            http_status: None,
            timing: Timing::default(),
            usage: UsageReport::default(),
            message_count: 0,
            advertised_tool_count: 0,
            generated_tool_count: None,
            request_bytes: None,
            response_bytes: None,
            output_limit: None,
            metered_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptSnapshot {
    pub ordinal: u32,
    pub outbound_at_ms: i64,
    pub outcome: RequestOutcome,
    pub http_status: Option<u16>,
    pub timing: Timing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSnapshot {
    pub ordinal: u32,
    /// A server-registered key, never an unvalidated model-supplied name.
    pub tool_key: String,
    pub stages: BTreeMap<Stage, StageOutcome>,
    pub conclusion: InputConclusion,
    pub issue: InputIssue,
    /// Only a path selected from the server's static schema is permitted.
    pub schema_path: Option<String>,
    pub permission: PermissionOutcome,
    pub correction_of: Option<String>,
    pub correction_status: CorrectionStatus,
    /// Next-input validation belongs to this original feedback cohort.
    pub correction_input: Option<InputConclusion>,
    pub argument_bytes: u32,
    pub stage_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationSnapshot {
    pub tool_observation_id: Option<String>,
    pub tool_key: Option<String>,
    pub ordinal: u32,
    /// None means the native per-step dispatch was not observed.
    pub dispatched: Option<bool>,
    pub outcome: OperationOutcome,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshot {
    pub definition: runtime::RuntimeDefinition,
    pub labels: runtime::RuntimeLabels,
    pub value: u64,
    pub duration_ms: Option<u64>,
    pub quantities: BTreeMap<runtime::RuntimeQuantity, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "snapshot",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ObservationPayload {
    Call(CallSnapshot),
    Attempt(AttemptSnapshot),
    Tool(ToolSnapshot),
    Operation(OperationSnapshot),
    Runtime(RuntimeSnapshot),
}

impl ObservationPayload {
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Call(_) => ObjectKind::Call,
            Self::Attempt(_) => ObjectKind::Attempt,
            Self::Tool(_) => ObjectKind::Tool,
            Self::Operation(_) => ObjectKind::Operation,
            Self::Runtime(_) => ObjectKind::Runtime,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub object_id: String,
    pub call_id: Option<String>,
    pub phase: ObservationPhase,
    pub sequence: u32,
    pub started_at_ms: i64,
    pub occurred_at_ms: i64,
    pub attribution: Attribution,
    pub payload: ObservationPayload,
    pub relation: Option<ObservationRelation>,
}

pub fn safe_schema_path(path: &str) -> bool {
    if path.len() > 192 || !path.starts_with('$') {
        return false;
    }
    let mut rest = &path[1..];
    while !rest.is_empty() {
        if let Some(next) = rest.strip_prefix("[]") {
            rest = next;
        } else if let Some(next) = rest.strip_prefix('.') {
            let length = next.find(['.', '[']).unwrap_or(next.len());
            let field = &next[..length];
            if field.is_empty()
                || (field != "*"
                    && !field
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
            {
                return false;
            }
            rest = &next[length..];
        } else {
            return false;
        }
    }
    true
}

impl ObservationEvent {
    pub fn is_bounded(&self) -> bool {
        let id_ok = |id: &str| {
            !id.is_empty()
                && id.len() <= 192
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        };
        self.schema_version == EVENT_SCHEMA_VERSION
            && self.attribution.is_bounded()
            && self
                .relation
                .as_ref()
                .is_none_or(|relation| relation.accepts(&self.payload))
            && id_ok(&self.event_id)
            && id_ok(&self.object_id)
            && self.call_id.as_deref().is_none_or(id_ok)
            && match &self.payload {
                ObservationPayload::Tool(tool) => {
                    tool.tool_key.len() <= 96
                        && tool
                            .tool_key
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.".contains(&b))
                        && tool.schema_path.as_deref().is_none_or(safe_schema_path)
                        && tool.correction_of.as_deref().is_none_or(id_ok)
                }
                ObservationPayload::Operation(op) => {
                    op.tool_observation_id.as_deref().is_none_or(id_ok)
                        && op.tool_key.as_deref().is_none_or(|key| {
                            !key.is_empty()
                                && key.len() <= 96
                                && key
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"_.".contains(&b))
                        })
                }
                ObservationPayload::Runtime(_) => true,
                _ => true,
            }
    }
}

/// Implementations only submit to a bounded queue. They never await storage.
pub trait ObservabilitySeam: Send + Sync {
    fn submit(&self, event: ObservationEvent);
}

#[derive(Clone)]
pub struct ObservationContext {
    pub call_id: String,
    pub started_at_ms: i64,
    pub attribution: Attribution,
    seam: Arc<dyn ObservabilitySeam>,
    snapshot: Arc<Mutex<Option<CallSnapshot>>>,
}

impl fmt::Debug for ObservationContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservationContext")
            .field("call_id", &self.call_id)
            .finish_non_exhaustive()
    }
}

/// One call's host-only handle, published after the existing model resolution.
/// Contention loses observation rather than waiting on the business path.
#[derive(Clone, Default)]
pub struct ObservationCapture {
    state: Arc<Mutex<CaptureState>>,
}

#[derive(Default)]
enum CaptureState {
    #[default]
    Vacant,
    Recorded(ObservationContext),
    Taken,
}

impl ObservationCapture {
    pub fn publish(&self, context: ObservationContext) {
        if let Ok(mut state) = self.state.try_lock()
            && matches!(*state, CaptureState::Vacant)
        {
            *state = CaptureState::Recorded(context);
        }
    }

    pub fn take(&self) -> Option<ObservationContext> {
        let mut state = self.state.try_lock().ok()?;
        match std::mem::replace(&mut *state, CaptureState::Taken) {
            CaptureState::Recorded(context) => Some(context),
            CaptureState::Vacant | CaptureState::Taken => None,
        }
    }
}

/// Record the caller's actual structured-output check. Its business parser may
/// still return a degraded display or an empty, filtered candidate list.
pub fn record_structured_output(
    turn: &crate::chat::ModelTurn,
    passed: bool,
    observation: Option<&ObservationContext>,
    now_ms: i64,
) {
    use crate::chat::StopReason;
    let Some(observation) = observation else {
        return;
    };
    let outcome = match turn.stop_reason {
        StopReason::MaxTokens => OutputOutcome::OutputTruncated,
        StopReason::ContextWindowExceeded => OutputOutcome::ContextLimit,
        StopReason::ToolUse | StopReason::Other => OutputOutcome::InvalidProtocol,
        StopReason::EndTurn if !turn.tool_calls.is_empty() => OutputOutcome::InvalidProtocol,
        StopReason::EndTurn if turn.text.trim().is_empty() => OutputOutcome::EmptyResponse,
        StopReason::EndTurn if passed => OutputOutcome::Accepted,
        StopReason::EndTurn => OutputOutcome::InvalidStructuredOutput,
    };
    observation.output(outcome, now_ms);
}

impl ObservationContext {
    pub fn output(&self, outcome: OutputOutcome, now_ms: i64) {
        self.call(
            ObservationPhase::Output,
            0,
            now_ms,
            CallSnapshot {
                output: outcome,
                ..Default::default()
            },
        );
    }

    pub fn metered(&self, now_ms: i64) {
        self.call(
            ObservationPhase::Metered,
            0,
            now_ms,
            CallSnapshot {
                metered_at_ms: Some(now_ms),
                ..Default::default()
            },
        );
    }

    pub fn timeout(&self, now_ms: i64) {
        let snapshot = self
            .snapshot
            .try_lock()
            .ok()
            .and_then(|value| value.clone());
        if let Some(mut snapshot) = snapshot
            && matches!(
                snapshot.outcome,
                RequestOutcome::Pending | RequestOutcome::ObservationIncomplete
            )
        {
            snapshot.outcome = RequestOutcome::Timeout;
            self.call(ObservationPhase::Terminal, 1, now_ms, snapshot);
        }
    }
    pub fn new(
        call_id: String,
        started_at_ms: i64,
        attribution: Attribution,
        seam: Arc<dyn ObservabilitySeam>,
    ) -> Self {
        Self {
            call_id,
            started_at_ms,
            attribution,
            seam,
            snapshot: Arc::new(Mutex::new(None)),
        }
    }

    pub fn emit(
        &self,
        object_id: String,
        phase: ObservationPhase,
        sequence: u32,
        occurred_at_ms: i64,
        payload: ObservationPayload,
    ) {
        self.emit_related(object_id, phase, sequence, occurred_at_ms, payload, None);
    }

    pub fn emit_related(
        &self,
        object_id: String,
        phase: ObservationPhase,
        sequence: u32,
        occurred_at_ms: i64,
        payload: ObservationPayload,
        relation: Option<ObservationRelation>,
    ) {
        let event = ObservationEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            event_id: format!("{object_id}.{}.{sequence}", phase as u8),
            object_id,
            call_id: Some(self.call_id.clone()),
            phase,
            sequence,
            started_at_ms: self.started_at_ms,
            occurred_at_ms,
            attribution: self.attribution.clone(),
            payload,
            relation,
        };
        if event.is_bounded() {
            let _ =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.seam.submit(event)));
        }
    }

    pub fn call(
        &self,
        phase: ObservationPhase,
        sequence: u32,
        occurred_at_ms: i64,
        snapshot: CallSnapshot,
    ) {
        if matches!(
            phase,
            ObservationPhase::Started | ObservationPhase::Progress | ObservationPhase::Terminal
        ) && let Ok(mut current) = self.snapshot.try_lock()
        {
            *current = Some(snapshot.clone());
        }
        self.emit(
            self.call_id.clone(),
            phase,
            sequence,
            occurred_at_ms,
            ObservationPayload::Call(snapshot),
        );
    }

    pub fn tool_id(&self, ordinal: u32) -> String {
        format!("{}.tool.{ordinal}", self.call_id)
    }
    pub fn attempt_id(&self, ordinal: u32) -> String {
        format!("{}.attempt.{ordinal}", self.call_id)
    }
}

pub fn bounded_label(value: &str, limit: usize) -> String {
    let mut result = String::new();
    for character in value.chars().filter(|c| !c.is_control()) {
        if result.len() + character.len_utf8() > limit {
            break;
        }
        result.push(character);
    }
    result
}

/// Covers host validation before an adapter starts. An adapter-owned snapshot
/// always takes precedence, including when an outer accounting step fails.
pub struct PreflightObservation<'a> {
    context: Option<ObservationContext>,
    reason: NotStartedReason,
    fallback: Option<Box<dyn FnOnce() -> Option<ObservationContext> + 'a>>,
}

impl<'a> PreflightObservation<'a> {
    pub fn new(context: Option<ObservationContext>) -> Self {
        Self {
            context,
            reason: NotStartedReason::Unknown,
            fallback: None,
        }
    }

    pub fn reason(&mut self, reason: NotStartedReason) {
        self.reason = reason;
    }

    /// The host may read already resolved metadata on drop, without doing I/O.
    /// A successful binding discards this fallback before the adapter starts.
    pub fn with_fallback(
        mut self,
        fallback: impl FnOnce() -> Option<ObservationContext> + 'a,
    ) -> Self {
        self.fallback = Some(Box::new(fallback));
        self
    }

    pub fn bind(&mut self, context: Option<ObservationContext>) {
        self.context = context;
        self.fallback = None;
    }
}

impl Drop for PreflightObservation<'_> {
    fn drop(&mut self) {
        if self.context.is_none()
            && let Some(fallback) = self.fallback.take()
        {
            self.context = std::panic::catch_unwind(std::panic::AssertUnwindSafe(fallback))
                .ok()
                .flatten();
        }
        let Some(context) = &self.context else {
            return;
        };
        let untouched = context
            .snapshot
            .try_lock()
            .ok()
            .is_some_and(|snapshot| snapshot.is_none());
        if untouched {
            context.call(
                ObservationPhase::Terminal,
                0,
                tool::now_ms(),
                CallSnapshot {
                    not_started_reason: Some(self.reason),
                    ..Default::default()
                },
            );
        }
    }
}

/// Adapter-owned fixed-size timing state. No streaming content is retained.
pub struct RequestObservation {
    pub context: ObservationContext,
    pub snapshot: CallSnapshot,
    pub attempt: AttemptSnapshot,
    clock: Option<Instant>,
    terminal: bool,
}

impl RequestObservation {
    pub fn new(context: ObservationContext, message_count: usize, tool_count: usize) -> Self {
        let snapshot = CallSnapshot {
            message_count: message_count.min(u32::MAX as usize) as u32,
            advertised_tool_count: tool_count.min(u32::MAX as usize) as u32,
            ..Default::default()
        };
        context.call(
            ObservationPhase::Started,
            0,
            context.started_at_ms,
            snapshot.clone(),
        );
        let attempt = AttemptSnapshot {
            ordinal: 0,
            outbound_at_ms: context.started_at_ms,
            outcome: RequestOutcome::NotStarted,
            http_status: None,
            timing: Timing::default(),
        };
        Self {
            context,
            snapshot,
            attempt,
            clock: None,
            terminal: false,
        }
    }

    pub fn outbound(&mut self, now_ms: i64) {
        self.clock = Some(Instant::now());
        self.snapshot.outcome = RequestOutcome::Pending;
        self.attempt.outbound_at_ms = now_ms;
        self.attempt.outcome = RequestOutcome::Pending;
        self.context
            .call(ObservationPhase::Progress, 0, now_ms, self.snapshot.clone());
        self.context.emit(
            self.context.attempt_id(self.attempt.ordinal),
            ObservationPhase::Started,
            0,
            now_ms,
            ObservationPayload::Attempt(self.attempt.clone()),
        );
    }

    pub fn headers(&mut self, status: u16) {
        self.snapshot.http_status = Some(status);
        self.snapshot.timing.headers_ms = self.elapsed();
        self.attempt.http_status = Some(status);
        self.attempt.timing.headers_ms = self.snapshot.timing.headers_ms;
    }

    pub fn content(&mut self) {
        if self.snapshot.timing.first_content_ms.is_none() {
            self.snapshot.timing.first_content_ms = self.elapsed();
            self.attempt.timing.first_content_ms = self.snapshot.timing.first_content_ms;
        }
    }

    pub fn not_started(&mut self, reason: NotStartedReason, now_ms: i64) {
        if self.terminal || self.has_started() {
            return;
        }
        self.snapshot.not_started_reason = Some(reason);
        self.finish(
            RequestOutcome::NotStarted,
            OutputOutcome::NotEvaluated,
            now_ms,
        );
    }

    pub fn finish(&mut self, outcome: RequestOutcome, output: OutputOutcome, now_ms: i64) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        self.snapshot.outcome = outcome;
        self.snapshot.output = output;
        self.snapshot.timing.duration_ms = self.elapsed();
        if self.clock.is_some() {
            self.attempt.outcome = outcome;
            self.attempt.timing.duration_ms = self.snapshot.timing.duration_ms;
            self.context.emit(
                self.context.attempt_id(self.attempt.ordinal),
                ObservationPhase::Terminal,
                0,
                now_ms,
                ObservationPayload::Attempt(self.attempt.clone()),
            );
        }
        self.context
            .call(ObservationPhase::Terminal, 0, now_ms, self.snapshot.clone());
    }

    fn elapsed(&self) -> Option<u64> {
        self.clock
            .map(|clock| clock.elapsed().as_millis().min(u64::MAX as u128) as u64)
    }

    pub fn has_started(&self) -> bool {
        self.clock.is_some()
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        if !self.terminal {
            let start = if self.has_started() {
                self.attempt.outbound_at_ms
            } else {
                self.context.started_at_ms
            };
            let now = start
                .saturating_add(self.elapsed().unwrap_or_default().min(i64::MAX as u64) as i64);
            if self.has_started() {
                self.finish(
                    RequestOutcome::ObservationIncomplete,
                    OutputOutcome::NotEvaluated,
                    now,
                );
            } else {
                self.not_started(NotStartedReason::Unknown, now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ObservationEvent>>);
    impl ObservabilitySeam for Recorder {
        fn submit(&self, event: ObservationEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn context(recorder: Arc<dyn ObservabilitySeam>) -> ObservationContext {
        ObservationContext::new(
            "call_1".into(),
            1_000,
            Attribution {
                provider_id: "local.agent.1".into(),
                model_id: "local.agent.1".into(),
                model_name: "configured-model".into(),
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

    #[test]
    fn complete_snapshot_and_attempt_survive_a_missing_start() {
        let recorder = Arc::new(Recorder::default());
        let mut observation = RequestObservation::new(context(recorder.clone()), 2, 3);
        observation.outbound(1_001);
        observation.headers(200);
        observation.content();
        observation.finish(
            RequestOutcome::Returned,
            OutputOutcome::InvalidProtocol,
            1_005,
        );
        observation.finish(
            RequestOutcome::HttpError,
            OutputOutcome::NotEvaluated,
            1_006,
        );
        let events = recorder.0.lock().unwrap();
        assert_eq!(events.len(), 5);
        let ObservationPayload::Call(call) = &events[4].payload else {
            panic!("call snapshot");
        };
        assert_eq!(call.outcome, RequestOutcome::Returned);
        assert_eq!(call.output, OutputOutcome::InvalidProtocol);
        assert_eq!(events[4].started_at_ms, 1_000);
        assert_eq!(call.advertised_tool_count, 3);
    }

    #[test]
    fn missing_cache_is_not_zero_and_protocol_applicability_is_explicit() {
        let mut report = UsageReport {
            normalized: TokenUsage {
                input_tokens: Some(70),
                output_tokens: Some(20),
                cache_read_tokens: Some(30),
                cache_write_tokens: None,
            },
            ..Default::default()
        };
        assert!(report.basic_complete());
        assert_eq!(report.cache_pair(), Some((30, 100)));
        report.cache_write_applicable = true;
        assert_eq!(report.cache_pair(), None);
        report.normalized.cache_write_tokens = Some(5);
        assert_eq!(report.cache_pair(), Some((30, 105)));
    }

    #[test]
    fn submission_panic_cannot_change_business_flow() {
        struct Broken;
        impl ObservabilitySeam for Broken {
            fn submit(&self, _: ObservationEvent) {
                panic!("unavailable");
            }
        }
        let mut observation = RequestObservation::new(context(Arc::new(Broken)), 0, 0);
        observation.outbound(1_001);
        observation.finish(RequestOutcome::Returned, OutputOutcome::Accepted, 1_002);
    }
}
