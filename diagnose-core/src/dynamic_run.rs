//! Provider-neutral contracts for the dynamic Device Assistant run ledger.
//!
//! These types describe durable facts and the model-maintained task projection;
//! they are deliberately not an execution graph. Authorization and dispatch
//! continue to use the durable-action and capability-provider contracts.

use std::collections::BTreeSet;
use std::fmt;

use desk_agent_protocol::capability_provider::{
    CapabilityCancelRequest, CapabilityCompletionClass, CapabilityCompletionEvent,
    CapabilityEffect, CapabilityProgressEvent, CapabilityTaskRef, ExecutionPolicy,
};
use desk_agent_protocol::data_lineage::{DataEnvelope, DestinationIdentity};
use sha2::{Digest, Sha256};

/// Provenance namespace for server-owned run bookkeeping. Results carrying
/// this provider id are deterministic derivatives of already-authorized model
/// context; they are not independently selectable device-data sources.
pub const RUN_CONTROL_PROVIDER_ID: &str = "assistant.run_control";
use serde::{Deserialize, Serialize};

pub const AGENT_RUN_EVENT_SCHEMA_VERSION: u16 = 1;
pub const TASK_STATUS_PROJECTION_SCHEMA_VERSION: u16 = 1;
pub const PERMISSION_REQUEST_SCHEMA_VERSION: u16 = 1;
pub const BACKGROUND_TASK_SCHEMA_VERSION: u16 = 1;
pub const MAX_TASK_STATUS_ITEMS: usize = 128;
pub const MAX_PERMISSION_REQUESTS: usize = 16;
pub const MAX_PERMISSION_REQUEST_ITEMS: usize = 16;
pub const MAX_PERMISSION_SCOPE_VALUES: usize = 32;
pub const MAX_PERMISSION_EXACT_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_RUN_EVENT_IDS: usize = 128;
pub const MAX_TASK_DESCRIPTION_BYTES: usize = 512;
pub const MAX_TASK_NOTE_BYTES: usize = 1024;
pub const MAX_PERMISSION_REASON_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    InProgress,
    Blocked,
    Done,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusItem {
    pub item_id: String,
    pub description: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub last_updated_step_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusProjection {
    pub schema_version: u16,
    /// Model-produced monotonic projection revision. It is a UX projection and
    /// never authorizes a tool or overrides durable execution facts.
    pub revision: u64,
    #[serde(default)]
    pub items: Vec<TaskStatusItem>,
    pub updated_at: String,
}

impl TaskStatusProjection {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        if self.schema_version != TASK_STATUS_PROJECTION_SCHEMA_VERSION {
            return Err(DynamicRunContractError::UnsupportedProjectionVersion(
                self.schema_version,
            ));
        }
        if self.revision == 0 {
            return Err(DynamicRunContractError::InvalidRevision);
        }
        validate_text("updated_at", &self.updated_at, MAX_RUN_EVENT_IDS)?;
        if self.items.len() > MAX_TASK_STATUS_ITEMS {
            return Err(DynamicRunContractError::TooManyProjectionItems);
        }
        let mut ids = BTreeSet::new();
        for item in &self.items {
            validate_id("item_id", &item.item_id)?;
            validate_text("description", &item.description, MAX_TASK_DESCRIPTION_BYTES)?;
            validate_id("last_updated_step_id", &item.last_updated_step_id)?;
            if let Some(note) = &item.note {
                validate_text("note", note, MAX_TASK_NOTE_BYTES)?;
            }
            if !ids.insert(item.item_id.as_str()) {
                return Err(DynamicRunContractError::DuplicateProjectionItem);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunEventKind {
    UserFollowup,
    ObjectContextUpdated,
    LiveContextUpdated,
    ModelStep,
    ToolDiscovery,
    PermissionRequested,
    PermissionDecided,
    ToolCall,
    ToolResult,
    ArtifactProduced,
    BackgroundProgress,
    BackgroundCompletion,
    CancelRequested,
    CancelDelivered,
    TaskStatusUpdated,
    Superseded,
}

impl AgentRunEventKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserFollowup => "user_followup",
            Self::ObjectContextUpdated => "object_context_updated",
            Self::LiveContextUpdated => "live_context_updated",
            Self::ModelStep => "model_step",
            Self::ToolDiscovery => "tool_discovery",
            Self::PermissionRequested => "permission_requested",
            Self::PermissionDecided => "permission_decided",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ArtifactProduced => "artifact_produced",
            Self::BackgroundProgress => "background_progress",
            Self::BackgroundCompletion => "background_completion",
            Self::CancelRequested => "cancel_requested",
            Self::CancelDelivered => "cancel_delivered",
            Self::TaskStatusUpdated => "task_status_updated",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRunEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub run_id: String,
    pub event_seq: u64,
    /// Revision of user input the event was produced against. Zero is allowed
    /// only for run facts that predate the first user input.
    pub input_revision: u64,
    pub kind: AgentRunEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub source_envelope_ids: Vec<String>,
    #[serde(default)]
    pub result_envelope_ids: Vec<String>,
    pub created_at: String,
}

impl AgentRunEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        if self.schema_version != AGENT_RUN_EVENT_SCHEMA_VERSION {
            return Err(DynamicRunContractError::UnsupportedEventVersion(
                self.schema_version,
            ));
        }
        validate_id("event_id", &self.event_id)?;
        validate_id("run_id", &self.run_id)?;
        if self.event_seq == 0 {
            return Err(DynamicRunContractError::InvalidSequence);
        }
        if let Some(value) = &self.correlation_id {
            validate_id("correlation_id", value)?;
        }
        validate_unique_ids("source_envelope_ids", &self.source_envelope_ids)?;
        validate_unique_ids("result_envelope_ids", &self.result_envelope_ids)?;
        validate_text("created_at", &self.created_at, MAX_RUN_EVENT_IDS)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskState {
    Running,
    CancelRequested,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

impl BackgroundTaskState {
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::OutcomeUnknown
        )
    }

    pub const fn from_completion(completion: CapabilityCompletionClass) -> Self {
        match completion {
            CapabilityCompletionClass::Succeeded => Self::Succeeded,
            CapabilityCompletionClass::Failed => Self::Failed,
            CapabilityCompletionClass::Cancelled => Self::Cancelled,
            CapabilityCompletionClass::OutcomeUnknown => Self::OutcomeUnknown,
        }
    }
}

/// Durable projection for one provider execution that returned Accepted. The
/// provider call identity does not change when Adaptive crosses its foreground
/// budget; there is exactly one task/call/generation and therefore no second
/// authorization reservation or dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskRecord {
    pub schema_version: u16,
    pub task: CapabilityTaskRef,
    pub turn_id: String,
    pub tool_name: String,
    pub canonical_input_digest_sha256: String,
    pub effect: CapabilityEffect,
    pub execution_policy: ExecutionPolicy,
    pub supports_cancel: bool,
    pub state: BackgroundTaskState,
    pub progress_sequence: u64,
    pub started_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_request_id: Option<String>,
    #[serde(default)]
    pub result_envelope_ids: Vec<String>,
}

impl BackgroundTaskRecord {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        if self.schema_version != BACKGROUND_TASK_SCHEMA_VERSION {
            return Err(DynamicRunContractError::UnsupportedBackgroundTaskVersion(
                self.schema_version,
            ));
        }
        self.task
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        validate_id("tool_name", &self.tool_name)?;
        validate_id("turn_id", &self.turn_id)?;
        if self.canonical_input_digest_sha256.len() != 64
            || !self
                .canonical_input_digest_sha256
                .bytes()
                .all(|value| value.is_ascii_hexdigit())
        {
            return Err(DynamicRunContractError::InvalidCanonicalInputDigest);
        }
        if matches!(self.execution_policy, ExecutionPolicy::InlineOnly) {
            return Err(DynamicRunContractError::InlineTaskCannotBeBackground);
        }
        validate_text("started_at", &self.started_at, MAX_RUN_EVENT_IDS)?;
        validate_text("updated_at", &self.updated_at, MAX_RUN_EVENT_IDS)?;
        if self.state.is_terminal() != self.terminal_at.is_some() {
            return Err(DynamicRunContractError::InvalidBackgroundTerminalState);
        }
        if self.state == BackgroundTaskState::CancelRequested && self.cancel_request_id.is_none() {
            return Err(DynamicRunContractError::InvalidBackgroundCancelState);
        }
        if let Some(request_id) = &self.cancel_request_id {
            validate_id("cancel_request_id", request_id)?;
        }
        if let Some(terminal_at) = &self.terminal_at {
            validate_text("terminal_at", terminal_at, MAX_RUN_EVENT_IDS)?;
        }
        validate_unique_ids("result_envelope_ids", &self.result_envelope_ids)
    }

    pub fn apply_progress(
        &mut self,
        progress: &CapabilityProgressEvent,
        updated_at: String,
    ) -> Result<(), DynamicRunContractError> {
        self.validate()?;
        progress
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        if self.state.is_terminal()
            || progress.task != self.task
            || progress.sequence <= self.progress_sequence
        {
            return Err(DynamicRunContractError::StaleBackgroundEvent);
        }
        validate_text("updated_at", &updated_at, MAX_RUN_EVENT_IDS)?;
        self.progress_sequence = progress.sequence;
        self.updated_at = updated_at;
        Ok(())
    }

    pub fn apply_completion(
        &mut self,
        completion: &CapabilityCompletionEvent,
        terminal_at: String,
    ) -> Result<(), DynamicRunContractError> {
        self.validate()?;
        completion
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        if self.state.is_terminal()
            || completion.task != self.task
            || completion.sequence <= self.progress_sequence
        {
            return Err(DynamicRunContractError::StaleBackgroundEvent);
        }
        validate_text("terminal_at", &terminal_at, MAX_RUN_EVENT_IDS)?;
        self.progress_sequence = completion.sequence;
        self.state = BackgroundTaskState::from_completion(completion.completion);
        self.updated_at = terminal_at.clone();
        self.terminal_at = Some(terminal_at);
        self.result_envelope_ids = completion.result_envelope_ids.clone();
        Ok(())
    }

    pub fn apply_cancel_request(
        &mut self,
        request: &CapabilityCancelRequest,
        updated_at: String,
    ) -> Result<(), DynamicRunContractError> {
        self.validate()?;
        request
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        if !self.supports_cancel {
            return Err(DynamicRunContractError::BackgroundCancelUnsupported);
        }
        if self.state.is_terminal() || request.task != self.task {
            return Err(DynamicRunContractError::StaleBackgroundEvent);
        }
        if self.state == BackgroundTaskState::CancelRequested {
            return if self.cancel_request_id.as_deref() == Some(request.request_id.as_str()) {
                Ok(())
            } else {
                Err(DynamicRunContractError::StaleBackgroundEvent)
            };
        }
        validate_text("updated_at", &updated_at, MAX_RUN_EVENT_IDS)?;
        self.state = BackgroundTaskState::CancelRequested;
        self.cancel_request_id = Some(request.request_id.clone());
        self.updated_at = updated_at;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundProgressRunEvent {
    pub event: AgentRunEvent,
    pub progress: CapabilityProgressEvent,
}

impl BackgroundProgressRunEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        self.progress
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        if self.event.kind != AgentRunEventKind::BackgroundProgress
            || self.event.run_id != self.progress.task.run_id
            || self.event.input_revision != self.progress.task.input_revision
            || self.event.correlation_id.as_deref() != Some(self.progress.task.task_id.as_str())
            || !self.event.result_envelope_ids.is_empty()
        {
            return Err(DynamicRunContractError::WrongEventKind);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundCompletionRunEvent {
    pub event: AgentRunEvent,
    pub completion: CapabilityCompletionEvent,
}

impl BackgroundCompletionRunEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        self.completion
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        if self.event.kind != AgentRunEventKind::BackgroundCompletion
            || self.event.run_id != self.completion.task.run_id
            || self.event.input_revision != self.completion.task.input_revision
            || self.event.correlation_id.as_deref() != Some(self.completion.task.task_id.as_str())
            || self.event.result_envelope_ids != self.completion.result_envelope_ids
        {
            return Err(DynamicRunContractError::WrongEventKind);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundCancelRequestedRunEvent {
    pub event: AgentRunEvent,
    pub request: CapabilityCancelRequest,
}

/// Durable acknowledgement that the exact, stable cancel request reached the
/// Provider runtime. This does not claim that execution stopped; only a later
/// Provider completion may move the task to `Cancelled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundCancelDeliveredRunEvent {
    pub event: AgentRunEvent,
    pub request: CapabilityCancelRequest,
}

impl BackgroundCancelDeliveredRunEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        self.request
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        if self.event.kind != AgentRunEventKind::CancelDelivered
            || self.event.run_id != self.request.task.run_id
            || self.event.input_revision != self.request.task.input_revision
            || self.event.correlation_id.as_deref() != Some(self.request.task.task_id.as_str())
            || !self.event.source_envelope_ids.is_empty()
            || !self.event.result_envelope_ids.is_empty()
        {
            return Err(DynamicRunContractError::WrongEventKind);
        }
        Ok(())
    }
}

impl BackgroundCancelRequestedRunEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        self.request
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidBackgroundTask(error.to_string()))?;
        if self.event.kind != AgentRunEventKind::CancelRequested
            || self.event.run_id != self.request.task.run_id
            || self.event.input_revision != self.request.task.input_revision
            || self.event.correlation_id.as_deref() != Some(self.request.task.task_id.as_str())
        {
            return Err(DynamicRunContractError::WrongEventKind);
        }
        Ok(())
    }
}

/// The durable acknowledgement unit for a user message. The actual text is
/// persisted in the session conversation; the ledger carries its validated
/// DataEnvelope and ordered revision metadata, not another unbounded copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFollowupEvent {
    pub event: AgentRunEvent,
    pub actor_id: String,
    pub input_seq: u64,
    pub message_id: String,
    pub message_envelope: DataEnvelope,
}

/// Append-only evidence for a model-maintained task-status projection update.
/// The projection remains advisory UX state and carries no grant or execution
/// authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusUpdatedEvent {
    pub event: AgentRunEvent,
    pub projection: TaskStatusProjection,
}

impl TaskStatusUpdatedEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        if self.event.kind != AgentRunEventKind::TaskStatusUpdated {
            return Err(DynamicRunContractError::WrongEventKind);
        }
        self.projection.validate()?;
        if self.event.input_revision == 0 {
            return Err(DynamicRunContractError::InvalidRevision);
        }
        Ok(())
    }
}

impl UserFollowupEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        if self.event.kind != AgentRunEventKind::UserFollowup {
            return Err(DynamicRunContractError::WrongEventKind);
        }
        if self.input_seq == 0 || self.event.input_revision == 0 {
            return Err(DynamicRunContractError::InvalidRevision);
        }
        validate_id("actor_id", &self.actor_id)?;
        validate_id("message_id", &self.message_id)?;
        self.message_envelope
            .validate()
            .map_err(|error| DynamicRunContractError::InvalidEnvelope(error.to_string()))?;
        if !self
            .event
            .source_envelope_ids
            .iter()
            .any(|id| id == &self.message_envelope.envelope_id)
        {
            return Err(DynamicRunContractError::MissingMessageEnvelope);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRequestState {
    Pending,
    NeedsRevalidation,
    Approved,
    PartiallyApproved,
    Denied,
    Replaced,
    Withdrawn,
}

impl PermissionRequestState {
    pub const fn can_user_decide(self) -> bool {
        matches!(self, Self::Pending)
    }

    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Approved
                | Self::PartiallyApproved
                | Self::Denied
                | Self::Replaced
                | Self::Withdrawn
        )
    }
}

/// One model-proposed permission item after server normalization. This record is
/// only a request shown to the user; it is deliberately not a grant, reservation,
/// or dispatch authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRequestItem {
    pub item_id: String,
    pub provider_id: String,
    pub tool_name: String,
    pub expected_effect: CapabilityEffect,
    /// Opaque, bounded resource selectors normalized by the server. Providers
    /// interpret only their own reviewed selector vocabulary.
    #[serde(default)]
    pub resource_scope: Vec<String>,
    /// Opaque, bounded operation selectors (for example `create_file`).
    #[serde(default)]
    pub operation_scope: Vec<String>,
    /// Explicit model/web/mail/chat destinations requested for ExportData.
    #[serde(default)]
    pub export_destinations: Vec<DestinationIdentity>,
    /// Server-canonicalized exact call input whenever the Provider contract
    /// binds authority to immutable arguments. This includes one-shot
    /// high-risk calls and lower-risk browser navigation to an exact URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_input_json: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_input_digest_sha256: Option<String>,
    pub suggested_ttl_seconds: u32,
    pub suggested_max_uses: u32,
    pub reason: String,
}

impl GrantRequestItem {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        validate_id("permission_item_id", &self.item_id)?;
        validate_id("permission_provider_id", &self.provider_id)?;
        validate_id("permission_tool_name", &self.tool_name)?;
        validate_scope_values("resource_scope", &self.resource_scope)?;
        validate_scope_values("operation_scope", &self.operation_scope)?;
        if self.export_destinations.len() > MAX_PERMISSION_SCOPE_VALUES {
            return Err(DynamicRunContractError::TooManyPermissionScopeValues(
                "export_destinations",
            ));
        }
        for destination in &self.export_destinations {
            destination
                .validate()
                .map_err(|error| DynamicRunContractError::InvalidDestination(error.to_string()))?;
        }
        match (
            self.canonical_input_json.as_deref(),
            self.canonical_input_digest_sha256.as_deref(),
        ) {
            (None, None) => {}
            (Some(input), Some(digest))
                if !input.is_empty()
                    && input.len() <= MAX_PERMISSION_EXACT_INPUT_BYTES
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    && format!("{:x}", Sha256::digest(input.as_bytes())) == digest => {}
            _ => return Err(DynamicRunContractError::InvalidCanonicalPermissionInput),
        }
        if self.suggested_ttl_seconds == 0 || self.suggested_max_uses == 0 {
            return Err(DynamicRunContractError::InvalidPermissionLimit);
        }
        validate_text(
            "permission_reason",
            &self.reason,
            MAX_PERMISSION_REASON_BYTES,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub input_revision: u64,
    pub state: PermissionRequestState,
    pub items: Vec<GrantRequestItem>,
    pub created_at: String,
}

impl PermissionRequest {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        if self.schema_version != PERMISSION_REQUEST_SCHEMA_VERSION {
            return Err(DynamicRunContractError::UnsupportedPermissionVersion(
                self.schema_version,
            ));
        }
        validate_id("permission_request_id", &self.request_id)?;
        if self.input_revision == 0 {
            return Err(DynamicRunContractError::InvalidRevision);
        }
        validate_text("permission_created_at", &self.created_at, MAX_RUN_EVENT_IDS)?;
        if self.items.is_empty() || self.items.len() > MAX_PERMISSION_REQUEST_ITEMS {
            return Err(DynamicRunContractError::InvalidPermissionItemCount);
        }
        let mut ids = BTreeSet::new();
        for item in &self.items {
            item.validate()?;
            if !ids.insert(item.item_id.as_str()) {
                return Err(DynamicRunContractError::DuplicatePermissionItem);
            }
        }
        Ok(())
    }

    /// A newer user requirement invalidates approval of an unresolved request.
    /// The model must explicitly replace, withdraw, or re-request it against the
    /// new revision; the UI must not allow a stale request to be approved.
    pub fn require_revalidation(&mut self, current_input_revision: u64) -> bool {
        if self.state == PermissionRequestState::Pending
            && current_input_revision > self.input_revision
        {
            self.state = PermissionRequestState::NeedsRevalidation;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequestedEvent {
    pub event: AgentRunEvent,
    pub request: PermissionRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionItemDecision {
    Approve {
        #[serde(default)]
        resource_scope: Vec<String>,
        #[serde(default)]
        operation_scope: Vec<String>,
        #[serde(default)]
        export_destinations: Vec<DestinationIdentity>,
        ttl_seconds: u32,
        max_uses: u32,
    },
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecisionItem {
    pub item_id: String,
    #[serde(flatten)]
    pub decision: PermissionItemDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecidedEvent {
    pub event: AgentRunEvent,
    pub request_id: String,
    pub request_input_revision: u64,
    pub resulting_state: PermissionRequestState,
    pub items: Vec<PermissionDecisionItem>,
}

impl PermissionDecidedEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        if self.event.kind != AgentRunEventKind::PermissionDecided
            || self.event.correlation_id.as_deref() != Some(self.request_id.as_str())
            || self.event.input_revision != self.request_input_revision
            || !matches!(
                self.resulting_state,
                PermissionRequestState::Approved
                    | PermissionRequestState::PartiallyApproved
                    | PermissionRequestState::Denied
            )
        {
            return Err(DynamicRunContractError::PermissionEventMismatch);
        }
        validate_id("permission_request_id", &self.request_id)?;
        if self.items.is_empty() || self.items.len() > MAX_PERMISSION_REQUEST_ITEMS {
            return Err(DynamicRunContractError::InvalidPermissionItemCount);
        }
        let mut ids = BTreeSet::new();
        for item in &self.items {
            validate_id("permission_item_id", &item.item_id)?;
            if !ids.insert(item.item_id.as_str()) {
                return Err(DynamicRunContractError::DuplicatePermissionItem);
            }
            if let PermissionItemDecision::Approve {
                resource_scope,
                operation_scope,
                export_destinations,
                ttl_seconds,
                max_uses,
            } = &item.decision
            {
                validate_scope_values("resource_scope", resource_scope)?;
                validate_scope_values("operation_scope", operation_scope)?;
                if export_destinations.len() > MAX_PERMISSION_SCOPE_VALUES {
                    return Err(DynamicRunContractError::TooManyPermissionScopeValues(
                        "export_destinations",
                    ));
                }
                for destination in export_destinations {
                    destination.validate().map_err(|error| {
                        DynamicRunContractError::InvalidDestination(error.to_string())
                    })?;
                }
                if *ttl_seconds == 0 || *max_uses == 0 {
                    return Err(DynamicRunContractError::InvalidPermissionLimit);
                }
            }
        }
        Ok(())
    }
}

impl PermissionRequest {
    /// Validate a complete user decision against the normalized request, then
    /// update only this request projection. Approved scope must be a subset and
    /// limits may only narrow. This still does not mint a grant.
    pub fn apply_user_decision(
        &mut self,
        items: &[PermissionDecisionItem],
    ) -> Result<PermissionRequestState, DynamicRunContractError> {
        if self.state != PermissionRequestState::Pending || items.len() != self.items.len() {
            return Err(DynamicRunContractError::PermissionNotDecidable);
        }
        let decisions = items
            .iter()
            .map(|item| (item.item_id.as_str(), &item.decision))
            .collect::<std::collections::BTreeMap<_, _>>();
        if decisions.len() != items.len() {
            return Err(DynamicRunContractError::DuplicatePermissionItem);
        }
        let mut approved = 0usize;
        for requested in &self.items {
            let decision = decisions
                .get(requested.item_id.as_str())
                .ok_or(DynamicRunContractError::PermissionDecisionIncomplete)?;
            if let PermissionItemDecision::Approve {
                resource_scope,
                operation_scope,
                export_destinations,
                ttl_seconds,
                max_uses,
            } = decision
            {
                if !is_subset(resource_scope, &requested.resource_scope)
                    || !is_subset(operation_scope, &requested.operation_scope)
                    || !export_destinations
                        .iter()
                        .all(|destination| requested.export_destinations.contains(destination))
                    || *ttl_seconds > requested.suggested_ttl_seconds
                    || *max_uses > requested.suggested_max_uses
                    || *ttl_seconds == 0
                    || *max_uses == 0
                {
                    return Err(DynamicRunContractError::PermissionDecisionWidensScope);
                }
                approved += 1;
            }
        }
        self.state = if approved == 0 {
            PermissionRequestState::Denied
        } else if approved == self.items.len() {
            PermissionRequestState::Approved
        } else {
            PermissionRequestState::PartiallyApproved
        };
        Ok(self.state)
    }
}

impl PermissionRequestedEvent {
    pub fn validate(&self) -> Result<(), DynamicRunContractError> {
        self.event.validate()?;
        self.request.validate()?;
        if self.event.kind != AgentRunEventKind::PermissionRequested {
            return Err(DynamicRunContractError::WrongEventKind);
        }
        if self.event.input_revision != self.request.input_revision
            || self.event.correlation_id.as_deref() != Some(self.request.request_id.as_str())
        {
            return Err(DynamicRunContractError::PermissionEventMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynamicRunContractError {
    UnsupportedEventVersion(u16),
    UnsupportedProjectionVersion(u16),
    UnsupportedPermissionVersion(u16),
    UnsupportedBackgroundTaskVersion(u16),
    EmptyField(&'static str),
    OversizedField(&'static str),
    InvalidSequence,
    InvalidRevision,
    TooManyProjectionItems,
    DuplicateProjectionItem,
    InvalidPermissionItemCount,
    DuplicatePermissionItem,
    TooManyPermissionScopeValues(&'static str),
    DuplicatePermissionScopeValue(&'static str),
    InvalidPermissionLimit,
    InvalidCanonicalPermissionInput,
    InvalidDestination(String),
    PermissionEventMismatch,
    PermissionNotDecidable,
    PermissionDecisionIncomplete,
    PermissionDecisionWidensScope,
    InlineTaskCannotBeBackground,
    InvalidBackgroundTerminalState,
    InvalidBackgroundCancelState,
    InvalidBackgroundTask(String),
    InvalidCanonicalInputDigest,
    StaleBackgroundEvent,
    BackgroundCancelUnsupported,
    TooManyIds(&'static str),
    DuplicateId(&'static str),
    WrongEventKind,
    MissingMessageEnvelope,
    InvalidEnvelope(String),
}

impl fmt::Display for DynamicRunContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for DynamicRunContractError {}

fn validate_id(field: &'static str, value: &str) -> Result<(), DynamicRunContractError> {
    validate_text(field, value, MAX_RUN_EVENT_IDS)
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), DynamicRunContractError> {
    if value.trim().is_empty() {
        Err(DynamicRunContractError::EmptyField(field))
    } else if value.len() > max_bytes {
        Err(DynamicRunContractError::OversizedField(field))
    } else {
        Ok(())
    }
}

fn validate_unique_ids(
    field: &'static str,
    values: &[String],
) -> Result<(), DynamicRunContractError> {
    if values.len() > MAX_RUN_EVENT_IDS {
        return Err(DynamicRunContractError::TooManyIds(field));
    }
    let mut unique = BTreeSet::new();
    for value in values {
        validate_id(field, value)?;
        if !unique.insert(value.as_str()) {
            return Err(DynamicRunContractError::DuplicateId(field));
        }
    }
    Ok(())
}

fn validate_scope_values(
    field: &'static str,
    values: &[String],
) -> Result<(), DynamicRunContractError> {
    if values.len() > MAX_PERMISSION_SCOPE_VALUES {
        return Err(DynamicRunContractError::TooManyPermissionScopeValues(field));
    }
    let mut unique = BTreeSet::new();
    for value in values {
        validate_text(field, value, MAX_TASK_DESCRIPTION_BYTES)?;
        if !unique.insert(value.as_str()) {
            return Err(DynamicRunContractError::DuplicatePermissionScopeValue(
                field,
            ));
        }
    }
    Ok(())
}

fn is_subset(values: &[String], allowed: &[String]) -> bool {
    values.iter().all(|value| allowed.contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> TaskStatusProjection {
        TaskStatusProjection {
            schema_version: TASK_STATUS_PROJECTION_SCHEMA_VERSION,
            revision: 1,
            items: vec![TaskStatusItem {
                item_id: "collect-inputs".into(),
                description: "Collect the selected workbook inputs".into(),
                status: TaskStatus::InProgress,
                note: Some("AI assessment; user may correct it".into()),
                last_updated_step_id: "step-1".into(),
            }],
            updated_at: "2026-08-25T00:00:00Z".into(),
        }
    }

    #[test]
    fn task_projection_is_bounded_and_has_stable_unique_ids() {
        projection().validate().unwrap();
        let mut duplicate = projection();
        duplicate.items.push(duplicate.items[0].clone());
        assert_eq!(
            duplicate.validate(),
            Err(DynamicRunContractError::DuplicateProjectionItem)
        );
    }

    #[test]
    fn projection_is_not_an_authorization_or_execution_record() {
        let value = serde_json::to_value(projection()).unwrap();
        for forbidden in [
            "grant_id",
            "approval_id",
            "dispatch_intent",
            "execution_id",
            "artifact_bytes",
        ] {
            assert!(value.get(forbidden).is_none());
        }
    }

    #[test]
    fn event_requires_monotonic_sequence_and_closed_kind() {
        let event = AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: "event-1".into(),
            run_id: "run-1".into(),
            event_seq: 1,
            input_revision: 1,
            kind: AgentRunEventKind::ModelStep,
            correlation_id: Some("turn-1".into()),
            source_envelope_ids: vec!["input-1".into()],
            result_envelope_ids: Vec::new(),
            created_at: "2026-08-25T00:00:00Z".into(),
        };
        event.validate().unwrap();
        let encoded = serde_json::to_string(&event).unwrap();
        assert!(encoded.contains("model_step"));
        let unknown = encoded.replace("model_step", "arbitrary_future_effect");
        assert!(serde_json::from_str::<AgentRunEvent>(&unknown).is_err());
    }

    #[test]
    fn task_status_event_carries_projection_without_authority() {
        let update = TaskStatusUpdatedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "status-1".into(),
                run_id: "run-1".into(),
                event_seq: 2,
                input_revision: 1,
                kind: AgentRunEventKind::TaskStatusUpdated,
                correlation_id: Some("call-1".into()),
                source_envelope_ids: Vec::new(),
                result_envelope_ids: Vec::new(),
                created_at: "2026-08-25T00:00:00Z".into(),
            },
            projection: projection(),
        };
        update.validate().unwrap();
        let json = serde_json::to_value(update).unwrap();
        assert!(json.to_string().find("grant_id").is_none());
        assert!(json.to_string().find("execution_id").is_none());
    }

    fn permission_request() -> PermissionRequest {
        PermissionRequest {
            schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
            request_id: "permission-1".into(),
            input_revision: 2,
            state: PermissionRequestState::Pending,
            items: vec![GrantRequestItem {
                item_id: "write-report".into(),
                provider_id: "file.workspace".into(),
                tool_name: "create_report".into(),
                expected_effect: CapabilityEffect::WriteArtifact,
                resource_scope: vec!["directory:chosen-output".into()],
                operation_scope: vec!["create_file".into()],
                export_destinations: Vec::new(),
                canonical_input_json: None,
                canonical_input_digest_sha256: None,
                suggested_ttl_seconds: 900,
                suggested_max_uses: 1,
                reason: "Create the report requested by the user".into(),
            }],
            created_at: "2026-08-25T00:00:00Z".into(),
        }
    }

    #[test]
    fn permission_request_is_bounded_and_never_contains_a_grant() {
        let request = permission_request();
        request.validate().unwrap();
        let json = serde_json::to_value(request).unwrap().to_string();
        for forbidden in ["grant_id", "reservation", "dispatch_intent", "execution_id"] {
            assert!(!json.contains(forbidden));
        }
    }

    #[test]
    fn newer_input_moves_only_pending_permission_to_needs_revalidation() {
        let mut request = permission_request();
        assert!(request.require_revalidation(3));
        assert_eq!(request.state, PermissionRequestState::NeedsRevalidation);
        assert!(!request.state.can_user_decide());
        assert!(!request.require_revalidation(4));

        let mut decided = permission_request();
        decided.state = PermissionRequestState::Denied;
        assert!(!decided.require_revalidation(3));
        assert!(decided.state.is_terminal());
    }

    #[test]
    fn permission_event_binds_request_to_input_revision() {
        let request = permission_request();
        let update = PermissionRequestedEvent {
            event: AgentRunEvent {
                schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
                event_id: "permission-event-1".into(),
                run_id: "run-1".into(),
                event_seq: 4,
                input_revision: request.input_revision,
                kind: AgentRunEventKind::PermissionRequested,
                correlation_id: Some(request.request_id.clone()),
                source_envelope_ids: vec!["model-output-1".into()],
                result_envelope_ids: Vec::new(),
                created_at: "2026-08-25T00:00:00Z".into(),
            },
            request,
        };
        update.validate().unwrap();
        let mut mismatched = update;
        mismatched.request.input_revision += 1;
        assert_eq!(
            mismatched.validate(),
            Err(DynamicRunContractError::PermissionEventMismatch)
        );
    }

    #[test]
    fn user_decision_can_only_narrow_and_is_not_a_grant() {
        let mut request = permission_request();
        let decisions = vec![PermissionDecisionItem {
            item_id: "write-report".into(),
            decision: PermissionItemDecision::Approve {
                resource_scope: vec!["directory:chosen-output".into()],
                operation_scope: vec!["create_file".into()],
                export_destinations: Vec::new(),
                ttl_seconds: 300,
                max_uses: 1,
            },
        }];
        assert_eq!(
            request.apply_user_decision(&decisions).unwrap(),
            PermissionRequestState::Approved
        );

        let mut widening = permission_request();
        let mut bad = decisions;
        if let PermissionItemDecision::Approve { resource_scope, .. } = &mut bad[0].decision {
            resource_scope.push("directory:anywhere".into());
        }
        assert_eq!(
            widening.apply_user_decision(&bad),
            Err(DynamicRunContractError::PermissionDecisionWidensScope)
        );
        let json = serde_json::to_value(&request).unwrap().to_string();
        assert!(!json.contains("grant_id"));
    }

    #[test]
    fn adaptive_background_task_keeps_one_call_and_rejects_stale_events() {
        let task = CapabilityTaskRef {
            task_id: "task-1".into(),
            call_id: "call-1".into(),
            run_id: "run-1".into(),
            provider_id: "file.workspace".into(),
            capability_id: "file.report.create".into(),
            input_revision: 2,
            generation: 1,
        };
        let mut record = BackgroundTaskRecord {
            schema_version: BACKGROUND_TASK_SCHEMA_VERSION,
            task: task.clone(),
            turn_id: "turn-1".into(),
            tool_name: "create_report".into(),
            canonical_input_digest_sha256:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            effect: CapabilityEffect::WriteArtifact,
            execution_policy: ExecutionPolicy::Adaptive {
                foreground_budget_ms: 5_000,
            },
            supports_cancel: true,
            state: BackgroundTaskState::Running,
            progress_sequence: 0,
            started_at: "2026-08-26T00:00:00Z".into(),
            updated_at: "2026-08-26T00:00:00Z".into(),
            terminal_at: None,
            cancel_request_id: None,
            result_envelope_ids: Vec::new(),
        };
        record.validate().unwrap();
        let progress = CapabilityProgressEvent {
            task: task.clone(),
            sequence: 1,
            completed_units: Some(1),
            total_units: Some(2),
            message_key: Some("report.progress".into()),
        };
        record
            .apply_progress(&progress, "2026-08-26T00:00:01Z".into())
            .unwrap();
        assert_eq!(record.progress_sequence, 1);
        assert_eq!(
            record.apply_progress(&progress, "2026-08-26T00:00:02Z".into()),
            Err(DynamicRunContractError::StaleBackgroundEvent)
        );
        let completion = CapabilityCompletionEvent {
            task,
            sequence: 2,
            completion: CapabilityCompletionClass::Succeeded,
            result_envelope_ids: vec!["result-envelope-1".into()],
        };
        record
            .apply_completion(&completion, "2026-08-26T00:00:03Z".into())
            .unwrap();
        assert_eq!(record.state, BackgroundTaskState::Succeeded);
        assert_eq!(record.task.call_id, "call-1");
        assert_eq!(record.task.generation, 1);
        assert_eq!(record.result_envelope_ids, vec!["result-envelope-1"]);
    }
}
