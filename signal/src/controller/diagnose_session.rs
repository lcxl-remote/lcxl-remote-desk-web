//! Browser-facing read of an OSS Signal AI-diagnose conversation.
//!
//! The browser supplies the same target connection and conversation intent used
//! by the signaling turn. The server resolves the target's authenticated client
//! id and re-derives the subject-namespaced conversation key, so a caller cannot
//! select an arbitrary SQLite row.

use actix_web::{HttpResponse, get, post, web};
use desk_diagnose_core::chat::ChatMessage;
use desk_diagnose_core::context_attachment::{
    AttachmentStaleReason, AttachmentState, ContextAttachment, ContextAttachmentKind,
};
use desk_diagnose_core::conversation_key::derive_conversation_key;
use desk_diagnose_core::{
    capability_availability::project_capability_availability,
    device_assistant::{device_assistant_provider_registry, provider_readiness_reports},
};
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::agent_session_store::{PermissionGrantIssuanceContext, SignalAgentSessionStore};
use crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
use crate::error::DeskSignalError;

pub const TAG: &str = "DiagnoseSession";

#[derive(Debug, Deserialize, ToSchema)]
pub struct DiagnoseSessionQuery {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DiagnoseSessionListQuery {
    pub connection: String,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDecisionBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub request_id: String,
    pub items: Vec<PermissionDecisionItemBody>,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundCancelBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub task_id: String,
    /// Stable client-generated id used to make a repeated submission idempotent.
    pub request_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityGrantRevokeBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub grant_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDecisionItemBody {
    pub item_id: String,
    #[serde(flatten)]
    pub decision: PermissionItemDecisionBody,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionItemDecisionBody {
    Approve {
        #[serde(default)]
        resource_scope: Vec<String>,
        #[serde(default)]
        operation_scope: Vec<String>,
        #[serde(default)]
        export_destinations: Vec<desk_agent_protocol::data_lineage::DestinationIdentity>,
        ttl_seconds: u32,
        max_uses: u32,
    },
    Deny,
}

impl From<PermissionDecisionItemBody> for desk_diagnose_core::dynamic_run::PermissionDecisionItem {
    fn from(value: PermissionDecisionItemBody) -> Self {
        use desk_diagnose_core::dynamic_run::PermissionItemDecision;
        Self {
            item_id: value.item_id,
            decision: match value.decision {
                PermissionItemDecisionBody::Approve {
                    resource_scope,
                    operation_scope,
                    export_destinations,
                    ttl_seconds,
                    max_uses,
                } => PermissionItemDecision::Approve {
                    resource_scope,
                    operation_scope,
                    export_destinations,
                    ttl_seconds,
                    max_uses,
                },
                PermissionItemDecisionBody::Deny => PermissionItemDecision::Deny,
            },
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDecisionResponse {
    pub state: PermissionRequestStateDto,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotToolCallDto {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMessageDto {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<SnapshotToolCallDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Server-issued id that correlates a delayed background completion with
    /// the task originally returned by `exec_command`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_task_id: Option<String>,
}

impl From<ChatMessage> for SnapshotMessageDto {
    fn from(message: ChatMessage) -> Self {
        Self {
            id: message.message_id,
            turn_id: message.turn_id,
            role: message.role.as_str().to_string(),
            text: message.text,
            tool_calls: message
                .tool_calls
                .into_iter()
                .map(|call| SnapshotToolCallDto {
                    id: call.id,
                    name: call.name,
                    arguments_json: call.arguments_json,
                })
                .collect(),
            tool_call_id: message.tool_call_id,
            background_task_id: message.background_task_id,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextNoticeKindDto {
    Trimmed,
    Compacted,
}

impl From<desk_diagnose_core::model_context::ContextNoticeKind> for ContextNoticeKindDto {
    fn from(kind: desk_diagnose_core::model_context::ContextNoticeKind) -> Self {
        match kind {
            desk_diagnose_core::model_context::ContextNoticeKind::Trimmed => Self::Trimmed,
            desk_diagnose_core::model_context::ContextNoticeKind::Compacted => Self::Compacted,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextNoticeDto {
    pub id: String,
    pub turn_id: String,
    pub kind: ContextNoticeKindDto,
    pub checkpoint_generation: Option<u32>,
    pub covered_message_count: Option<u32>,
}

impl From<desk_diagnose_core::model_context::ContextNotice> for ContextNoticeDto {
    fn from(notice: desk_diagnose_core::model_context::ContextNotice) -> Self {
        Self {
            id: notice.id,
            turn_id: notice.turn_id,
            kind: notice.kind.into(),
            checkpoint_generation: notice.checkpoint_generation,
            covered_message_count: notice.covered_message_count,
        }
    }
}

/// Browser-safe attachment metadata. Opaque tokens, envelope digests and model
/// destination identities remain server-side; the selector only needs enough
/// information to show provenance, expiry and whether a refresh is required.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextAttachmentDto {
    pub id: String,
    pub kind: String,
    pub provider_id: String,
    pub capability_id: String,
    pub display_summary: String,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: u64,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
}

impl From<ContextAttachment> for ContextAttachmentDto {
    fn from(attachment: ContextAttachment) -> Self {
        let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        Self::from_at(attachment, now_unix_ms)
    }
}

impl ContextAttachmentDto {
    fn from_at(attachment: ContextAttachment, now_unix_ms: u64) -> Self {
        let effective_state = if matches!(&attachment.state, AttachmentState::Active)
            && attachment.expires_at_unix_ms <= now_unix_ms
        {
            AttachmentState::Stale {
                reason: AttachmentStaleReason::Expired,
            }
        } else {
            attachment.state.clone()
        };
        let (state, stale_reason) = match effective_state {
            AttachmentState::Active => ("active".to_string(), None),
            AttachmentState::Stale { reason } => (
                "stale".to_string(),
                Some(stale_reason_name(reason).to_string()),
            ),
        };
        Self {
            id: attachment.attachment_id,
            kind: attachment_kind_name(attachment.kind).to_string(),
            provider_id: attachment.object_ref.source_provider_id,
            capability_id: attachment.object_ref.source_capability_id,
            display_summary: attachment.display_summary,
            created_at_unix_ms: attachment.created_at_unix_ms,
            expires_at_unix_ms: attachment.expires_at_unix_ms,
            state,
            stale_reason,
        }
    }
}

fn attachment_kind_name(kind: ContextAttachmentKind) -> &'static str {
    match kind {
        ContextAttachmentKind::Device => "device",
        ContextAttachmentKind::InteractiveSession => "interactive_session",
        ContextAttachmentKind::ActiveApplication => "active_application",
        ContextAttachmentKind::UiSelection => "ui_selection",
        ContextAttachmentKind::OfficeDocument => "office_document",
        ContextAttachmentKind::Worksheet => "worksheet",
        ContextAttachmentKind::Range => "range",
        ContextAttachmentKind::File => "file",
        ContextAttachmentKind::DirectorySelection => "directory_selection",
        ContextAttachmentKind::TerminalSessionRef => "terminal_session_ref",
        ContextAttachmentKind::CurrentScreen => "current_screen",
        ContextAttachmentKind::ExternalSourceSet => "external_source_set",
    }
}

fn stale_reason_name(reason: AttachmentStaleReason) -> &'static str {
    match reason {
        AttachmentStaleReason::Expired => "expired",
        AttachmentStaleReason::Detached => "detached",
        AttachmentStaleReason::WorkerRespawned => "worker_respawned",
        AttachmentStaleReason::SessionChanged => "session_changed",
        AttachmentStaleReason::DocumentChanged => "document_changed",
        AttachmentStaleReason::ObjectChanged => "object_changed",
        AttachmentStaleReason::CapabilityUnavailable => "capability_unavailable",
        AttachmentStaleReason::PolicyNarrowed => "policy_narrowed",
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseSessionSnapshotDto {
    pub seq: i64,
    /// Whether the persisted turn is still running or awaiting approval.
    pub active: bool,
    /// Diagnose request currently represented by the persisted turn.
    pub request_id: Option<String>,
    /// Running background command generation that may be cancelled.
    pub active_execution_generation: Option<String>,
    /// Latest durably accepted user input and the model-processing watermark.
    pub latest_input_seq: u64,
    pub input_revision: u64,
    pub handled_input_seq: u64,
    /// AI-maintained UX projection; never an authorization or execution fact.
    pub task_status_projection: Option<TaskStatusProjectionDto>,
    /// Model-proposed, server-normalized requests. Pending is approvable;
    /// NeedsRevalidation is display-only until the model replaces/reissues it.
    pub permission_requests: Vec<PermissionRequestDto>,
    /// Durable Provider executions that outlived their foreground wait.
    pub background_tasks: Vec<BackgroundTaskDto>,
    /// Server-issued authority metadata. It never proves a call was dispatched.
    pub capability_grants: Vec<CapabilityGrantDto>,
    pub messages: Vec<SnapshotMessageDto>,
    /// Durable transcript metadata for context-window changes. No omitted text is exposed.
    pub context_notices: Vec<ContextNoticeDto>,
    /// Durable selection metadata only; no UI tree, cells, files or screenshots.
    pub context_attachments: Vec<ContextAttachmentDto>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityGrantDto {
    pub grant_id: String,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub risk_tier: String,
    pub resource_scope: Vec<String>,
    pub operation_scope: Vec<String>,
    pub remaining_uses: u32,
    pub expires_at_unix_ms: u64,
    pub revoked_at_unix_ms: Option<u64>,
    pub revoked_reason: Option<String>,
}

impl From<desk_agent_protocol::capability_grant::CapabilityGrant> for CapabilityGrantDto {
    fn from(grant: desk_agent_protocol::capability_grant::CapabilityGrant) -> Self {
        Self {
            grant_id: grant.grant_id,
            provider_id: grant.provider_id,
            capability_id: grant.capability_id,
            tool_name: grant.tool_name,
            risk_tier: format!("{:?}", grant.risk_tier).to_ascii_lowercase(),
            resource_scope: grant.resource_scope,
            operation_scope: grant.operation_scope,
            remaining_uses: grant.remaining_uses,
            expires_at_unix_ms: grant.expires_at_unix_ms,
            revoked_at_unix_ms: grant.revoked_at_unix_ms,
            revoked_reason: grant.revoked_reason,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatusDto {
    Todo,
    InProgress,
    Blocked,
    Done,
    Skipped,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusItemDto {
    pub item_id: String,
    pub description: String,
    pub status: TaskStatusDto,
    pub note: Option<String>,
    pub last_updated_step_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusProjectionDto {
    pub schema_version: u16,
    pub revision: u64,
    pub items: Vec<TaskStatusItemDto>,
    pub updated_at: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRequestStateDto {
    Pending,
    NeedsRevalidation,
    Approved,
    PartiallyApproved,
    Denied,
    Replaced,
    Withdrawn,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskStateDto {
    Running,
    CancelRequested,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskDto {
    pub task_id: String,
    pub call_id: String,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
    pub effect: desk_agent_protocol::capability_provider::CapabilityEffect,
    pub state: BackgroundTaskStateDto,
    pub progress_sequence: u64,
    pub supports_cancel: bool,
    pub updated_at: String,
    pub terminal_at: Option<String>,
}

impl From<desk_diagnose_core::dynamic_run::BackgroundTaskRecord> for BackgroundTaskDto {
    fn from(record: desk_diagnose_core::dynamic_run::BackgroundTaskRecord) -> Self {
        use desk_diagnose_core::dynamic_run::BackgroundTaskState;
        Self {
            task_id: record.task.task_id,
            call_id: record.task.call_id,
            provider_id: record.task.provider_id,
            capability_id: record.task.capability_id,
            tool_name: record.tool_name,
            effect: record.effect,
            state: match record.state {
                BackgroundTaskState::Running => BackgroundTaskStateDto::Running,
                BackgroundTaskState::CancelRequested => BackgroundTaskStateDto::CancelRequested,
                BackgroundTaskState::Succeeded => BackgroundTaskStateDto::Succeeded,
                BackgroundTaskState::Failed => BackgroundTaskStateDto::Failed,
                BackgroundTaskState::Cancelled => BackgroundTaskStateDto::Cancelled,
                BackgroundTaskState::OutcomeUnknown => BackgroundTaskStateDto::OutcomeUnknown,
            },
            progress_sequence: record.progress_sequence,
            supports_cancel: record.supports_cancel,
            updated_at: record.updated_at,
            terminal_at: record.terminal_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GrantRequestItemDto {
    pub item_id: String,
    pub provider_id: String,
    pub tool_name: String,
    pub expected_effect: desk_agent_protocol::capability_provider::CapabilityEffect,
    pub resource_scope: Vec<String>,
    pub operation_scope: Vec<String>,
    pub export_destinations: Vec<desk_agent_protocol::data_lineage::DestinationIdentity>,
    pub suggested_ttl_seconds: u32,
    pub suggested_max_uses: u32,
    pub reason: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestDto {
    pub schema_version: u16,
    pub request_id: String,
    pub input_revision: u64,
    pub state: PermissionRequestStateDto,
    pub items: Vec<GrantRequestItemDto>,
    pub created_at: String,
}

impl From<desk_diagnose_core::dynamic_run::TaskStatus> for TaskStatusDto {
    fn from(status: desk_diagnose_core::dynamic_run::TaskStatus) -> Self {
        use desk_diagnose_core::dynamic_run::TaskStatus;
        match status {
            TaskStatus::Todo => Self::Todo,
            TaskStatus::InProgress => Self::InProgress,
            TaskStatus::Blocked => Self::Blocked,
            TaskStatus::Done => Self::Done,
            TaskStatus::Skipped => Self::Skipped,
        }
    }
}

impl From<desk_diagnose_core::dynamic_run::TaskStatusProjection> for TaskStatusProjectionDto {
    fn from(projection: desk_diagnose_core::dynamic_run::TaskStatusProjection) -> Self {
        Self {
            schema_version: projection.schema_version,
            revision: projection.revision,
            items: projection
                .items
                .into_iter()
                .map(|item| TaskStatusItemDto {
                    item_id: item.item_id,
                    description: item.description,
                    status: item.status.into(),
                    note: item.note,
                    last_updated_step_id: item.last_updated_step_id,
                })
                .collect(),
            updated_at: projection.updated_at,
        }
    }
}

impl From<desk_diagnose_core::dynamic_run::PermissionRequestState> for PermissionRequestStateDto {
    fn from(state: desk_diagnose_core::dynamic_run::PermissionRequestState) -> Self {
        use desk_diagnose_core::dynamic_run::PermissionRequestState;
        match state {
            PermissionRequestState::Pending => Self::Pending,
            PermissionRequestState::NeedsRevalidation => Self::NeedsRevalidation,
            PermissionRequestState::Approved => Self::Approved,
            PermissionRequestState::PartiallyApproved => Self::PartiallyApproved,
            PermissionRequestState::Denied => Self::Denied,
            PermissionRequestState::Replaced => Self::Replaced,
            PermissionRequestState::Withdrawn => Self::Withdrawn,
        }
    }
}

impl From<desk_diagnose_core::dynamic_run::PermissionRequest> for PermissionRequestDto {
    fn from(request: desk_diagnose_core::dynamic_run::PermissionRequest) -> Self {
        Self {
            schema_version: request.schema_version,
            request_id: request.request_id,
            input_revision: request.input_revision,
            state: request.state.into(),
            items: request
                .items
                .into_iter()
                .map(|item| GrantRequestItemDto {
                    item_id: item.item_id,
                    provider_id: item.provider_id,
                    tool_name: item.tool_name,
                    expected_effect: item.expected_effect,
                    resource_scope: item.resource_scope,
                    operation_scope: item.operation_scope,
                    export_destinations: item.export_destinations,
                    suggested_ttl_seconds: item.suggested_ttl_seconds,
                    suggested_max_uses: item.suggested_max_uses,
                    reason: item.reason,
                })
                .collect(),
            created_at: request.created_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseSessionSummaryDto {
    pub session_id: String,
    pub conversation_id: Option<String>,
    pub first_question: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub active: bool,
    pub message_count: usize,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DiagnoseSessionListDto {
    pub sessions: Vec<DiagnoseSessionSummaryDto>,
}

fn not_accessible() -> HttpResponse {
    HttpResponse::Ok().json(RestResponse::<()>::failed(
        DeskErrorCode::PERMISSION_ERROR,
        "Diagnose session not found or not accessible".to_string(),
    ))
}

#[utoipa::path(
    tag = TAG,
    summary = "Read an AI-diagnose conversation snapshot (browser view)",
    params(
        ("connection" = String, Query, description = "Target connection id"),
        ("conversation" = Option<String>, Query, description = "Client conversation intent"),
        ("session" = Option<String>, Query, description = "Opaque session id from history list"),
    ),
    responses((status = 200, description = "Conversation snapshot, or a uniform \
        not-found/not-accessible response", body = RestResponse<DiagnoseSessionSnapshotDto>)),
)]
#[get("/my/diagnose-session")]
pub async fn get_diagnose_session(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<DiagnoseSessionQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&query.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };

    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let store = SignalAgentSessionStore::new(crate::db::get_db().clone());
    let session_id = match (
        query.session.as_deref().filter(|value| !value.is_empty()),
        query.conversation.as_deref(),
    ) {
        (Some(session_id), _) => session_id.to_string(),
        (None, Some(conversation)) => {
            derive_conversation_key(&actor_id, &target_audience, Some(conversation), "")
        }
        (None, None) => return Ok(not_accessible()),
    };
    let snapshot = store
        .read_snapshot_for_subject(&session_id, &actor_id, &target_audience)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    match snapshot {
        Some(snapshot) => {
            let background_tasks =
                crate::agent_background_task_store::SignalBackgroundTaskStore::new(
                    crate::db::get_db().clone(),
                )
                .list_for_run(&session_id)
                .await
                .map_err(|error| {
                    DeskSignalError::new_custom_error(
                        DeskErrorCode::SYSTEM_ERROR,
                        &format!("load background tasks: {error}"),
                    )
                })?;
            let capability_grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(
                crate::db::get_db().clone(),
            )
            .list_for_subject(&session_id, &actor_id, &target_audience)
            .await
            .map_err(|error| {
                DeskSignalError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("load capability grants: {error}"),
                )
            })?;
            Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
                DiagnoseSessionSnapshotDto {
                    seq: snapshot.seq,
                    active: snapshot.active,
                    request_id: snapshot.request_id,
                    active_execution_generation: snapshot.active_execution_generation,
                    latest_input_seq: snapshot.latest_input_seq,
                    input_revision: snapshot.input_revision,
                    handled_input_seq: snapshot.handled_input_seq,
                    task_status_projection: snapshot.task_status_projection.map(Into::into),
                    permission_requests: snapshot
                        .permission_requests
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    background_tasks: background_tasks.into_iter().map(Into::into).collect(),
                    capability_grants: capability_grants.into_iter().map(Into::into).collect(),
                    messages: snapshot.messages.into_iter().map(Into::into).collect(),
                    context_notices: snapshot
                        .context_notices
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    context_attachments: snapshot
                        .context_attachments
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                },
            )))
        }
        None => Ok(not_accessible()),
    }
}

#[utoipa::path(
    tag = TAG,
    summary = "Revoke one owner-scoped Device Assistant capability grant",
    request_body = CapabilityGrantRevokeBody,
    responses((status = 200, description = "Revoked grant metadata; no work is dispatched", body = RestResponse<CapabilityGrantDto>)),
)]
#[post("/my/diagnose-session/capability-grant/revoke")]
pub async fn revoke_diagnose_capability_grant(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<CapabilityGrantRevokeBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&body.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let session_id = match (
        body.session.as_deref().filter(|value| !value.is_empty()),
        body.conversation.as_deref(),
    ) {
        (Some(session_id), _) => session_id.to_string(),
        (None, Some(conversation)) => {
            derive_conversation_key(&actor_id, &target_audience, Some(conversation), "")
        }
        (None, None) => return Ok(not_accessible()),
    };
    let session = SignalAgentSessionStore::new(crate::db::get_db().clone())
        .read_snapshot_for_subject(&session_id, &actor_id, &target_audience)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    if session.is_none() {
        return Ok(not_accessible());
    }
    let now_unix_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "system clock predates Unix epoch",
        )
    })?;
    let grant =
        crate::capability_grant_store::SignalCapabilityGrantStore::new(crate::db::get_db().clone())
            .revoke(
                &body.grant_id,
                &actor_id,
                &target_audience,
                now_unix_ms,
                &body.reason,
            )
            .await
            .map_err(|error| {
                DeskSignalError::new_custom_error(
                    DeskErrorCode::PRECONDITION_FAILED,
                    &error.to_string(),
                )
            })?;
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(CapabilityGrantDto::from(
            grant,
        ))),
    )
}

#[utoipa::path(
    tag = TAG,
    summary = "Decide one pending Device Assistant permission request",
    request_body = PermissionDecisionBody,
    responses((status = 200, description = "Durable decision projection; no tool is dispatched", body = RestResponse<PermissionDecisionResponse>)),
)]
#[post("/my/diagnose-session/permission-decision")]
pub async fn decide_diagnose_permission(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<PermissionDecisionBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&body.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let store = SignalAgentSessionStore::new(crate::db::get_db().clone());
    let session_id = match (
        body.session.as_deref().filter(|value| !value.is_empty()),
        body.conversation.as_deref(),
    ) {
        (Some(session_id), _) => session_id.to_string(),
        (None, Some(conversation)) => {
            derive_conversation_key(&actor_id, &target_audience, Some(conversation), "")
        }
        (None, None) => return Ok(not_accessible()),
    };
    let now_dt = chrono::Utc::now();
    let now_unix_ms = u64::try_from(now_dt.timestamp_millis()).map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "system clock predates Unix epoch",
        )
    })?;
    let readiness = crate::computer_use_readiness::global_computer_use_readiness_cache()
        .get_fresh(&body.connection, now_dt)
        .ok_or_else(|| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "target capability readiness is unavailable; refresh and retry",
            )
        })?;
    let readiness_revision = readiness.readiness.revision;
    let reports = provider_readiness_reports(&readiness.readiness).map_err(|error| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            &format!("invalid target capability readiness: {error}"),
        )
    })?;
    let registry = device_assistant_provider_registry();
    let inventory = project_capability_availability(
        &registry,
        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
        now_unix_ms,
        reports,
    )
    .map_err(|error| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            &format!("invalid target capability inventory: {error}"),
        )
    })?;
    let now = now_dt.to_rfc3339();
    let state = store
        .decide_permission_request(
            &session_id,
            &actor_id,
            &target_audience,
            &body.request_id,
            body.items.clone().into_iter().map(Into::into).collect(),
            PermissionGrantIssuanceContext {
                surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision,
                now_unix_ms,
            },
            &now,
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &error.message)
        })?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        PermissionDecisionResponse {
            state: state.into(),
        },
    )))
}

#[utoipa::path(
    tag = TAG,
    summary = "Request cancellation of an authorized Device Assistant background task",
    request_body = BackgroundCancelBody,
    responses((status = 200, description = "Durable cancellation intent; Provider delivery is asynchronous", body = RestResponse<BackgroundTaskDto>)),
)]
#[post("/my/diagnose-session/background-task/cancel")]
pub async fn cancel_diagnose_background_task(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<BackgroundCancelBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&body.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let session_id = match (
        body.session.as_deref().filter(|value| !value.is_empty()),
        body.conversation.as_deref(),
    ) {
        (Some(session_id), _) => session_id.to_string(),
        (None, Some(conversation)) => {
            derive_conversation_key(&actor_id, &target_audience, Some(conversation), "")
        }
        (None, None) => return Ok(not_accessible()),
    };
    let store = crate::agent_background_task_store::SignalBackgroundTaskStore::new(
        crate::db::get_db().clone(),
    );
    store
        .request_cancel_for_subject(
            &body.task_id,
            &session_id,
            &actor_id,
            &target_audience,
            &body.request_id,
            &body.reason,
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                &error.to_string(),
            )
        })?;
    let record = store
        .load(&body.task_id)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
        .ok_or_else(|| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "background task was not found or not accessible",
            )
        })?;
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(BackgroundTaskDto::from(
            record,
        ))),
    )
}

#[utoipa::path(
    tag = TAG,
    summary = "List recent AI-diagnose conversations for a device",
    params(
        ("connection" = String, Query, description = "Target connection id"),
        ("limit" = Option<u64>, Query, description = "Maximum rows, default 30 and capped at 100"),
    ),
    responses((status = 200, description = "Authorized recent diagnose sessions",
        body = RestResponse<DiagnoseSessionListDto>)),
)]
#[get("/my/diagnose-sessions")]
pub async fn list_diagnose_sessions(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<DiagnoseSessionListQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let target_audience = {
        let map = connection_map.read().await;
        let Some(target) = map.get(&query.connection) else {
            return Ok(not_accessible());
        };
        if target.auth_context.auth_kind != AuthKind::TokenAuth
            || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
        {
            return Ok(not_accessible());
        }
        match target.model.version_info.client_id.as_deref() {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => return Ok(not_accessible()),
        }
    };
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let limit = query.limit.unwrap_or(30).clamp(1, 100);
    let summaries = SignalAgentSessionStore::new(crate::db::get_db().clone())
        .list_diagnose_sessions(&actor_id, &target_audience, limit)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    let sessions = summaries
        .into_iter()
        .map(|summary| DiagnoseSessionSummaryDto {
            session_id: summary.session_id,
            conversation_id: summary.client_conversation_id,
            first_question: summary.first_question,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            active: summary.active,
            message_count: summary.message_count,
        })
        .collect();
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(DiagnoseSessionListDto {
            sessions,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    };
    use desk_diagnose_core::chat::{ChatMessage, ChatRole, ToolCallRef};
    use desk_diagnose_core::context_attachment::{
        AttachmentBounds, AttachmentObjectRef, CONTEXT_ATTACHMENT_SCHEMA_VERSION,
    };
    use desk_diagnose_core::session::AgentSessionSurface;

    #[test]
    fn snapshot_message_uses_the_manager_compatible_camel_case_shape() {
        let mut message = ChatMessage::text("m1", ChatRole::Assistant, "checking");
        message.tool_calls.push(ToolCallRef {
            id: "c1".into(),
            name: "read_system_info".into(),
            arguments_json: "{}".into(),
        });
        message.image_data_url = Some("data:image/jpeg;base64,AQID".into());
        let value = serde_json::to_value(SnapshotMessageDto::from(message)).unwrap();
        assert_eq!(value["id"], "m1");
        assert_eq!(value["toolCalls"][0]["argumentsJson"], "{}");
        assert!(value.get("tool_calls").is_none());
        assert!(value.get("imageDataUrl").is_none());
        assert!(!value.to_string().contains("AQID"));

        let completion = ChatMessage::untrusted_output("done-1", "call-1", "task-1", "exit_code=0");
        let value = serde_json::to_value(SnapshotMessageDto::from(completion)).unwrap();
        assert_eq!(value["backgroundTaskId"], "task-1");
        assert!(value.get("background_task_id").is_none());
    }

    #[test]
    fn context_notice_uses_the_manager_compatible_compacted_shape() {
        let notice = desk_diagnose_core::model_context::ContextNotice::compacted("turn-7", 3, 19);
        let value = serde_json::to_value(ContextNoticeDto::from(notice)).unwrap();
        assert_eq!(value["kind"], "compacted");
        assert_eq!(value["checkpointGeneration"], 3);
        assert_eq!(value["coveredMessageCount"], 19);
    }

    #[test]
    fn attachment_snapshot_never_exposes_opaque_ref_or_egress_metadata() {
        let attachment = ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: "attachment-1".into(),
            client_request_id: "request-1".into(),
            actor_id: "owner".into(),
            device_id: "device".into(),
            surface: AgentSessionSurface::DeviceAssistant,
            kind: ContextAttachmentKind::InteractiveSession,
            object_ref: AttachmentObjectRef {
                opaque_token: "never-send-this-token".into(),
                object_incarnation: "worker-1".into(),
                source_provider_id: "desktop.ui".into(),
                source_capability_id: "desktop.ui.inspect".into(),
            },
            bounds: AttachmentBounds {
                max_bytes: 1024,
                max_objects: 16,
            },
            display_summary: "current interactive session".into(),
            created_at_unix_ms: 100,
            expires_at_unix_ms: 200,
            envelope: DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: "private-envelope".into(),
                content: ContentRef::EphemeralObservation {
                    observation_id: "private-observation".into(),
                    size_bytes: 1,
                    expires_at_unix_ms: 200,
                },
                provenance: DataProvenance {
                    source_provider_id: "desktop.ui".into(),
                    source_tool_name: "inspect_desktop_ui".into(),
                    source_object_id: Some("private-object".into()),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: "a".repeat(64),
                sensitivity: Sensitivity::Sensitive,
                allowed_destinations: vec![DestinationIdentity::Model {
                    connection_id: "private-connection".into(),
                    connection_revision: 1,
                    model_id: "private-model".into(),
                    profile_revision: 1,
                }],
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(200),
                    delete_with_run: false,
                },
            },
            state: AttachmentState::Active,
        };
        let active = ContextAttachmentDto::from_at(attachment.clone(), 150);
        assert_eq!(active.state, "active");
        let expired = ContextAttachmentDto::from_at(attachment.clone(), 200);
        assert_eq!(expired.state, "stale");
        assert_eq!(expired.stale_reason.as_deref(), Some("expired"));

        let encoded = serde_json::to_string(&ContextAttachmentDto::from(attachment)).unwrap();
        assert!(encoded.contains("desktop.ui.inspect"));
        for secret in [
            "never-send-this-token",
            "private-envelope",
            "private-observation",
            "private-connection",
            "private-model",
            &"a".repeat(64),
        ] {
            assert!(!encoded.contains(secret));
        }
    }
}
