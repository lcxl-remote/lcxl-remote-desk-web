//! Agentic-loop session state: the two orthogonal state machines, the persisted
//! session, the authorization scope helpers, and the per-turn / lifetime counts.
//!
//! Two machines move independently (§5):
//! - [`TurnState`] tracks the conversational turn (idle → running → …). Only the
//!   turn machine gates whether a new turn may start.
//! - [`ExecutionState`] tracks an in-flight mutating execution and its
//!   reconciliation. A turn can finish while an execution is still unknown, and a
//!   late result resolves the execution without touching the turn machine.
//!
//! [`PersistedAgentSession`] is the whole thing serialized: the manager stores it
//! in Redis/DB and replays it across instances and restarts, so it is `serde` and
//! carries a `version` for optimistic concurrency. The Direct runtime keeps the
//! same struct in memory. The persistent **subject** (actor / device /
//! policy revision / scope) is validated on every follow-up turn and never
//! rebound on reconnect; the **turn routing** fields (connection / request /
//! turn id) are transient and rebind each turn.

use std::collections::HashSet;

use desk_agent_protocol::AgentScope;
use serde::{Deserialize, Serialize};

use crate::chat::{ChatRole, TokenUsage};
use crate::context_attachment::{
    AttachmentRuntimeBinding, AttachmentStaleReason, AttachmentState, ContextAttachment,
    ContextAttachmentError, ContextAttachmentEvent, revalidate_attachments,
    validate_attachment_set, validate_attachment_subject,
};
use crate::model_context::{ContextNotice, MAX_CONTEXT_NOTICES, ModelContextState};
use crate::replay::{ReplayDisposition, ReplayUnavailableReason};

pub const CONVERSATION_SCHEMA_VERSION: u16 = 1;
/// Opaque replay is bounded independently from visible transcript text.
pub const MAX_REPLAY_ENVELOPE_BYTES: usize = 256 * 1024;
pub const MAX_SESSION_REPLAY_BYTES: usize = 2 * 1024 * 1024;

/// Durable mutation families sharing the same approval, dispatch-generation and
/// completion semantics. This discriminator is persisted with every action
/// identity so a completion can never be routed through an exec-only field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    #[default]
    AgentExec,
    ComputerAction,
    OfficePatch,
    FilePatch,
    CapabilityProvider,
}

impl WorkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentExec => "agent_exec",
            Self::ComputerAction => "computer_action",
            Self::OfficePatch => "office_patch",
            Self::FilePatch => "file_patch",
            Self::CapabilityProvider => "capability_provider",
        }
    }

    pub fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "agent_exec" => Some(Self::AgentExec),
            "computer_action" => Some(Self::ComputerAction),
            "office_patch" => Some(Self::OfficePatch),
            "file_patch" => Some(Self::FilePatch),
            "capability_provider" => Some(Self::CapabilityProvider),
            _ => None,
        }
    }
}

/// Generic identity of one dispatched durable action.
///
/// New JSON always carries `action_request_id` and `work_kind`. For `agent_exec`
/// it additionally writes the legacy `exec_request_id`, allowing an older manager
/// to read a session written during a rolling deployment. New readers accept the
/// old exec-only shape and upgrade it in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionIdentity {
    pub work_id: i64,
    pub action_request_id: String,
    pub execution_id: String,
    pub kind: WorkKind,
}

impl ActionIdentity {
    pub fn new(
        work_id: i64,
        action_request_id: impl Into<String>,
        execution_id: impl Into<String>,
        kind: WorkKind,
    ) -> Self {
        Self {
            work_id,
            action_request_id: action_request_id.into(),
            execution_id: execution_id.into(),
            kind,
        }
    }

    pub fn agent_exec(
        work_id: i64,
        exec_request_id: impl Into<String>,
        execution_id: impl Into<String>,
    ) -> Self {
        Self::new(work_id, exec_request_id, execution_id, WorkKind::AgentExec)
    }
}

#[derive(Serialize)]
struct ActionIdentityRef<'a> {
    work_id: i64,
    action_request_id: &'a str,
    execution_id: &'a str,
    work_kind: WorkKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec_request_id: Option<&'a str>,
}

#[derive(Deserialize)]
struct ActionIdentityOwned {
    work_id: i64,
    #[serde(default)]
    action_request_id: Option<String>,
    execution_id: String,
    #[serde(default)]
    work_kind: WorkKind,
    #[serde(default)]
    exec_request_id: Option<String>,
}

impl Serialize for ActionIdentity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ActionIdentityRef {
            work_id: self.work_id,
            action_request_id: &self.action_request_id,
            execution_id: &self.execution_id,
            work_kind: self.kind,
            exec_request_id: (self.kind == WorkKind::AgentExec)
                .then_some(self.action_request_id.as_str()),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ActionIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ActionIdentityOwned::deserialize(deserializer)?;
        let action_request_id = match (wire.action_request_id, wire.exec_request_id) {
            (Some(action), Some(exec))
                if wire.work_kind == WorkKind::AgentExec && action != exec =>
            {
                return Err(serde::de::Error::custom(
                    "agent_exec action_request_id does not match exec_request_id",
                ));
            }
            (Some(action), _) => action,
            (None, Some(exec)) => exec,
            (None, None) => {
                return Err(serde::de::Error::missing_field("action_request_id"));
            }
        };
        Ok(Self {
            work_id: wire.work_id,
            action_request_id,
            execution_id: wire.execution_id,
            kind: wire.work_kind,
        })
    }
}

#[derive(Debug)]
pub enum SessionDecodeError {
    Json(serde_json::Error),
    UnsupportedVersion(u64),
    InvalidModelContext(String),
    MissingReplayDisposition(String),
    PersistedContextSummary(String),
    InvalidContextAttachment(String),
    InvalidDataEnvelope { message_id: String, error: String },
    InvalidDynamicRun(String),
}

impl std::fmt::Display for SessionDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "invalid agent session JSON: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported conversation schema version {version}")
            }
            Self::InvalidModelContext(error) => {
                write!(f, "invalid model context state: {error}")
            }
            Self::MissingReplayDisposition(message_id) => write!(
                f,
                "assistant tool-call message {message_id} has no replay disposition"
            ),
            Self::PersistedContextSummary(message_id) => write!(
                f,
                "synthetic context summary {message_id} must not be persisted in conversation"
            ),
            Self::InvalidContextAttachment(error) => {
                write!(f, "invalid context attachment: {error}")
            }
            Self::InvalidDataEnvelope { message_id, error } => {
                write!(f, "invalid DataEnvelope on message {message_id}: {error}")
            }
            Self::InvalidDynamicRun(error) => {
                write!(f, "invalid dynamic run state: {error}")
            }
        }
    }
}

impl std::error::Error for SessionDecodeError {}

impl From<serde_json::Error> for SessionDecodeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// The conversational turn machine. A new turn may be claimed only from
/// [`TurnState::Idle`]; `claim_turn` is the only `Idle → Running` transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnState {
    /// No turn running; a new turn may be claimed.
    #[default]
    Idle,
    /// A turn is executing the agent loop.
    Running,
    /// A mutating tool is waiting for user approval (set in the mutating path).
    AwaitingApproval,
    /// The current turn was cancelled by the operator.
    Cancelled,
    /// The current turn ended in an error.
    Failed,
}

impl TurnState {
    /// Whether the turn has reached a settled (terminal) state, so a fresh
    /// follow-up turn may be claimed while reusing the accumulated conversation
    /// history. [`Idle`] (a completed turn), [`Failed`] (an errored turn), and
    /// [`Cancelled`] are all settled.
    ///
    /// [`Idle`]: TurnState::Idle
    /// [`Failed`]: TurnState::Failed
    /// [`Cancelled`]: TurnState::Cancelled
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            TurnState::Idle | TurnState::Failed | TurnState::Cancelled
        )
    }

    /// Whether a turn is in flight ([`Running`] or [`AwaitingApproval`]). An
    /// active turn is **not** directly claimable — it must first be safely
    /// settled (e.g. by lease recovery) before a follow-up turn can begin.
    ///
    /// [`Running`]: TurnState::Running
    /// [`AwaitingApproval`]: TurnState::AwaitingApproval
    pub fn is_active(self) -> bool {
        matches!(self, TurnState::Running | TurnState::AwaitingApproval)
    }

    /// Whether a fresh turn may be claimed from this state. A turn is claimable
    /// from any settled state (so follow-up questions continue the same session);
    /// an active turn ([`is_active`]) is not.
    ///
    /// [`is_active`]: TurnState::is_active
    pub fn can_claim(self) -> bool {
        self.is_settled()
    }
}

/// The execution-reconciliation machine, orthogonal to [`TurnState`].
///
/// Only the mutating path drives the non-`None` variants; a read-only turn leaves
/// this at [`ExecutionState::None`]. The `work_id` / `execution_id` /
/// [`ActionIdentity`] identifies the durable work item, its typed correlation and
/// the immutable dispatch generation used for late-result fencing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExecutionState {
    /// No execution in flight.
    #[default]
    None,
    /// A mutating tool was dispatched and its real result is awaited.
    Executing {
        #[serde(flatten)]
        action: ActionIdentity,
    },
    /// The execution may have run but its outcome is unknown (cancel / timeout /
    /// crash after dispatch). `placeholder_message_id` anchors the placeholder
    /// tool result that keeps the conversation well-formed; a late real result
    /// replaces it in place (CAS by id) rather than appending.
    OutcomeUnknown {
        #[serde(flatten)]
        action: ActionIdentity,
        placeholder_message_id: String,
        since: String,
    },
    /// A turn was interrupted (crash / lease takeover) while a mutating tool was
    /// outstanding, but with **no recoverable execution identity** — the runtime
    /// could not prove whether the command ran or even reached dispatch. Unlike
    /// [`OutcomeUnknown`], no late result will ever reconcile this, so the
    /// conversation is permanently barred from new mutation (read-only follow-up
    /// only). This is the conservative recovery verdict when nothing better is known.
    ///
    /// [`OutcomeUnknown`]: ExecutionState::OutcomeUnknown
    Interrupted { since: String },
}

impl ExecutionState {
    /// Whether a mutating tool may be exposed/started right now. A new mutation is
    /// allowed only from a clean [`None`] state; while an outcome is unknown or the
    /// session was interrupted with no recoverable identity, only read-only
    /// follow-up is allowed.
    ///
    /// [`None`]: ExecutionState::None
    pub fn allows_new_mutation(&self) -> bool {
        matches!(self, ExecutionState::None)
    }

    /// The in-flight task a `wait_for_task` call could wait on, as
    /// `(work_id, execution_id, exec_request_id)`. A dispatched background task
    /// ([`Executing`]) or a recoverable unknown outcome ([`OutcomeUnknown`]) both
    /// have a durable identity whose result may still arrive; an [`Interrupted`]
    /// turn (no recoverable identity) and a clean [`None`] have nothing to wait on.
    ///
    /// [`Executing`]: ExecutionState::Executing
    /// [`OutcomeUnknown`]: ExecutionState::OutcomeUnknown
    /// [`Interrupted`]: ExecutionState::Interrupted
    /// [`None`]: ExecutionState::None
    pub fn waitable_task(&self) -> Option<&ActionIdentity> {
        match self {
            ExecutionState::Executing { action }
            | ExecutionState::OutcomeUnknown { action, .. } => Some(action),
            _ => None,
        }
    }
}

/// What caused a turn to be claimed.
///
/// `User` is a control-end request (a browser / app follow-up). `ExecCompletion`
/// is an automation turn the manager fires by itself after a background command
/// finishes, so the model reacts to the result without a human prompt.
/// `PermissionDecision` resumes the same owner-directed requirement after the
/// owner records a grant decision; it appends no synthetic user message.
///
/// The origin is adopted at the turn boundary (part of the claim) and pins one
/// security-relevant invariant for the whole turn: an automation turn must not be
/// able to start **new** mutations. Otherwise a completion could trigger a turn
/// that dispatches another command whose completion triggers another turn — an
/// unbounded self-driving chain with no human in the loop. `allows_new_mutation`
/// is the gate; the loop both hides mutating tools from an automation turn and
/// refuses one defensively if it is somehow still requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerOrigin {
    /// A control-end (browser / app) request. The default for any turn.
    #[default]
    User,
    /// An owner decision resumes the existing requirement. It may consume a
    /// matching grant, but does not reset the task's automation chain.
    PermissionDecision,
    /// A manager-fired automation turn reacting to a completed background command.
    /// Retained only to deserialize sessions written before generic work origins.
    ExecCompletion,
    /// A manager-fired automation turn reacting to a completed durable action.
    WorkCompletion { kind: WorkKind },
}

/// User-facing surface that owns an agent session.
///
/// `Unknown` exists only as the short-lived default before a newly constructed
/// session is assigned its owning surface; persisted rows must be explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionSurface {
    #[default]
    Unknown,
    TerminalCopilot,
    DeviceAssistant,
}

impl TriggerOrigin {
    /// Whether a turn of this origin may start a **new** mutating command. Only a
    /// `User` turn may; an `ExecCompletion` turn is barred so completions cannot
    /// drive an unbounded self-triggering chain of executions.
    pub fn allows_new_mutation(self) -> bool {
        matches!(
            self,
            TriggerOrigin::User | TriggerOrigin::PermissionDecision
        )
    }
}

/// A completed background command whose result is in the conversation but which
/// the model has not yet reacted to — a candidate for firing an automation turn.
///
/// The set of these on a session is the durable work-list the automation executor
/// sweeps. An entry is added when a result is delivered (only while the automation
/// gate is on) and removed once the model has seen it in a request it then reacted
/// to (see [`PersistedAgentSession::clear_reacted_auto_triggers`]).
///
/// `event_id` is the `message_id` of the completion message in the conversation —
/// the identity the reacted-check matches on and the dedup key. `chain_id` ties
/// the entry to the automation chain that was current when the command was
/// dispatched, so a later user turn that starts a fresh chain can be told this
/// entry is stale. `resolution_org_id` pins the org context for re-evaluating
/// authorization off-connection when the executor claims the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingWorkTrigger {
    pub work_id: i64,
    #[serde(default)]
    pub kind: WorkKind,
    pub execution_id: String,
    pub tool_call_id: String,
    pub event_id: String,
    pub chain_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_org_id: Option<i64>,
    /// When the entry was enqueued (RFC3339 UTC). The earliest `since` across the
    /// set is denormalized into an indexed column so the executor can find sessions
    /// with pending work without scanning every session's JSON.
    pub since: String,
}

/// Source compatibility for callers while the persisted concept migrates from an
/// exec-only automatic trigger to a generic work-completion trigger.
pub type PendingAutoTrigger = PendingWorkTrigger;

/// One agent conversation + session, in the shape the manager persists (and the
/// Direct runtime keeps in memory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedAgentSession {
    pub conversation_id: String,
    /// Validated client continuation intent. The storage key remains the
    /// subject-namespaced `conversation_id`; this value exists so an authorized
    /// history viewer can resume a selected conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_conversation_id: Option<String>,
    /// User-facing surface that created this session.
    #[serde(default)]
    pub surface: AgentSessionSurface,
    /// Version of persisted conversation/replay/context semantics. Fresh
    /// sessions always write version 1; storage adapters explicitly upgrade
    /// legacy version 0 before saving.
    #[serde(default)]
    pub conversation_schema_version: u16,
    pub conversation: Vec<crate::chat::ChatMessage>,
    #[serde(default)]
    pub model_context_state: ModelContextState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_notices: Vec<ContextNotice>,
    /// User-selected context metadata and opaque ContentRefs. Raw file,
    /// terminal, Office, UI and screen bytes never enter session JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_attachments: Vec<ContextAttachment>,

    // ---- Persistent security subject (validated each follow-up; never rebinds) ----
    pub actor_id: String,
    pub device_id: String,
    /// Authorization anchor. A change is detected at turn boundaries (recompute)
    /// and mid-turn (narrow-only intersection); see [`Self::begin_turn`].
    pub policy_revision: i64,
    /// Scope snapshot captured at the start of the current turn — the baseline for
    /// the narrow-only mid-turn intersection (§5.3).
    pub turn_start_scope: AgentScope,
    /// Currently effective scope: at a turn boundary it equals the freshly
    /// computed PDP scope (may expand or narrow vs the last turn); during a turn
    /// it is `turn_start_scope ∩ latest` (narrow only).
    pub scope_snapshot: AgentScope,

    // ---- Transient turn routing (rebinds each turn / on reconnect) ----
    /// What caused the current turn to be claimed. Adopted from the claim at each
    /// turn boundary, so it always reflects the in-progress turn (never a stale
    /// carry-over). Gates whether the turn may start new mutations.
    #[serde(default)]
    pub trigger_origin: TriggerOrigin,
    /// The automation chain the current turn belongs to: the id of the user turn
    /// that began it. Reset to the claiming turn's id on a `User` claim; preserved
    /// on an `ExecCompletion` claim, so one chain keeps one id. A pending entry
    /// whose `chain_id` no longer equals this has been superseded by a newer user
    /// turn and is stale.
    #[serde(default)]
    pub chain_id: String,
    /// Automation turns already spent on the current chain, reset with the chain on
    /// a `User` claim. Bounds a self-driving chain together with the configured
    /// per-chain cap.
    #[serde(default)]
    pub automation_turns_used: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_control_connection_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_id: Option<String>,

    // ---- The two orthogonal machines ----
    pub turn_state: TurnState,
    pub execution_state: ExecutionState,

    /// Completed background results the model has not yet reacted to — the
    /// automation executor's durable work-list. Empty unless the automation gate
    /// added an entry on delivery; drained as the model reacts, so a session with
    /// automation off never accumulates any. See [`PendingAutoTrigger`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_auto_triggers: Vec<PendingAutoTrigger>,

    // ---- Dynamic run input ledger projection (Stage 2) ----
    /// Latest durably accepted user-input sequence for this run.
    #[serde(default)]
    pub latest_input_seq: u64,
    /// Revision fence advanced whenever a user follow-up is durably committed.
    #[serde(default)]
    pub input_revision: u64,
    /// Highest user-input sequence included in a successfully persisted model
    /// result. It may never exceed `latest_input_seq`.
    #[serde(default)]
    pub handled_input_seq: u64,
    /// Highest append-only run event sequence allocated for this run.
    #[serde(default)]
    pub last_event_seq: u64,
    /// Model-maintained, user-correctable UX projection. This is intentionally
    /// absent from authorization and execution-state decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_status_projection: Option<crate::dynamic_run::TaskStatusProjection>,
    /// Bounded user-facing permission requests proposed by the model. These are
    /// not grants and are never consulted by tool dispatch. A newer user input
    /// moves unresolved requests to NeedsRevalidation before it is committed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permission_requests: Vec<crate::dynamic_run::PermissionRequest>,

    /// Lease fencing token. Rotated on every claim (and on lease takeover during
    /// recovery), it identifies the *current* turn owner. A [`SessionSeam::save`]
    /// CAS matches on this token **and** the `version`: the version blocks a stale
    /// snapshot from the same owner, while the token blocks a *revived* old owner
    /// whose lease already expired and was taken over — its save fails because the
    /// token has since rotated, so it can never overwrite the new owner's work.
    ///
    /// [`SessionSeam::save`]: crate::seam::SessionSeam::save
    #[serde(default)]
    pub lease_token: u64,

    // ---- Counting: turn-level (circuit breaker) + lifetime (audit / budget) ----
    pub current_turn_steps: u32,
    pub current_turn_tokens: TokenUsage,
    pub lifetime_steps: u32,
    pub lifetime_tokens: TokenUsage,

    pub created_at: String,
    pub updated_at: String,
    /// Optimistic-concurrency version (DB CAS on the manager).
    pub version: i64,
}

/// Why a turn could not be claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnClaimError {
    /// A turn is already running (turn state is not `Idle`).
    Busy,
}

/// Why a follow-up turn was refused at the subject check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectMismatch {
    Actor,
    Device,
    Surface,
    /// A surface restricted to the personal device owner was claimed from an
    /// organization context or by a non-owner actor.
    PersonalContext,
}

impl PersistedAgentSession {
    /// Replace oversized/old opaque replay payloads with fail-closed tombstones.
    /// Visible transcript fields and tool grouping are never removed.
    pub fn enforce_replay_storage_limits(&mut self) {
        let mut total = 0usize;
        for message in &mut self.conversation {
            let Some(ReplayDisposition::Present { envelope }) = message.replay_disposition.as_ref()
            else {
                continue;
            };
            let cost = envelope.encoded_cost();
            if cost > MAX_REPLAY_ENVELOPE_BYTES {
                let source_context_key = envelope.source_context_key.clone();
                message.replay_disposition = Some(ReplayDisposition::Unavailable {
                    source_context_key: Some(source_context_key),
                    reason: ReplayUnavailableReason::EvictedByStorageLimit,
                });
            } else {
                total = total.saturating_add(cost);
            }
        }
        if total <= MAX_SESSION_REPLAY_BYTES {
            return;
        }
        for message in &mut self.conversation {
            let Some(ReplayDisposition::Present { envelope }) = message.replay_disposition.as_ref()
            else {
                continue;
            };
            let cost = envelope.encoded_cost();
            let source_context_key = envelope.source_context_key.clone();
            message.replay_disposition = Some(ReplayDisposition::Unavailable {
                source_context_key: Some(source_context_key),
                reason: ReplayUnavailableReason::EvictedByStorageLimit,
            });
            total = total.saturating_sub(cost);
            if total <= MAX_SESSION_REPLAY_BYTES {
                break;
            }
        }
    }

    /// Build a non-destructive durable projection. Provider image data is kept
    /// in the live in-memory turn for the next model call, but never enters the
    /// database/SQLite JSON at an intermediate save or crash boundary.
    pub fn encode_json_for_storage(&self) -> Result<String, serde_json::Error> {
        let mut projection = self.clone();
        projection.enforce_replay_storage_limits();
        crate::image_input::strip_session_images(&mut projection.conversation);
        serde_json::to_string(&projection)
    }

    /// Decode the persisted envelope with an explicit legacy-v0 upgrade. Unknown
    /// versions fail closed instead of being partially interpreted by serde.
    pub fn decode_json(input: &str) -> Result<Self, SessionDecodeError> {
        let value: serde_json::Value = serde_json::from_str(input)?;
        let version = value
            .get("conversation_schema_version")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if version > u64::from(CONVERSATION_SCHEMA_VERSION) {
            return Err(SessionDecodeError::UnsupportedVersion(version));
        }
        let mut session: Self = serde_json::from_value(value)?;
        validate_attachment_set(&session.context_attachments)
            .map_err(|error| SessionDecodeError::InvalidContextAttachment(error.to_string()))?;
        for attachment in &session.context_attachments {
            validate_attachment_subject(
                attachment,
                &session.actor_id,
                &session.device_id,
                session.surface,
            )
            .map_err(|error| SessionDecodeError::InvalidContextAttachment(error.to_string()))?;
        }
        if let Some(message) = session
            .conversation
            .iter()
            .find(|message| message.role == ChatRole::ContextSummary)
        {
            return Err(SessionDecodeError::PersistedContextSummary(
                message.message_id.clone(),
            ));
        }
        for message in &session.conversation {
            if let Some(envelope) = &message.data_envelope {
                envelope
                    .validate()
                    .map_err(|error| SessionDecodeError::InvalidDataEnvelope {
                        message_id: message.message_id.clone(),
                        error: error.to_string(),
                    })?;
            }
        }
        if session.handled_input_seq > session.latest_input_seq {
            return Err(SessionDecodeError::InvalidDynamicRun(
                "handled_input_seq exceeds latest_input_seq".into(),
            ));
        }
        if session.input_revision < session.latest_input_seq {
            return Err(SessionDecodeError::InvalidDynamicRun(
                "input_revision is behind latest_input_seq".into(),
            ));
        }
        if let Some(projection) = &session.task_status_projection {
            projection
                .validate()
                .map_err(|error| SessionDecodeError::InvalidDynamicRun(error.to_string()))?;
        }
        if session.permission_requests.len() > crate::dynamic_run::MAX_PERMISSION_REQUESTS {
            return Err(SessionDecodeError::InvalidDynamicRun(
                "too many permission requests".into(),
            ));
        }
        let mut permission_ids = std::collections::BTreeSet::new();
        for request in &session.permission_requests {
            request
                .validate()
                .map_err(|error| SessionDecodeError::InvalidDynamicRun(error.to_string()))?;
            if request.input_revision > session.input_revision {
                return Err(SessionDecodeError::InvalidDynamicRun(
                    "permission request is ahead of input revision".into(),
                ));
            }
            if !permission_ids.insert(request.request_id.as_str()) {
                return Err(SessionDecodeError::InvalidDynamicRun(
                    "duplicate permission request id".into(),
                ));
            }
        }
        if version == 0 {
            session.conversation_schema_version = CONVERSATION_SCHEMA_VERSION;
            session.model_context_state = ModelContextState::default();
            session.context_notices.clear();
            for message in &mut session.conversation {
                if message.role == crate::chat::ChatRole::Assistant
                    && !message.tool_calls.is_empty()
                    && message.replay_disposition.is_none()
                {
                    message.replay_disposition = Some(ReplayDisposition::legacy_unknown());
                }
            }
            return Ok(session);
        }
        session
            .model_context_state
            .upgrade_from_v1()
            .map_err(|error| SessionDecodeError::InvalidModelContext(error.to_string()))?;
        for message in &session.conversation {
            if message.role == crate::chat::ChatRole::Assistant
                && !message.tool_calls.is_empty()
                && message.replay_disposition.is_none()
            {
                return Err(SessionDecodeError::MissingReplayDisposition(
                    message.message_id.clone(),
                ));
            }
        }
        Ok(session)
    }

    /// Start a brand-new session bound to a subject, with a turn-boundary scope.
    pub fn new(
        conversation_id: impl Into<String>,
        actor_id: impl Into<String>,
        device_id: impl Into<String>,
        policy_revision: i64,
        scope: AgentScope,
        now: impl Into<String> + Clone,
    ) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            client_conversation_id: None,
            surface: AgentSessionSurface::Unknown,
            conversation_schema_version: CONVERSATION_SCHEMA_VERSION,
            conversation: Vec::new(),
            model_context_state: ModelContextState::default(),
            context_notices: Vec::new(),
            context_attachments: Vec::new(),
            actor_id: actor_id.into(),
            device_id: device_id.into(),
            policy_revision,
            turn_start_scope: scope.clone(),
            scope_snapshot: scope,
            trigger_origin: TriggerOrigin::User,
            chain_id: String::new(),
            automation_turns_used: 0,
            active_control_connection_id: None,
            current_request_id: None,
            current_turn_id: None,
            turn_state: TurnState::Idle,
            execution_state: ExecutionState::None,
            pending_auto_triggers: Vec::new(),
            latest_input_seq: 0,
            input_revision: 0,
            handled_input_seq: 0,
            last_event_seq: 0,
            task_status_projection: None,
            permission_requests: Vec::new(),
            lease_token: 0,
            current_turn_steps: 0,
            current_turn_tokens: TokenUsage::default(),
            lifetime_steps: 0,
            lifetime_tokens: TokenUsage::default(),
            created_at: now.clone().into(),
            updated_at: now.into(),
            version: 0,
        }
    }

    /// Attach validated client-facing metadata to a new session, or upgrade a
    /// legacy row when it is next claimed from a known surface.
    pub fn adopt_client_metadata(
        &mut self,
        client_conversation_id: Option<&str>,
        surface: AgentSessionSurface,
    ) {
        if self.client_conversation_id.is_none() {
            self.client_conversation_id = client_conversation_id.map(str::to_string);
        }
        if self.surface == AgentSessionSurface::Unknown {
            self.surface = surface;
        }
    }

    /// Fence unresolved permission UI against a newly accepted user revision.
    /// This mutates only request state; it never creates, revokes, or matches a
    /// grant. The durable append transaction persists it together with the new
    /// user message so a stale panel can never remain approvable after ACK.
    pub fn require_permission_revalidation(&mut self, next_input_revision: u64) -> usize {
        self.permission_requests
            .iter_mut()
            .map(|request| usize::from(request.require_revalidation(next_input_revision)))
            .sum()
    }

    pub fn add_permission_request(
        &mut self,
        request: crate::dynamic_run::PermissionRequest,
    ) -> Result<(), crate::dynamic_run::DynamicRunContractError> {
        request.validate()?;
        if request.input_revision != self.input_revision {
            return Err(crate::dynamic_run::DynamicRunContractError::InvalidRevision);
        }
        if self
            .permission_requests
            .iter()
            .any(|existing| existing.request_id == request.request_id)
        {
            return Err(crate::dynamic_run::DynamicRunContractError::PermissionEventMismatch);
        }
        if self.permission_requests.len() >= crate::dynamic_run::MAX_PERMISSION_REQUESTS {
            return Err(crate::dynamic_run::DynamicRunContractError::InvalidPermissionItemCount);
        }
        self.permission_requests.push(request);
        Ok(())
    }

    /// Add a deterministic per-turn context notice without exposing omitted
    /// content. The bounded list is transcript metadata, not model history.
    pub fn add_context_notice(&mut self, notice: ContextNotice) -> bool {
        if self.context_notices.iter().any(|item| item.id == notice.id) {
            return false;
        }
        self.context_notices.push(notice);
        if self.context_notices.len() > MAX_CONTEXT_NOTICES {
            let excess = self.context_notices.len() - MAX_CONTEXT_NOTICES;
            self.context_notices.drain(..excess);
        }
        true
    }

    /// Idempotently attach immutable context metadata. Reusing a client request
    /// id with different bytes is rejected; refreshing must use a new request and
    /// attachment identity.
    pub fn attach_context(
        &mut self,
        attachment: ContextAttachment,
    ) -> Result<bool, ContextAttachmentError> {
        validate_attachment_subject(&attachment, &self.actor_id, &self.device_id, self.surface)?;
        if let Some(existing) = self
            .context_attachments
            .iter()
            .find(|existing| existing.client_request_id == attachment.client_request_id)
        {
            if existing == &attachment {
                return Ok(false);
            }
            return Err(ContextAttachmentError::DuplicateClientRequestId);
        }
        let mut projected = self.context_attachments.clone();
        projected.push(attachment.clone());
        validate_attachment_set(&projected)?;
        self.context_attachments.push(attachment);
        Ok(true)
    }

    pub fn detach_context(&mut self, attachment_id: &str) -> bool {
        let Some(attachment) = self
            .context_attachments
            .iter_mut()
            .find(|attachment| attachment.attachment_id == attachment_id)
        else {
            return false;
        };
        if matches!(
            attachment.state,
            AttachmentState::Stale {
                reason: AttachmentStaleReason::Detached
            }
        ) {
            return false;
        }
        attachment.mark_stale(AttachmentStaleReason::Detached);
        true
    }

    /// Atomically stale one immutable live ref and attach its replacement. A
    /// refresh never mutates/rebinds the old opaque token in place.
    pub fn refresh_context(
        &mut self,
        stale_attachment_id: &str,
        reason: AttachmentStaleReason,
        replacement: ContextAttachment,
    ) -> Result<bool, ContextAttachmentError> {
        validate_attachment_subject(&replacement, &self.actor_id, &self.device_id, self.surface)?;
        let Some(stale_index) = self
            .context_attachments
            .iter()
            .position(|attachment| attachment.attachment_id == stale_attachment_id)
        else {
            return Err(ContextAttachmentError::AttachmentNotFound);
        };
        let stale = &self.context_attachments[stale_index];
        if replacement.attachment_id == stale.attachment_id
            || replacement.client_request_id == stale.client_request_id
        {
            return Err(ContextAttachmentError::RefreshIdentityReused);
        }
        if let Some(existing) = self
            .context_attachments
            .iter()
            .find(|attachment| attachment.client_request_id == replacement.client_request_id)
        {
            if existing == &replacement
                && matches!(
                    self.context_attachments[stale_index].state,
                    crate::context_attachment::AttachmentState::Stale { .. }
                )
            {
                return Ok(false);
            }
            return Err(ContextAttachmentError::DuplicateClientRequestId);
        }
        if self
            .context_attachments
            .iter()
            .any(|attachment| attachment.attachment_id == replacement.attachment_id)
        {
            return Err(ContextAttachmentError::DuplicateAttachmentId);
        }

        let mut projected = self.context_attachments.clone();
        projected[stale_index].mark_stale(reason);
        projected.push(replacement);
        validate_attachment_set(&projected)?;
        self.context_attachments = projected;
        Ok(true)
    }

    pub fn active_context(&self, now_unix_ms: u64) -> Vec<&ContextAttachment> {
        self.context_attachments
            .iter()
            .filter(|attachment| attachment.is_active_at(now_unix_ms))
            .collect()
    }

    pub fn revalidate_context(
        &mut self,
        now_unix_ms: u64,
        bindings: &[AttachmentRuntimeBinding],
    ) -> Vec<ContextAttachmentEvent> {
        revalidate_attachments(&mut self.context_attachments, now_unix_ms, bindings)
    }

    pub fn mark_context_stale(&mut self, reason: AttachmentStaleReason) {
        for attachment in &mut self.context_attachments {
            if matches!(
                attachment.state,
                crate::context_attachment::AttachmentState::Active
            ) {
                attachment.mark_stale(reason);
            }
        }
    }

    /// Validate that a follow-up turn comes from the same subject. The connection
    /// id is **not** part of the subject (a reconnect must be able to continue).
    pub fn check_subject(&self, actor_id: &str, device_id: &str) -> Result<(), SubjectMismatch> {
        if self.actor_id != actor_id {
            return Err(SubjectMismatch::Actor);
        }
        if self.device_id != device_id {
            return Err(SubjectMismatch::Device);
        }
        Ok(())
    }

    /// Enforce conversation-domain separation. Persisted sessions with no
    /// explicit surface fail closed; this system has no legacy adoption path.
    pub fn check_surface(&self, requested: AgentSessionSurface) -> Result<(), SubjectMismatch> {
        if self.surface != requested {
            return Err(SubjectMismatch::Surface);
        }
        Ok(())
    }

    /// Claim the conversation for a new turn (a `settled → Running` transition).
    /// A turn may be claimed from any settled state ([`TurnState::can_claim`]), so
    /// a follow-up question continues the same conversation even after the prior
    /// turn finished or failed; the accumulated `conversation` history is left
    /// untouched. `execution_state` is also left as-is, so an unresolved
    /// `OutcomeUnknown` keeps the next turn read-only. On success: bind the turn
    /// routing, recompute the scope at the turn boundary (`current_pdp` may expand
    /// or narrow vs the last turn), and reset the turn-level counters. The caller's
    /// [`SessionSeam`] is responsible for doing this atomically (DB CAS / in-memory
    /// lock).
    ///
    /// [`SessionSeam`]: crate::seam::SessionSeam
    pub fn begin_turn(
        &mut self,
        turn_id: impl Into<String>,
        request_id: Option<String>,
        connection_id: Option<String>,
        policy_revision: i64,
        current_pdp_scope: AgentScope,
        now: impl Into<String>,
    ) -> Result<(), TurnClaimError> {
        if !self.turn_state.can_claim() {
            return Err(TurnClaimError::Busy);
        }
        // Rotate the fencing token: this claim becomes the sole owner, and any
        // prior owner whose lease was taken over now holds a stale token.
        self.lease_token = self.lease_token.wrapping_add(1);
        self.turn_state = TurnState::Running;
        self.current_turn_id = Some(turn_id.into());
        self.current_request_id = request_id;
        self.active_control_connection_id = connection_id;
        // Turn boundary: adopt the freshly computed PDP scope (expand or narrow).
        self.policy_revision = policy_revision;
        self.turn_start_scope = current_pdp_scope.clone();
        self.scope_snapshot = current_pdp_scope;
        // Reset the turn-level circuit-breaker counters.
        self.current_turn_steps = 0;
        self.current_turn_tokens = TokenUsage::default();
        self.updated_at = now.into();
        Ok(())
    }

    /// Apply a mid-turn policy change: narrow the effective scope to
    /// `turn_start_scope ∩ latest` (a running turn may only lose authority, never
    /// gain it). Updates `policy_revision` to the observed latest.
    pub fn narrow_for_revision(&mut self, latest_revision: i64, latest_scope: &AgentScope) {
        self.scope_snapshot = narrow_scope(&self.turn_start_scope, latest_scope);
        self.policy_revision = latest_revision;
    }

    /// Record one model→tool step and its token usage against both the turn-level
    /// counters (circuit breaker) and the lifetime counters (audit / budget).
    pub fn record_step(&mut self, usage: TokenUsage) {
        self.current_turn_steps = self.current_turn_steps.saturating_add(1);
        self.lifetime_steps = self.lifetime_steps.saturating_add(1);
        add_usage(&mut self.current_turn_tokens, usage);
        add_usage(&mut self.lifetime_tokens, usage);
    }

    /// Record a provider-backed context-compression call without consuming one
    /// model→tool iteration. Compression usage is still part of the authoritative
    /// turn/lifetime token totals and billing surfaces.
    pub fn record_compression_usage(&mut self, usage: TokenUsage) {
        add_usage(&mut self.current_turn_tokens, usage);
        add_usage(&mut self.lifetime_tokens, usage);
    }

    /// Build the authoritative raw-history protection snapshot used by context
    /// planning. A checkpoint source id is never a substitute for one of these
    /// live message/tool ids.
    pub fn context_protection_set(&self) -> crate::model_context::ContextProtectionSet {
        let mut protection = crate::model_context::ContextProtectionSet {
            current_turn_id: self.current_turn_id.clone(),
            ..crate::model_context::ContextProtectionSet::default()
        };
        for pending in &self.pending_auto_triggers {
            protection.protect_message(pending.event_id.clone());
            protection.protect_tool_call(pending.tool_call_id.clone());
        }
        if let ExecutionState::OutcomeUnknown {
            placeholder_message_id,
            ..
        } = &self.execution_state
        {
            protection.protect_message(placeholder_message_id.clone());
        }
        if let Some(action) = self.execution_state.waitable_task() {
            for message in &self.conversation {
                if message.background_task_id.as_deref() == Some(action.action_request_id.as_str())
                {
                    protection.protect_message(message.message_id.clone());
                    if let Some(tool_call_id) = &message.tool_call_id {
                        protection.protect_tool_call(tool_call_id.clone());
                    }
                }
            }
        }
        protection
    }

    /// Whether the per-turn step budget is exhausted (circuit breaker). The bound
    /// is supplied by the caller (via `LoopDeps`) so different runtimes can tune
    /// it — diagnose uses [`crate::MAX_STEPS_PER_TURN`], the latency-sensitive
    /// terminal copilot uses a tighter bound.
    pub fn turn_step_budget_exhausted(&self, max_steps_per_turn: u32) -> bool {
        self.current_turn_steps >= max_steps_per_turn
    }

    /// Settle the turn machine at the end of a turn. Only the turn machine is
    /// touched — an in-flight [`ExecutionState`] is left for its own
    /// reconciliation. `now` updates the modified timestamp.
    ///
    /// Deliberately does **not** touch [`Self::pending_auto_triggers`]: a turn that
    /// ended without reacting (a circuit break, a truncated turn) must leave its
    /// pending entries queued. Draining happens only where the model actually
    /// reacts — see [`Self::clear_reacted_auto_triggers`].
    pub fn finish_turn(&mut self, terminal: TurnState, now: impl Into<String>) {
        self.turn_state = terminal;
        self.updated_at = now.into();
    }

    /// Adopt the claim's trigger origin at the turn boundary, called by every claim
    /// path right after [`begin_turn`](Self::begin_turn). A `User` claim starts a
    /// fresh automation chain — its id becomes `turn_id` and the per-chain budget
    /// resets — while an `ExecCompletion` claim continues the existing chain (so a
    /// self-driving chain keeps one id) and **spends** one of its automation-turn
    /// budget.
    ///
    /// The spend happens here, inside the claim, so it is atomic with the
    /// `Idle → Running` CAS: two instances racing to fire the same chain's next
    /// automation turn both attempt the claim, but only one's CAS lands, so the
    /// budget advances exactly once per turn actually started. Counting at claim
    /// (not on success) is deliberately fail-safe — a crashed automation turn still
    /// consumes budget, so a turn that dies mid-run cannot be retried without bound.
    pub fn adopt_trigger(&mut self, origin: TriggerOrigin, turn_id: &str) {
        self.trigger_origin = origin;
        match origin {
            TriggerOrigin::User => {
                self.chain_id = turn_id.to_string();
                self.automation_turns_used = 0;
            }
            TriggerOrigin::PermissionDecision => {}
            TriggerOrigin::ExecCompletion | TriggerOrigin::WorkCompletion { .. } => {
                self.automation_turns_used = self.automation_turns_used.saturating_add(1);
            }
        }
    }

    /// Enqueue a completed result as a pending auto-trigger, deduped by `event_id`.
    /// Returns whether it was newly added (`false` if an entry with the same
    /// `event_id` is already queued). Pure: the caller persists under its own CAS
    /// and syncs the denormalized index column from [`Self::earliest_pending_at`].
    pub fn add_pending_auto_trigger(&mut self, trigger: PendingAutoTrigger) -> bool {
        if self
            .pending_auto_triggers
            .iter()
            .any(|p| p.event_id == trigger.event_id)
        {
            return false;
        }
        self.pending_auto_triggers.push(trigger);
        true
    }

    /// Drop every pending auto-trigger the model has now seen and reacted to — one
    /// whose `event_id` (the completion message's `message_id`) is among the
    /// `message_id`s of the request the model just answered. Returns how many were
    /// dropped.
    ///
    /// Called only where the model actually reacts (the assistant answer /
    /// tool-call save), never on the generic turn-settle path — otherwise a turn
    /// that circuit-broke before reacting (e.g. a one-step budget) would wrongly
    /// drop a completion the model never saw. Pure.
    pub fn clear_reacted_auto_triggers(&mut self, request_message_ids: &HashSet<String>) -> usize {
        let before = self.pending_auto_triggers.len();
        self.pending_auto_triggers
            .retain(|p| !request_message_ids.contains(&p.event_id));
        before - self.pending_auto_triggers.len()
    }

    /// Remove one pending auto-trigger by `event_id` (the executor settled or
    /// skipped it). Returns whether one was removed. Pure.
    pub fn remove_pending_auto_trigger(&mut self, event_id: &str) -> bool {
        let before = self.pending_auto_triggers.len();
        self.pending_auto_triggers
            .retain(|p| p.event_id != event_id);
        before != self.pending_auto_triggers.len()
    }

    /// The earliest `since` across the pending set, or `None` when empty — the
    /// value denormalized into the indexed `pending_auto_trigger_at` column so the
    /// executor can find sessions with pending work without scanning every
    /// session's JSON. Timestamps are RFC3339 UTC, so the lexical minimum is the
    /// chronological minimum.
    pub fn earliest_pending_at(&self) -> Option<&str> {
        self.pending_auto_triggers
            .iter()
            .map(|p| p.since.as_str())
            .min()
    }

    /// Reconcile a late execution result against an unknown outcome (§6.2): if the
    /// execution machine is [`ExecutionState::OutcomeUnknown`] for `execution_id`,
    /// replace the placeholder tool-result message (matched by its `message_id`)
    /// text **in place** — never appending a second tool result for the same call —
    /// and clear the execution machine to [`ExecutionState::None`]. Returns whether
    /// a reconciliation happened (false if already resolved / a different
    /// execution / not unknown). The durable result is written first by the caller;
    /// this only mutates the conversation + execution state.
    pub fn reconcile_late_result(
        &mut self,
        execution_id: &str,
        result_text: impl Into<String>,
        now: impl Into<String>,
    ) -> bool {
        let placeholder_id = match &self.execution_state {
            ExecutionState::OutcomeUnknown {
                action,
                placeholder_message_id,
                ..
            } if action.execution_id == execution_id => placeholder_message_id.clone(),
            _ => return false,
        };
        if let Some(msg) = self
            .conversation
            .iter_mut()
            .find(|m| m.message_id == placeholder_id)
        {
            msg.text = result_text.into();
        }
        self.execution_state = ExecutionState::None;
        self.updated_at = now.into();
        true
    }

    /// Transition an outstanding [`ExecutionState::Executing`] to
    /// [`ExecutionState::OutcomeUnknown`] when a dispatched work item was recovered
    /// with no result (its owner crashed before finalizing and the host is not known
    /// to have completed it). Without this, a session left `Executing` by a
    /// background dispatch whose owner dies would block new mutation forever, because
    /// nothing else transitions it. Repurpose the running-task tool result (closing
    /// `tool_call_id`, written by the `Dispatched` arm) as the placeholder a late
    /// real result can still replace in place via [`reconcile_late_result`] /
    /// [`apply_completion`], rewriting its text to the unknown-outcome placeholder.
    ///
    /// Only acts when the machine is `Executing` for this `execution_id` **and** the
    /// running-task result closing `tool_call_id` is present (needed as the reconcile
    /// anchor); returns whether the transition happened. Pure: the caller persists
    /// under its own CAS.
    ///
    /// [`reconcile_late_result`]: Self::reconcile_late_result
    /// [`apply_completion`]: Self::apply_completion
    pub fn mark_execution_unknown(
        &mut self,
        execution_id: &str,
        tool_call_id: &str,
        now: impl Into<String>,
    ) -> bool {
        let action = match &self.execution_state {
            ExecutionState::Executing { action } if action.execution_id == execution_id => {
                action.clone()
            }
            _ => return false,
        };
        // Anchor on the running-task tool result closing this call. Absent it there
        // is nothing to reconcile a late result against, so leave the state as-is.
        let placeholder_id = match self
            .conversation
            .iter_mut()
            .find(|m| m.tool_call_id.as_deref() == Some(tool_call_id))
        {
            Some(msg) => {
                msg.text = RECOVER_OUTCOME_UNKNOWN.to_string();
                msg.message_id.clone()
            }
            None => return false,
        };
        let now = now.into();
        self.execution_state = ExecutionState::OutcomeUnknown {
            action,
            placeholder_message_id: placeholder_id,
            since: now.clone(),
        };
        self.updated_at = now;
        true
    }

    /// Append a mid-conversation system-event notification identified by `event_id`,
    /// idempotently. The appended [`ChatRole::SystemEvent`] message's `message_id`
    /// **is** the `event_id`, so a redelivery of the same logical event — the
    /// durable completion queue is allowed to deliver more than once and requires
    /// the sink to dedupe — finds it already present and is a no-op.
    ///
    /// Returns whether the message was newly appended (`true`) or already present
    /// (`false`). Pure: the caller persists the mutated session under its own CAS.
    ///
    /// [`ChatRole::SystemEvent`]: crate::chat::ChatRole::SystemEvent
    pub fn append_event_if_absent(
        &mut self,
        event_id: &str,
        text: impl Into<String>,
        now: impl Into<String>,
    ) -> bool {
        if self.conversation.iter().any(|m| m.message_id == event_id) {
            return false;
        }
        self.conversation
            .push(crate::chat::ChatMessage::system_event(event_id, text));
        self.updated_at = now.into();
        true
    }

    /// Deliver a completed background execution's result into the conversation,
    /// idempotently keyed by `event_id`. This is the durable completion queue's sink,
    /// covering every way the result can relate to the conversation:
    ///
    /// - **Already delivered** — a message keyed by `event_id` is present (a
    ///   foreground result the loop keyed on the delivery id, or a prior delivery of
    ///   this same event): a no-op (`false`).
    /// - **Recovered unknown outcome** — the execution machine is
    ///   [`ExecutionState::OutcomeUnknown`] for this `execution_id`: replace the
    ///   placeholder tool result in place and re-key it to `event_id` so a later
    ///   redelivery is recognized as already delivered.
    /// - **Open tool call** — the call is still unanswered (a foreground save was
    ///   lost before it closed the call): close it with the real result, keyed by
    ///   `event_id`, keeping the model history well-formed.
    /// - **Closed tool call** — the call already has a result (a background dispatch
    ///   closed it with a running-task placeholder): append the completion as a
    ///   [`ChatRole::UntrustedOutput`] message keyed by `event_id`. The bytes are
    ///   device output, so they are fenced as untrusted data rather than injected
    ///   with `system` authority.
    ///
    /// In the last three cases an outstanding [`ExecutionState::Executing`] for this
    /// execution is cleared so a follow-up may mutate again. Returns whether the
    /// session was mutated. Pure: the caller persists under its own CAS.
    ///
    /// [`ChatRole::UntrustedOutput`]: crate::chat::ChatRole::UntrustedOutput
    pub fn apply_completion(
        &mut self,
        event_id: &str,
        execution_id: &str,
        tool_call_id: &str,
        background_task_id: &str,
        result_text: impl Into<String>,
        now: impl Into<String>,
    ) -> bool {
        self.apply_completion_with_envelope(
            event_id,
            execution_id,
            tool_call_id,
            background_task_id,
            result_text,
            None,
            now,
        )
    }

    /// Envelope-aware form used by generic Provider completions. The exact
    /// persisted bytes and their lineage remain attached when the completion is
    /// replayed into a later model turn.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_completion_with_envelope(
        &mut self,
        event_id: &str,
        execution_id: &str,
        tool_call_id: &str,
        background_task_id: &str,
        result_text: impl Into<String>,
        result_envelope: Option<desk_agent_protocol::data_lineage::DataEnvelope>,
        now: impl Into<String>,
    ) -> bool {
        // Idempotent: a message keyed by this event is already in the conversation.
        if self.conversation.iter().any(|m| m.message_id == event_id) {
            return false;
        }
        let result_text = result_text.into();
        let now = now.into();

        // A recovered unknown outcome for this execution: replace the placeholder in
        // place and re-key it to the event id so a redelivery dedups on it above.
        if let ExecutionState::OutcomeUnknown {
            action,
            placeholder_message_id,
            ..
        } = &self.execution_state
            && action.execution_id == execution_id
        {
            let placeholder_id = placeholder_message_id.clone();
            if let Some(msg) = self
                .conversation
                .iter_mut()
                .find(|m| m.message_id == placeholder_id)
            {
                msg.text = result_text;
                msg.message_id = event_id.to_string();
                msg.background_task_id = Some(background_task_id.to_string());
                msg.data_envelope = result_envelope;
            }
            self.execution_state = ExecutionState::None;
            self.updated_at = now;
            return true;
        }

        // Otherwise close the open call with the real result, or — if it is already
        // closed — append the completion as fenced untrusted output (device bytes,
        // never a `system` message); both keyed by event id.
        let open = unclosed_tool_call_ids(&self.conversation)
            .iter()
            .any(|id| id == tool_call_id);
        if open {
            let mut message =
                crate::chat::ChatMessage::tool_result(event_id, tool_call_id, result_text);
            message.background_task_id = Some(background_task_id.to_string());
            message.data_envelope = result_envelope;
            self.conversation.push(message);
        } else {
            let mut message = crate::chat::ChatMessage::untrusted_output(
                event_id,
                tool_call_id,
                background_task_id,
                result_text,
            );
            message.data_envelope = result_envelope;
            self.conversation.push(message);
        }
        if matches!(
            &self.execution_state,
            ExecutionState::Executing { action } if action.execution_id == execution_id
        ) {
            self.execution_state = ExecutionState::None;
        }
        self.updated_at = now;
        true
    }

    /// The tool-call ids the conversation left unanswered — an assistant tool call
    /// with no matching tool result, left dangling when a turn was interrupted
    /// mid-execution. A recovering [`SessionSeam`] reads these to correlate the
    /// in-flight call with its durable work item before deciding the recovery
    /// [`verdict`]. First-seen order, de-duplicated.
    ///
    /// [`verdict`]: RecoveryVerdict
    /// [`SessionSeam`]: crate::seam::SessionSeam
    pub fn unclosed_tool_call_ids(&self) -> Vec<String> {
        unclosed_tool_call_ids(&self.conversation)
    }

    /// Recover an orphaned **active** session (its lease expired and was taken over)
    /// into a well-formed, settled state, per an explicitly supplied [`verdict`].
    ///
    /// A crash mid-turn can leave the conversation malformed: an assistant
    /// tool-call message with no matching tool result (the loop never returned from
    /// `confirm_and_exec`). Recovery closes every such unclosed tool call with a
    /// placeholder result so the model history replays cleanly, settles the turn to
    /// [`TurnState::Failed`] so a read-only follow-up can be claimed, and sets the
    /// execution machine according to the verdict:
    ///
    /// - [`NotExecuted`] — the command provably never ran: a plain "not executed"
    ///   result; execution machine cleared to [`None`] (a later turn may mutate).
    /// - [`OutcomeUnknown`] — dispatched with a recoverable identity: a placeholder
    ///   a late real result can replace in place ([`reconcile_late_result`]); only
    ///   read-only follow-up until then.
    /// - [`InterruptedUnknown`] — no recoverable identity: closed as interrupted and
    ///   the conversation is permanently barred from new mutation, without
    ///   fabricating an unreconcilable identity.
    ///
    /// The caller (a [`SessionSeam`]) decides the verdict and performs this inside
    /// the same atomic claim that rotates the lease token; recovery itself is pure.
    ///
    /// [`verdict`]: RecoveryVerdict
    /// [`NotExecuted`]: RecoveryVerdict::NotExecuted
    /// [`OutcomeUnknown`]: RecoveryVerdict::OutcomeUnknown
    /// [`InterruptedUnknown`]: RecoveryVerdict::InterruptedUnknown
    /// [`None`]: ExecutionState::None
    /// [`reconcile_late_result`]: Self::reconcile_late_result
    /// [`SessionSeam`]: crate::seam::SessionSeam
    pub fn recover_session(&mut self, verdict: RecoveryVerdict, now: impl Into<String>) {
        let now = now.into();
        let unclosed = unclosed_tool_call_ids(&self.conversation);
        match verdict {
            RecoveryVerdict::NotExecuted => {
                for call_id in &unclosed {
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            recovery_message_id(call_id),
                            call_id,
                            RECOVER_NOT_EXECUTED,
                        ));
                }
                self.execution_state = ExecutionState::None;
            }
            RecoveryVerdict::OutcomeUnknown {
                tool_call_id,
                action,
            } => {
                // At most one mutating call is in flight at a time. Bind the
                // placeholder to the exact call correlated by the runtime's
                // durable work row; other dangling calls were never started.
                if unclosed.iter().any(|call_id| call_id == &tool_call_id) {
                    let placeholder_id = recovery_message_id(&tool_call_id);
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            placeholder_id.clone(),
                            &tool_call_id,
                            RECOVER_OUTCOME_UNKNOWN,
                        ));
                    self.execution_state = ExecutionState::OutcomeUnknown {
                        action,
                        placeholder_message_id: placeholder_id,
                        since: now.clone(),
                    };
                } else {
                    // A runtime supplied an identity that cannot be correlated to
                    // the persisted conversation. Fail closed rather than attach
                    // the late result to the wrong tool call.
                    self.execution_state = ExecutionState::Interrupted { since: now.clone() };
                }
                for call_id in unclosed.iter().filter(|call_id| *call_id != &tool_call_id) {
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            recovery_message_id(call_id),
                            call_id,
                            RECOVER_NOT_EXECUTED,
                        ));
                }
            }
            RecoveryVerdict::InterruptedUnknown => {
                for call_id in &unclosed {
                    self.conversation
                        .push(crate::chat::ChatMessage::tool_result(
                            recovery_message_id(call_id),
                            call_id,
                            RECOVER_INTERRUPTED,
                        ));
                }
                self.execution_state = ExecutionState::Interrupted { since: now.clone() };
            }
        }
        self.finish_turn(TurnState::Failed, now);
        crate::image_input::strip_session_images(&mut self.conversation);
    }
}

/// The placeholder tool-result text written when recovery proves a mutating call
/// never ran.
const RECOVER_NOT_EXECUTED: &str = "not executed: the turn was interrupted before this command ran";
/// The placeholder text for a recovered call whose outcome is unknown but
/// reconcilable; a late real result replaces it in place.
const RECOVER_OUTCOME_UNKNOWN: &str =
    "execution outcome unknown; the command may have executed; do not assume success";
/// The placeholder text for a recovered call with no recoverable identity (the
/// conversation is barred from further mutation).
const RECOVER_INTERRUPTED: &str = "the turn was interrupted; this command's outcome is unknown and cannot be reconciled; do not assume success";

/// Deterministic message id for a recovery-appended tool result, derived from the
/// closed tool call so it is unique within the conversation and stable for the
/// `OutcomeUnknown` placeholder's later in-place reconciliation.
fn recovery_message_id(tool_call_id: &str) -> String {
    format!("recover-{tool_call_id}")
}

/// Find tool-call ids that have no matching tool-result message — an assistant
/// tool call left unanswered when a turn was interrupted mid-execution. Preserves
/// first-seen order and de-duplicates.
fn unclosed_tool_call_ids(conversation: &[crate::chat::ChatMessage]) -> Vec<String> {
    let answered: std::collections::HashSet<&str> = conversation
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    let mut out: Vec<String> = Vec::new();
    for m in conversation {
        for c in &m.tool_calls {
            if !answered.contains(c.id.as_str()) && !out.iter().any(|x| x == &c.id) {
                out.push(c.id.clone());
            }
        }
    }
    out
}

/// The verdict a [`SessionSeam`] supplies to [`PersistedAgentSession::recover_session`]
/// when taking over an orphaned active session — how to close an outstanding
/// mutating tool call. Determining it requires runtime knowledge the pure session
/// does not hold (whether a durable work item was dispatched), so it is passed in.
///
/// [`SessionSeam`]: crate::seam::SessionSeam
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryVerdict {
    /// The command provably never ran (no dispatch); safe to clear the execution
    /// machine and allow a later mutation.
    NotExecuted,
    /// The command was dispatched and carries a recoverable identity; its outcome
    /// is unknown and a late result can reconcile it in place.
    OutcomeUnknown {
        /// The exact dangling call backed by the durable execution identity. A
        /// provider may return several tool calls in one assistant message, so it
        /// is not necessarily the first still-unclosed call after a crash.
        tool_call_id: String,
        action: ActionIdentity,
    },
    /// No recoverable identity is known; close conservatively and bar further
    /// mutation without fabricating an unreconcilable identity.
    InterruptedUnknown,
}

/// Add a [`TokenUsage`] delta into an accumulator, treating `None` as 0 and
/// preserving `Some` once either side reports a count.
fn add_usage(acc: &mut TokenUsage, delta: TokenUsage) {
    if let Some(d) = delta.input_tokens {
        acc.input_tokens = Some(acc.input_tokens.unwrap_or(0) + d);
    }
    if let Some(d) = delta.output_tokens {
        acc.output_tokens = Some(acc.output_tokens.unwrap_or(0) + d);
    }
}

/// Restrictiveness rank of an [`ExecutionMode`]: lower = less capable. The
/// declaration order is meaningfully increasing in capability, so the
/// intersection of two modes is the lower-ranked (more restrictive) one.
fn mode_rank(mode: desk_agent_protocol::ExecutionMode) -> u8 {
    use desk_agent_protocol::ExecutionMode::*;
    match mode {
        SuggestOnly => 0,
        ReadOnly => 1,
        ConfirmEachAction => 2,
        SessionApproved => 3,
        Automated => 4,
    }
}

/// Narrow `start` by `latest`: the intersection of two scopes (never broader than
/// `start`). Granted capabilities are the set intersection; the mode is the more
/// restrictive of the two; expiry is the earlier bound. Used for the narrow-only
/// mid-turn revision change (§5.3) — a running turn can only lose authority.
pub fn narrow_scope(start: &AgentScope, latest: &AgentScope) -> AgentScope {
    let granted: Vec<_> = start
        .granted
        .iter()
        .filter(|c| latest.granted.contains(c))
        .copied()
        .collect();
    let mode = if mode_rank(latest.mode) <= mode_rank(start.mode) {
        latest.mode
    } else {
        start.mode
    };
    let expires_at = match (&start.expires_at, &latest.expires_at) {
        (Some(a), Some(b)) => Some(if a <= b { a.clone() } else { b.clone() }),
        (Some(a), None) => Some(a.clone()),
        (None, b) => b.clone(),
    };
    AgentScope {
        granted,
        mode,
        expires_at,
        policy_name: latest
            .policy_name
            .clone()
            .or_else(|| start.policy_name.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_attachment::{
        AttachmentBounds, AttachmentObjectRef, AttachmentState, CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        ContextAttachmentKind,
    };
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, RetentionBoundary,
        Sensitivity,
    };
    use desk_agent_protocol::{Capability, ExecutionMode};

    fn scope(granted: &[Capability], mode: ExecutionMode) -> AgentScope {
        AgentScope {
            granted: granted.to_vec(),
            mode,
            expires_at: None,
            policy_name: None,
        }
    }

    fn session() -> PersistedAgentSession {
        PersistedAgentSession::new(
            "conv-1",
            "actor-1",
            "device-1",
            7,
            scope(&[Capability::SystemInfo], ExecutionMode::ReadOnly),
            "2026-06-20T00:00:00Z",
        )
    }

    fn context_attachment(id: &str, client_request_id: &str) -> ContextAttachment {
        let digest = "a".repeat(64);
        ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: id.into(),
            client_request_id: client_request_id.into(),
            actor_id: "actor-1".into(),
            device_id: "device-1".into(),
            surface: AgentSessionSurface::DeviceAssistant,
            kind: ContextAttachmentKind::Range,
            object_ref: AttachmentObjectRef {
                opaque_token: format!("token-{id}"),
                object_incarnation: format!("document-{id}"),
                source_provider_id: "office.document".into(),
                source_capability_id: "office.document.inspect".into(),
            },
            bounds: AttachmentBounds {
                max_bytes: 1024,
                max_objects: 16,
            },
            display_summary: format!("Book.xlsx / {id}"),
            created_at_unix_ms: 100,
            expires_at_unix_ms: 200,
            envelope: DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: format!("envelope-{id}"),
                content: ContentRef::ImmutableBlob {
                    blob_id: format!("blob-{id}"),
                    sha256: digest.clone(),
                    size_bytes: 4,
                    media_type: "application/json".into(),
                },
                provenance: DataProvenance {
                    source_provider_id: "office.document".into(),
                    source_tool_name: "inspect_office_document".into(),
                    source_object_id: Some(id.into()),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: digest,
                sensitivity: Sensitivity::Sensitive,
                allowed_destinations: Vec::new(),
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(200),
                    delete_with_run: true,
                },
            },
            state: AttachmentState::Active,
        }
    }

    #[test]
    fn refresh_context_is_atomic_immutable_and_idempotent() {
        let mut value = session();
        value.surface = AgentSessionSurface::DeviceAssistant;
        let old = context_attachment("old", "attach-old");
        let replacement = context_attachment("new", "refresh-new");
        assert!(value.attach_context(old).unwrap());
        assert!(
            value
                .refresh_context(
                    "old",
                    AttachmentStaleReason::DocumentChanged,
                    replacement.clone(),
                )
                .unwrap()
        );
        assert!(matches!(
            value.context_attachments[0].state,
            AttachmentState::Stale {
                reason: AttachmentStaleReason::DocumentChanged
            }
        ));
        assert_eq!(value.active_context(150), vec![&replacement]);
        assert!(
            !value
                .refresh_context("old", AttachmentStaleReason::DocumentChanged, replacement,)
                .unwrap()
        );

        let reused = context_attachment("old", "refresh-reused");
        assert_eq!(
            value.refresh_context("old", AttachmentStaleReason::ObjectChanged, reused),
            Err(ContextAttachmentError::RefreshIdentityReused)
        );
    }

    /// A turn can be claimed from a settled state; a second claim while Running is
    /// rejected; claiming resets the turn-level counters and adopts the new scope.
    #[test]
    fn begin_turn_from_settled_and_resets_counters() {
        let mut s = session();
        s.current_turn_steps = 5;
        s.lifetime_steps = 9;
        let new_scope = scope(
            &[Capability::SystemInfo, Capability::ProcessList],
            ExecutionMode::ReadOnly,
        );
        s.begin_turn(
            "turn-1",
            Some("req-1".into()),
            Some("conn-1".into()),
            8,
            new_scope.clone(),
            "2026-06-20T00:01:00Z",
        )
        .expect("claim from idle");
        assert_eq!(s.turn_state, TurnState::Running);
        assert_eq!(s.current_turn_steps, 0, "turn counter reset");
        assert_eq!(s.lifetime_steps, 9, "lifetime counter preserved");
        assert_eq!(s.policy_revision, 8);
        // Turn boundary adopts the new (here expanded) scope.
        assert_eq!(s.scope_snapshot, new_scope);
        assert_eq!(s.turn_start_scope, new_scope);

        // A second claim while Running is refused.
        assert_eq!(
            s.begin_turn("turn-2", None, None, 8, new_scope, "t"),
            Err(TurnClaimError::Busy)
        );
    }

    /// State classification: settled states ([`Idle`]/[`Failed`]/[`Cancelled`])
    /// are claimable and the complement of the active states
    /// ([`Running`]/[`AwaitingApproval`]).
    #[test]
    fn turn_state_settled_and_active_partition() {
        for st in [TurnState::Idle, TurnState::Failed, TurnState::Cancelled] {
            assert!(st.is_settled(), "{st:?} settled");
            assert!(!st.is_active(), "{st:?} not active");
            assert!(st.can_claim(), "{st:?} claimable");
        }
        for st in [TurnState::Running, TurnState::AwaitingApproval] {
            assert!(st.is_active(), "{st:?} active");
            assert!(!st.is_settled(), "{st:?} not settled");
            assert!(!st.can_claim(), "{st:?} not claimable");
        }
    }

    /// A follow-up turn can be claimed after the previous turn ended in `Failed`,
    /// and the accumulated conversation history is preserved across the re-claim
    /// (so the model sees the prior turns). An `AwaitingApproval`/`Running` session
    /// stays `Busy`.
    #[test]
    fn settled_session_reclaims_and_keeps_history() {
        use crate::chat::{ChatMessage, ChatRole};
        let mut s = session();
        // First turn runs, accumulates history, then fails.
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .expect("first claim");
        s.conversation
            .push(ChatMessage::text("u1", ChatRole::User, "why is cpu high?"));
        s.conversation
            .push(ChatMessage::text("a1", ChatRole::Assistant, "checking..."));
        s.finish_turn(TurnState::Failed, "t");
        assert_eq!(s.turn_state, TurnState::Failed);

        // A second turn can be claimed from the failed state, history intact.
        s.begin_turn("t2", None, None, 7, s.scope_snapshot.clone(), "t")
            .expect("reclaim from failed");
        assert_eq!(s.turn_state, TurnState::Running);
        assert_eq!(s.conversation.len(), 2, "history preserved across reclaim");

        // While Running, a further claim is refused.
        assert_eq!(
            s.begin_turn("t3", None, None, 7, s.scope_snapshot.clone(), "t"),
            Err(TurnClaimError::Busy)
        );

        // AwaitingApproval is likewise not directly claimable.
        s.turn_state = TurnState::AwaitingApproval;
        assert_eq!(
            s.begin_turn("t4", None, None, 7, s.scope_snapshot.clone(), "t"),
            Err(TurnClaimError::Busy)
        );
    }

    /// A `User` claim starts a fresh chain (its turn id) and zeroes the budget; a
    /// following `ExecCompletion` claim keeps the chain id and spends one budget per
    /// claim; the next `User` claim resets both again.
    #[test]
    fn adopt_trigger_tracks_chain_and_spends_budget() {
        let mut s = session();

        // A user turn opens chain "u1" with a clean budget.
        s.adopt_trigger(TriggerOrigin::User, "u1");
        assert_eq!(s.trigger_origin, TriggerOrigin::User);
        assert_eq!(s.chain_id, "u1");
        assert_eq!(s.automation_turns_used, 0);

        // Two automation turns on that chain each spend one budget; the chain id is
        // preserved (an automation claim continues the chain, never opens a new one).
        s.adopt_trigger(TriggerOrigin::ExecCompletion, "auto-a");
        assert_eq!(s.trigger_origin, TriggerOrigin::ExecCompletion);
        assert_eq!(s.chain_id, "u1", "an automation claim keeps the chain id");
        assert_eq!(s.automation_turns_used, 1);
        s.adopt_trigger(TriggerOrigin::ExecCompletion, "auto-b");
        assert_eq!(s.automation_turns_used, 2);

        // An owner permission decision can consume the newly issued grant but
        // neither opens a fresh chain nor spends automation budget.
        s.adopt_trigger(TriggerOrigin::PermissionDecision, "permission-a");
        assert_eq!(s.trigger_origin, TriggerOrigin::PermissionDecision);
        assert!(s.trigger_origin.allows_new_mutation());
        assert_eq!(s.chain_id, "u1");
        assert_eq!(s.automation_turns_used, 2);

        // A new user turn supersedes the chain and resets the budget.
        s.adopt_trigger(TriggerOrigin::User, "u2");
        assert_eq!(s.chain_id, "u2");
        assert_eq!(s.automation_turns_used, 0);
    }

    /// `finish_turn` settles only the turn machine; the execution machine is left
    /// untouched (a late result reconciles it separately).
    #[test]
    fn finish_turn_leaves_execution_state() {
        let mut s = session();
        s.execution_state = ExecutionState::OutcomeUnknown {
            action: ActionIdentity::agent_exec(1, "x", "e"),
            placeholder_message_id: "p".into(),
            since: "t".into(),
        };
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.finish_turn(TurnState::Idle, "2026-06-20T00:02:00Z");
        assert_eq!(s.turn_state, TurnState::Idle);
        assert!(
            matches!(s.execution_state, ExecutionState::OutcomeUnknown { .. }),
            "execution state must survive finish_turn"
        );
    }

    /// The subject check ignores the connection id (reconnect can continue) but
    /// rejects a different actor / device.
    #[test]
    fn subject_check_ignores_connection_but_pins_identity() {
        let s = session();
        assert!(s.check_subject("actor-1", "device-1").is_ok());
        assert_eq!(
            s.check_subject("actor-9", "device-1"),
            Err(SubjectMismatch::Actor)
        );
        assert_eq!(
            s.check_subject("actor-1", "device-9"),
            Err(SubjectMismatch::Device)
        );
    }

    #[test]
    fn surface_check_rejects_unassigned_and_cross_surface_sessions() {
        let mut session = session();
        assert_eq!(session.surface, AgentSessionSurface::Unknown);
        assert_eq!(
            session.check_surface(AgentSessionSurface::DeviceAssistant),
            Err(SubjectMismatch::Surface)
        );

        session.adopt_client_metadata(None, AgentSessionSurface::DeviceAssistant);
        assert!(
            session
                .check_surface(AgentSessionSurface::DeviceAssistant)
                .is_ok()
        );
        assert_eq!(
            session.check_surface(AgentSessionSurface::TerminalCopilot),
            Err(SubjectMismatch::Surface)
        );
    }

    /// A mid-turn revision change narrows only: capabilities shrink to the
    /// intersection and the mode drops to the more restrictive, never expanding
    /// past the turn-start scope.
    #[test]
    fn narrow_for_revision_only_shrinks() {
        let mut s = session();
        s.begin_turn(
            "t1",
            None,
            None,
            7,
            scope(
                &[Capability::SystemInfo, Capability::ProcessList],
                ExecutionMode::ConfirmEachAction,
            ),
            "t",
        )
        .unwrap();
        // Latest policy tries to add NetworkPorts (ignored — only intersection)
        // and is more restrictive on mode (ReadOnly < ConfirmEachAction → wins).
        s.narrow_for_revision(
            9,
            &scope(
                &[Capability::ProcessList, Capability::NetworkPorts],
                ExecutionMode::ReadOnly,
            ),
        );
        assert_eq!(s.scope_snapshot.granted, vec![Capability::ProcessList]);
        assert_eq!(s.scope_snapshot.mode, ExecutionMode::ReadOnly);
        assert_eq!(s.policy_revision, 9);
    }

    /// Steps and tokens accumulate against both turn-level and lifetime counters;
    /// the per-turn budget trips at `MAX_STEPS_PER_TURN`.
    #[test]
    fn step_counting_and_budget() {
        let mut s = session();
        for _ in 0..crate::MAX_STEPS_PER_TURN {
            assert!(!s.turn_step_budget_exhausted(crate::MAX_STEPS_PER_TURN));
            s.record_step(TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                ..Default::default()
            });
        }
        assert!(s.turn_step_budget_exhausted(crate::MAX_STEPS_PER_TURN));
        assert_eq!(s.current_turn_steps, crate::MAX_STEPS_PER_TURN);
        assert_eq!(
            s.current_turn_tokens.input_tokens,
            Some(10 * crate::MAX_STEPS_PER_TURN as i64)
        );
        assert_eq!(
            s.lifetime_tokens.output_tokens,
            Some(2 * crate::MAX_STEPS_PER_TURN as i64)
        );
    }

    /// A late result reconciles an unknown outcome: it replaces the placeholder
    /// message text in place (no second tool result) and clears the execution
    /// machine; a mismatched execution id or an already-resolved state is a no-op.
    #[test]
    fn reconcile_late_result_replaces_placeholder_in_place() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "ph-1",
            "call-1",
            "execution outcome unknown; the command may have executed; do not assume success",
        ));
        s.execution_state = ExecutionState::OutcomeUnknown {
            action: ActionIdentity::agent_exec(7, "req-1", "exec-1"),
            placeholder_message_id: "ph-1".into(),
            since: "2026-06-20T00:00:00Z".into(),
        };
        let conv_len = s.conversation.len();

        // A different execution id does not reconcile.
        assert!(!s.reconcile_late_result("other", "ran ok", "2026-06-20T00:01:00Z"));
        assert!(matches!(
            s.execution_state,
            ExecutionState::OutcomeUnknown { .. }
        ));

        // The matching id replaces the placeholder text in place and clears state.
        assert!(s.reconcile_late_result("exec-1", "exit_code=0", "2026-06-20T00:02:00Z"));
        assert_eq!(s.execution_state, ExecutionState::None);
        assert_eq!(
            s.conversation.len(),
            conv_len,
            "no second tool result appended"
        );
        let ph = s
            .conversation
            .iter()
            .find(|m| m.message_id == "ph-1")
            .unwrap();
        assert_eq!(ph.text, "exit_code=0");
        assert_eq!(ph.tool_call_id.as_deref(), Some("call-1"));

        // A second reconcile is a no-op (already resolved).
        assert!(!s.reconcile_late_result("exec-1", "again", "2026-06-20T00:03:00Z"));
    }

    /// An event is appended once and keyed by its `event_id`; a redelivery of the
    /// same event finds it present and is a no-op (the durable queue may deliver
    /// more than once). A different event_id appends a second message.
    #[test]
    fn append_event_if_absent_is_idempotent_by_event_id() {
        let mut s = session();
        let base = s.conversation.len();

        assert!(s.append_event_if_absent("ev-1", "task exec_a finished: exit 0", "t1"));
        assert_eq!(s.conversation.len(), base + 1);
        let msg = s.conversation.last().unwrap();
        assert_eq!(msg.message_id, "ev-1");
        assert_eq!(msg.role, crate::chat::ChatRole::SystemEvent);
        assert_eq!(msg.text, "task exec_a finished: exit 0");

        // Redelivery of the same event_id is a no-op, even with different text.
        assert!(!s.append_event_if_absent("ev-1", "task exec_a finished: exit 99", "t2"));
        assert_eq!(s.conversation.len(), base + 1, "no duplicate append");
        assert_eq!(
            s.conversation.last().unwrap().text,
            "task exec_a finished: exit 0",
            "the first-delivered text is kept"
        );

        // A distinct event_id appends a new message.
        assert!(s.append_event_if_absent("ev-2", "task exec_b finished: exit 0", "t3"));
        assert_eq!(s.conversation.len(), base + 2);
    }

    /// A completion for an outstanding background dispatch (Executing, tool call
    /// already closed with a running-task placeholder) appends the output as a
    /// fenced untrusted-output message (never a system event) and clears the
    /// execution machine; a redelivery is a no-op.
    #[test]
    fn apply_completion_appends_untrusted_output_for_a_closed_call() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        let mut s = session();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-1".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        // The dispatch already closed the call with a running-task placeholder.
        s.conversation.push(ChatMessage::tool_result(
            "run-1",
            "call-1",
            "dispatched; still running",
        ));
        s.execution_state = ExecutionState::Executing {
            action: ActionIdentity::agent_exec(8, "exec_t9", "e9"),
        };
        let base = s.conversation.len();

        assert!(s.apply_completion("work:8:done", "e9", "call-1", "task-9", "exit_code=0", "t1"));
        assert_eq!(s.conversation.len(), base + 1);
        let ev = s.conversation.last().unwrap();
        assert_eq!(ev.message_id, "work:8:done");
        assert_eq!(
            ev.role,
            ChatRole::UntrustedOutput,
            "device output must be fenced as untrusted data, not a system message"
        );
        assert_eq!(ev.text, "exit_code=0");
        assert_eq!(
            ev.tool_call_id.as_deref(),
            Some("call-1"),
            "the snapshot can associate the late completion with its tool card"
        );
        assert_eq!(ev.background_task_id.as_deref(), Some("task-9"));
        assert_eq!(
            s.execution_state,
            ExecutionState::None,
            "the dispatch is settled"
        );

        // Redelivery of the same event is a no-op.
        assert!(!s.apply_completion("work:8:done", "e9", "call-1", "task-9", "exit_code=0", "t2"));
        assert_eq!(s.conversation.len(), base + 1);
    }

    /// A completion for a tool call still open (a foreground save was lost before it
    /// closed the call) closes it with the real result, keyed by the event id.
    #[test]
    fn apply_completion_closes_an_open_call() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        let mut s = session();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-1".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        // No tool result for call-1 yet — the call is open.
        assert_eq!(s.unclosed_tool_call_ids(), vec!["call-1".to_string()]);

        assert!(s.apply_completion("work:8:done", "e9", "call-1", "task-9", "exit_code=0", "t1"));
        assert!(
            s.unclosed_tool_call_ids().is_empty(),
            "the call is now closed"
        );
        let msg = s.conversation.last().unwrap();
        assert_eq!(msg.message_id, "work:8:done");
        assert_eq!(msg.role, ChatRole::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(msg.background_task_id.as_deref(), Some("task-9"));
        assert_eq!(msg.text, "exit_code=0");
    }

    /// A completion for a recovered unknown outcome replaces the placeholder in place
    /// (no second result) and re-keys it to the event id, so a redelivery dedups.
    #[test]
    fn apply_completion_reconciles_unknown_outcome_and_rekeys() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "ph-1",
            "call-1",
            "execution outcome unknown; the command may have executed",
        ));
        s.execution_state = ExecutionState::OutcomeUnknown {
            action: ActionIdentity::agent_exec(8, "exec_t9", "e9"),
            placeholder_message_id: "ph-1".into(),
            since: "2026-06-20T00:00:00Z".into(),
        };
        let base = s.conversation.len();

        assert!(s.apply_completion("work:8:done", "e9", "call-1", "task-9", "exit_code=0", "t1"));
        assert_eq!(s.conversation.len(), base, "no second result appended");
        assert_eq!(s.execution_state, ExecutionState::None);
        let msg = s
            .conversation
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-1"))
            .unwrap();
        assert_eq!(msg.text, "exit_code=0");
        assert_eq!(msg.message_id, "work:8:done", "the placeholder is re-keyed");

        // Redelivery finds the re-keyed message present and is a no-op.
        assert!(!s.apply_completion("work:8:done", "e9", "call-1", "task-9", "again", "t2"));
        assert_eq!(s.conversation.len(), base);
    }

    /// A completion whose event id is already present (a foreground result the loop
    /// keyed on the delivery id) is a no-op — it never doubles the result.
    #[test]
    fn apply_completion_dedups_a_foreground_keyed_result() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "work:8:done",
            "call-1",
            "exit_code=0",
        ));
        let base = s.conversation.len();
        assert!(!s.apply_completion("work:8:done", "e9", "call-1", "task-9", "exit_code=0", "t1"));
        assert_eq!(s.conversation.len(), base);
    }

    /// Recovering a dispatched item with no result transitions `Executing` to
    /// `OutcomeUnknown`, repurposing the running-task result as the reconcile anchor
    /// (its text is rewritten to the unknown-outcome placeholder) and letting a late
    /// real result still land in place.
    #[test]
    fn mark_execution_unknown_transitions_and_anchors_the_placeholder() {
        let mut s = session();
        // The dispatch closed the call with a running-task placeholder.
        s.conversation
            .push(crate::chat::ChatMessage::background_task_running(
                "run-1", "call-1", "exec_t9",
            ));
        s.execution_state = ExecutionState::Executing {
            action: ActionIdentity::agent_exec(8, "exec_t9", "e9"),
        };
        let base = s.conversation.len();

        assert!(s.mark_execution_unknown("e9", "call-1", "2026-06-20T00:00:00Z"));
        assert_eq!(s.conversation.len(), base, "no message appended");
        match &s.execution_state {
            ExecutionState::OutcomeUnknown {
                action,
                placeholder_message_id,
                ..
            } => {
                assert_eq!(action.work_id, 8);
                assert_eq!(action.execution_id, "e9");
                assert_eq!(action.action_request_id, "exec_t9");
                assert_eq!(placeholder_message_id, "run-1");
            }
            other => panic!("expected OutcomeUnknown, got {other:?}"),
        }
        let anchor = s
            .conversation
            .iter()
            .find(|m| m.message_id == "run-1")
            .unwrap();
        assert_eq!(anchor.text, RECOVER_OUTCOME_UNKNOWN);
        assert!(!s.execution_state.allows_new_mutation());

        // A late real result reconciles the anchor in place.
        assert!(s.reconcile_late_result("e9", "exit_code=0", "2026-06-20T00:01:00Z"));
        assert_eq!(s.execution_state, ExecutionState::None);
        let anchor = s
            .conversation
            .iter()
            .find(|m| m.message_id == "run-1")
            .unwrap();
        assert_eq!(anchor.text, "exit_code=0");
    }

    /// The transition is scoped to the matching execution id: a mismatched id (a
    /// stale recovery for an execution the session already moved past) is a no-op.
    #[test]
    fn mark_execution_unknown_ignores_a_mismatched_execution() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "run-1",
            "call-1",
            "still running",
        ));
        s.execution_state = ExecutionState::Executing {
            action: ActionIdentity::agent_exec(8, "exec_t9", "e9"),
        };
        assert!(!s.mark_execution_unknown("e-other", "call-1", "2026-06-20T00:00:00Z"));
        assert!(
            matches!(s.execution_state, ExecutionState::Executing { .. }),
            "state is untouched"
        );
    }

    /// A session not in `Executing` (already settled to `None`, e.g. the completion
    /// won the race) is a no-op — recovery must not resurrect a barred state.
    #[test]
    fn mark_execution_unknown_is_a_no_op_when_not_executing() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::tool_result(
            "run-1",
            "call-1",
            "exit_code=0",
        ));
        assert_eq!(s.execution_state, ExecutionState::None);
        assert!(!s.mark_execution_unknown("e9", "call-1", "2026-06-20T00:00:00Z"));
        assert_eq!(s.execution_state, ExecutionState::None);
    }

    /// Without the running-task result closing the call (nothing to reconcile a late
    /// result against) the transition is refused, leaving `Executing` intact.
    #[test]
    fn mark_execution_unknown_requires_the_closing_result() {
        let mut s = session();
        s.execution_state = ExecutionState::Executing {
            action: ActionIdentity::agent_exec(8, "exec_t9", "e9"),
        };
        assert!(!s.mark_execution_unknown("e9", "call-1", "2026-06-20T00:00:00Z"));
        assert!(matches!(
            s.execution_state,
            ExecutionState::Executing { .. }
        ));
    }

    /// Each claim rotates the fencing token, so a stale prior owner can be told
    /// apart from the current one.
    #[test]
    fn begin_turn_rotates_lease_token() {
        let mut s = session();
        assert_eq!(s.lease_token, 0);
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        assert_eq!(s.lease_token, 1, "first claim rotates to 1");
        s.finish_turn(TurnState::Idle, "t");
        s.begin_turn("t2", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        assert_eq!(s.lease_token, 2, "second claim rotates again");
    }

    /// The public accessor surfaces dangling tool calls (no matching tool result)
    /// so a recovering seam can correlate them with durable work; answered calls are
    /// excluded.
    #[test]
    fn unclosed_tool_call_ids_lists_only_dangling_calls() {
        use crate::chat::{ChatMessage, ToolCallRef};
        let mut s = session();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![
                ToolCallRef {
                    id: "answered".into(),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                },
                ToolCallRef {
                    id: "dangling".into(),
                    name: "exec_command".into(),
                    arguments_json: "{}".into(),
                },
            ],
        ));
        s.conversation
            .push(ChatMessage::tool_result("r1", "answered", "ok"));
        assert_eq!(s.unclosed_tool_call_ids(), vec!["dangling".to_string()]);
    }

    /// Recovery of an interrupted mutating turn with no recoverable identity closes
    /// the unclosed tool call, settles to Failed, and bars further mutation.
    #[test]
    fn recover_interrupted_unknown_closes_and_bars_mutation() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        let mut s = session();
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.conversation.push(
            ChatMessage::text("u1", ChatRole::User, "restart it")
                .with_image("data:image/jpeg;base64,AQID"),
        );
        // Assistant requested a mutating call that never got a tool result (crash
        // during the approval/exec wait → AwaitingApproval).
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-x".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        s.turn_state = TurnState::AwaitingApproval;

        s.recover_session(RecoveryVerdict::InterruptedUnknown, "2026-06-20T01:00:00Z");

        assert_eq!(s.turn_state, TurnState::Failed);
        assert!(
            matches!(s.execution_state, ExecutionState::Interrupted { .. }),
            "interrupted bars new mutation"
        );
        assert!(!s.execution_state.allows_new_mutation());
        assert!(
            s.conversation
                .iter()
                .all(|message| message.image_data_url.is_none())
        );
        assert!(
            s.conversation[0]
                .text
                .contains(crate::image_input::IMAGE_NOT_RETAINED_PLACEHOLDER)
        );
        let closed = s
            .conversation
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-x"))
            .expect("the unclosed call was closed");
        assert_eq!(closed.role, ChatRole::Tool);
        assert!(closed.text.contains("interrupted"));
    }

    /// Recovery with `NotExecuted` closes the call as not-run and clears the
    /// execution machine so a later turn may mutate again.
    #[test]
    fn recover_not_executed_clears_execution_state() {
        use crate::chat::{ChatMessage, ToolCallRef};
        let mut s = session();
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![ToolCallRef {
                id: "call-y".into(),
                name: "exec_command".into(),
                arguments_json: "{}".into(),
            }],
        ));
        s.recover_session(RecoveryVerdict::NotExecuted, "t2");
        assert_eq!(s.turn_state, TurnState::Failed);
        assert_eq!(s.execution_state, ExecutionState::None);
        assert!(s.execution_state.allows_new_mutation());
    }

    /// Recovery with `OutcomeUnknown` records the identity + a placeholder that a
    /// late real result reconciles in place.
    #[test]
    fn recover_outcome_unknown_records_reconcilable_placeholder() {
        use crate::chat::{ChatMessage, ToolCallRef};
        let mut s = session();
        s.begin_turn("t1", None, None, 7, s.scope_snapshot.clone(), "t")
            .unwrap();
        s.conversation.push(ChatMessage::assistant_tool_calls(
            "a1",
            String::new(),
            vec![
                ToolCallRef {
                    id: "read-before".into(),
                    name: "system_info".into(),
                    arguments_json: "{}".into(),
                },
                ToolCallRef {
                    id: "call-z".into(),
                    name: "exec_command".into(),
                    arguments_json: "{}".into(),
                },
            ],
        ));
        s.recover_session(
            RecoveryVerdict::OutcomeUnknown {
                tool_call_id: "call-z".into(),
                action: ActionIdentity::agent_exec(9, "rq-9", "exec-9"),
            },
            "t2",
        );
        assert_eq!(s.turn_state, TurnState::Failed);
        assert!(!s.execution_state.allows_new_mutation());
        assert!(s.conversation.iter().any(|message| {
            message.tool_call_id.as_deref() == Some("read-before")
                && message.text == RECOVER_NOT_EXECUTED
        }));
        // A late result for exec-9 reconciles the placeholder in place.
        assert!(s.reconcile_late_result("exec-9", "exit_code=0", "t3"));
        assert_eq!(s.execution_state, ExecutionState::None);
        let closed = s
            .conversation
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("call-z"))
            .unwrap();
        assert_eq!(closed.text, "exit_code=0");
    }

    /// The session round-trips through serde (manager persistence).
    #[test]
    fn session_round_trips() {
        let mut s = session();
        s.conversation.push(crate::chat::ChatMessage::text(
            "m0",
            crate::chat::ChatRole::User,
            "hi",
        ));
        s.trigger_origin = TriggerOrigin::ExecCompletion;
        s.add_pending_auto_trigger(pending("ev-a", "2026-07-20T00:00:01Z"));
        let json = serde_json::to_string(&s).unwrap();
        let back = PersistedAgentSession::decode_json(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.trigger_origin, TriggerOrigin::ExecCompletion);
        assert_eq!(back.pending_auto_triggers.len(), 1);

        // A state written before the field existed decodes to an empty set.
        let mut old = serde_json::to_value(session()).unwrap();
        old.as_object_mut().unwrap().remove("pending_auto_triggers");
        let back: PersistedAgentSession = serde_json::from_value(old).unwrap();
        assert!(back.pending_auto_triggers.is_empty());
    }

    #[test]
    fn execution_state_upgrades_legacy_exec_identity_and_dual_writes_it() {
        let old = serde_json::json!({
            "kind": "executing",
            "work_id": 7,
            "execution_id": "generation-1",
            "exec_request_id": "exec_7"
        });
        let state: ExecutionState = serde_json::from_value(old).unwrap();
        let ExecutionState::Executing { action } = &state else {
            panic!("expected executing state");
        };
        assert_eq!(action.kind, WorkKind::AgentExec);
        assert_eq!(action.action_request_id, "exec_7");

        let encoded = serde_json::to_value(state).unwrap();
        assert_eq!(encoded["action_request_id"], "exec_7");
        assert_eq!(encoded["exec_request_id"], "exec_7");
        assert_eq!(encoded["work_kind"], "agent_exec");
    }

    #[test]
    fn computer_action_identity_never_serializes_an_exec_correlation() {
        let state = ExecutionState::Executing {
            action: ActionIdentity::new(
                8,
                "action_windows_1",
                "generation-2",
                WorkKind::ComputerAction,
            ),
        };
        let encoded = serde_json::to_value(state).unwrap();
        assert_eq!(encoded["action_request_id"], "action_windows_1");
        assert_eq!(encoded["work_kind"], "computer_action");
        assert!(encoded.get("exec_request_id").is_none());
    }

    #[test]
    fn mismatched_exec_compatibility_correlations_fail_closed() {
        let value = serde_json::json!({
            "kind": "executing",
            "work_id": 7,
            "execution_id": "generation-1",
            "work_kind": "agent_exec",
            "action_request_id": "exec_new",
            "exec_request_id": "exec_old"
        });
        assert!(serde_json::from_value::<ExecutionState>(value).is_err());
    }

    #[test]
    fn decoder_upgrades_v0_tool_calls_and_rejects_unknown_versions() {
        use crate::chat::ToolCallRef;
        let mut legacy = session();
        legacy
            .conversation
            .push(crate::chat::ChatMessage::assistant_tool_calls(
                "a1",
                "",
                vec![ToolCallRef {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                }],
            ));
        let mut value = serde_json::to_value(legacy).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("conversation_schema_version");
        value["conversation"][0]
            .as_object_mut()
            .unwrap()
            .remove("replay_disposition");
        let upgraded = PersistedAgentSession::decode_json(&value.to_string()).unwrap();
        assert_eq!(
            upgraded.conversation_schema_version,
            CONVERSATION_SCHEMA_VERSION
        );
        assert!(matches!(
            upgraded.conversation[0].replay_disposition,
            Some(ReplayDisposition::Unavailable { .. })
        ));

        value["conversation_schema_version"] = serde_json::json!(99);
        assert!(matches!(
            PersistedAgentSession::decode_json(&value.to_string()),
            Err(SessionDecodeError::UnsupportedVersion(99))
        ));

        let mut invalid = session();
        invalid
            .conversation
            .push(crate::chat::ChatMessage::context_summary(
                "cp",
                "must remain synthetic",
            ));
        assert!(matches!(
            PersistedAgentSession::decode_json(&serde_json::to_string(&invalid).unwrap()),
            Err(SessionDecodeError::PersistedContextSummary(id)) if id == "cp"
        ));
    }

    #[test]
    fn storage_limit_tombstones_opaque_replay_without_deleting_transcript() {
        use crate::chat::ToolCallRef;
        use crate::model_profile::WireProtocol;
        use crate::replay::{ProviderReplayEnvelope, ReplayCodec, SourceContextKey};

        let mut value = session();
        let source = SourceContextKey::derive(
            WireProtocol::OpenAiChatCompletions,
            "connection-1",
            "model-1",
            "model",
        );
        value
            .conversation
            .push(crate::chat::ChatMessage::assistant_tool_calls_with_replay(
                "a1",
                "visible",
                vec![ToolCallRef {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments_json: "{}".into(),
                }],
                ReplayDisposition::Present {
                    envelope: ProviderReplayEnvelope::new(
                        ReplayCodec::OpenAiReasoningContent,
                        source.clone(),
                        serde_json::Value::String("x".repeat(MAX_REPLAY_ENVELOPE_BYTES)),
                    ),
                },
            ));

        let encoded = value.encode_json_for_storage().unwrap();
        assert!(encoded.contains("visible"));
        let stored = PersistedAgentSession::decode_json(&encoded).unwrap();
        assert!(matches!(
            stored.conversation[0].replay_disposition,
            Some(ReplayDisposition::Unavailable {
                source_context_key: Some(ref key),
                reason: ReplayUnavailableReason::EvictedByStorageLimit,
            }) if key == &source
        ));
        assert!(matches!(
            value.conversation[0].replay_disposition,
            Some(ReplayDisposition::Present { .. })
        ));
    }

    #[test]
    fn storage_projection_strips_images_without_mutating_live_session() {
        let mut value = session();
        value.conversation.push(
            crate::chat::ChatMessage::text("screen", crate::chat::ChatRole::Tool, "captured")
                .with_image("data:image/jpeg;base64,AQID"),
        );

        let encoded = value.encode_json_for_storage().unwrap();
        assert!(!encoded.contains("data:image/jpeg;base64,AQID"));
        assert!(value.conversation[0].image_data_url.is_some());
        let stored = PersistedAgentSession::decode_json(&encoded).unwrap();
        assert!(stored.conversation[0].image_data_url.is_none());
        assert!(
            stored.conversation[0]
                .text
                .contains(crate::image_input::IMAGE_NOT_RETAINED_PLACEHOLDER)
        );
    }

    fn pending(event_id: &str, since: &str) -> PendingAutoTrigger {
        PendingAutoTrigger {
            work_id: 7,
            kind: WorkKind::AgentExec,
            execution_id: "e".into(),
            tool_call_id: "call-1".into(),
            event_id: event_id.into(),
            chain_id: "chain-1".into(),
            resolution_org_id: Some(3),
            since: since.into(),
        }
    }

    /// Pending auto-triggers are added deduped by `event_id`; `earliest_pending_at`
    /// tracks the chronological (lexical, RFC3339 UTC) minimum `since`.
    #[test]
    fn pending_add_dedups_and_tracks_earliest() {
        let mut s = session();
        assert_eq!(s.earliest_pending_at(), None);

        assert!(s.add_pending_auto_trigger(pending("ev-b", "2026-07-20T00:00:05Z")));
        assert!(s.add_pending_auto_trigger(pending("ev-a", "2026-07-20T00:00:01Z")));
        // A second entry for the same event_id is rejected (idempotent delivery).
        assert!(!s.add_pending_auto_trigger(pending("ev-a", "2026-07-20T00:00:09Z")));

        assert_eq!(s.pending_auto_triggers.len(), 2);
        assert_eq!(s.earliest_pending_at(), Some("2026-07-20T00:00:01Z"));
    }

    /// `clear_reacted_auto_triggers` drops exactly the entries whose `event_id` is
    /// in the request the model reacted to, and reports the count; `finish_turn`
    /// never touches the set (a circuit-broken turn keeps its pending work).
    #[test]
    fn pending_clear_reacted_by_membership_and_finish_turn_preserves() {
        let mut s = session();
        s.add_pending_auto_trigger(pending("ev-a", "t1"));
        s.add_pending_auto_trigger(pending("ev-b", "t2"));
        s.add_pending_auto_trigger(pending("ev-c", "t3"));

        // finish_turn must not drop anything.
        s.finish_turn(TurnState::Idle, "t9");
        assert_eq!(s.pending_auto_triggers.len(), 3);

        // Only ev-a and ev-c were in the request the model reacted to.
        let seen: HashSet<String> = ["ev-a".to_string(), "ev-c".to_string(), "m0".to_string()]
            .into_iter()
            .collect();
        assert_eq!(s.clear_reacted_auto_triggers(&seen), 2);
        assert_eq!(s.pending_auto_triggers.len(), 1);
        assert_eq!(s.pending_auto_triggers[0].event_id, "ev-b");

        // A direct removal by event_id.
        assert!(s.remove_pending_auto_trigger("ev-b"));
        assert!(!s.remove_pending_auto_trigger("ev-b"));
        assert!(s.pending_auto_triggers.is_empty());
        assert_eq!(s.earliest_pending_at(), None);
    }

    /// A fresh session defaults to a `User` origin, and a persisted state written
    /// before the field existed decodes to `User` (serde default), so old sessions
    /// keep the safe, unrestricted origin.
    #[test]
    fn trigger_origin_defaults_to_user() {
        assert_eq!(session().trigger_origin, TriggerOrigin::User);

        let mut json = serde_json::to_value(session()).unwrap();
        json.as_object_mut().unwrap().remove("trigger_origin");
        let back: PersistedAgentSession = serde_json::from_value(json).unwrap();
        assert_eq!(back.trigger_origin, TriggerOrigin::User);
    }
}
