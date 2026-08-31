//! Browser-facing access to an OSS Device Assistant conversation.
//!
//! The browser supplies the same target connection and conversation intent used
//! by the signaling turn. The server resolves the target's authenticated client
//! id and re-derives the subject-namespaced conversation key. Snapshot reads and
//! background stops can also locate an offline target using an explicit original
//! session id; the subsequent store operation still checks the original actor and
//! device. This recovery selector is not authority to execute a new action.

use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::communication::CommunicationDraftHandoff;
use desk_agent_protocol::computer_use::{ComputerActionCompleted, ComputerActionOutput};
use desk_agent_protocol::data_lineage::{ContentRef, DataEnvelope};
use desk_agent_protocol::device_assistant::DeviceAssistantAsk;
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
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

use crate::agent_session_store::{PermissionGrantIssuanceContext, SignalAgentSessionStore};
use crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
use crate::error::DeskSignalError;

pub const TAG: &str = "DeviceAssistantSession";
pub(crate) mod recovery;
const EVIDENCE_SUMMARY_SCHEMA_VERSION: u16 = 1;
const MAX_EVIDENCE_NODES: usize = 128;
const MAX_EVIDENCE_RECEIPTS: usize = 32;

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceAssistantSessionQuery {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceAssistantSessionListQuery {
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
pub struct UnknownOutcomeDispositionBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub work_id: i64,
    pub execution_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnknownOutcomeDto {
    pub work_id: i64,
    pub action_request_id: String,
    pub execution_id: String,
    pub work_kind: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnknownOutcomeDispositionResponse {
    pub disposed: bool,
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
pub struct DeviceAssistantSessionSnapshotDto {
    /// Opaque recovery selector; ownership is rechecked on every read or stop.
    pub session_id: String,
    pub seq: i64,
    /// Whether the persisted turn is still running or awaiting approval.
    pub active: bool,
    /// Device Assistant request currently represented by the persisted turn.
    pub request_id: Option<String>,
    /// Running background command generation that may be cancelled.
    pub active_execution_generation: Option<String>,
    /// Exact unresolved durable action that requires owner disposition before
    /// another mutation can start. No payload or secret-bearing input is exposed.
    pub unresolved_outcome: Option<UnknownOutcomeDto>,
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
    /// Bounded metadata-only lineage graph. It contains no message bodies,
    /// credentials, cookies, tokens, browser storage or native paths.
    pub evidence_summary: EvidenceSummaryDto,
    pub messages: Vec<SnapshotMessageDto>,
    /// Durable transcript metadata for context-window changes. No omitted text is exposed.
    pub context_notices: Vec<ContextNoticeDto>,
    /// Durable selection metadata only; no UI tree, cells, files or screenshots.
    pub context_attachments: Vec<ContextAttachmentDto>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceSummaryDto {
    pub schema_version: u16,
    pub nodes: Vec<EvidenceNodeDto>,
    pub artifacts: Vec<EvidenceArtifactDto>,
    pub handoff_receipts: Vec<EvidenceHandoffReceiptDto>,
    pub missing_source_envelope_ids: Vec<String>,
    pub truncated: bool,
    pub graph_complete: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceNodeDto {
    pub envelope_id: String,
    pub content_kind: String,
    pub content_sha256: Option<String>,
    pub size_bytes: u64,
    pub media_type: Option<String>,
    pub envelope_digest_sha256: String,
    pub sensitivity: String,
    pub source_provider_id: String,
    pub source_tool_name: String,
    pub source_object_id: Option<String>,
    pub source_envelope_ids: Vec<String>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceArtifactDto {
    pub source_envelope_id: String,
    pub artifact_id: String,
    pub file_name: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub digest_sha256: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceHandoffReceiptDto {
    pub source_envelope_id: String,
    pub handoff_id: String,
    pub run_id: String,
    pub surface_kind: String,
    pub prepared_payload_sha256: String,
    pub readback_payload_sha256: Option<String>,
    pub verification: String,
    pub send_authority: String,
    pub handed_off_at_unix_ms: u64,
}

fn enum_wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn evidence_node(envelope: &DataEnvelope) -> EvidenceNodeDto {
    let (content_kind, content_sha256, size_bytes, media_type) = match &envelope.content {
        ContentRef::ImmutableBlob {
            sha256,
            size_bytes,
            media_type,
            ..
        } => (
            "immutable_blob",
            Some(sha256.clone()),
            *size_bytes,
            Some(media_type.clone()),
        ),
        ContentRef::Artifact {
            sha256,
            size_bytes,
            media_type,
            ..
        } => (
            "artifact",
            Some(sha256.clone()),
            *size_bytes,
            Some(media_type.clone()),
        ),
        ContentRef::EphemeralObservation { size_bytes, .. } => {
            ("ephemeral_observation", None, *size_bytes, None)
        }
    };
    EvidenceNodeDto {
        envelope_id: envelope.envelope_id.clone(),
        content_kind: content_kind.into(),
        content_sha256,
        size_bytes,
        media_type,
        envelope_digest_sha256: envelope.digest_sha256.clone(),
        sensitivity: enum_wire_name(&envelope.sensitivity),
        source_provider_id: envelope.provenance.source_provider_id.clone(),
        source_tool_name: envelope.provenance.source_tool_name.clone(),
        source_object_id: envelope.provenance.source_object_id.clone(),
        source_envelope_ids: envelope.provenance.source_envelope_ids.clone(),
    }
}

fn build_evidence_summary(
    messages: &[ChatMessage],
    attachments: &[ContextAttachment],
) -> EvidenceSummaryDto {
    let mut envelopes = BTreeMap::<String, &DataEnvelope>::new();
    for attachment in attachments {
        envelopes
            .entry(attachment.envelope.envelope_id.clone())
            .or_insert(&attachment.envelope);
    }
    for message in messages {
        if let Some(envelope) = message.data_envelope.as_ref() {
            envelopes
                .entry(envelope.envelope_id.clone())
                .or_insert(envelope);
        }
    }
    let all_ids = envelopes.keys().cloned().collect::<BTreeSet<_>>();
    let mut missing = BTreeSet::new();
    for envelope in envelopes.values() {
        for source in &envelope.provenance.source_envelope_ids {
            if !all_ids.contains(source) {
                missing.insert(source.clone());
            }
        }
    }
    let truncated = envelopes.len() > MAX_EVIDENCE_NODES;
    let nodes = envelopes
        .values()
        .take(MAX_EVIDENCE_NODES)
        .map(|envelope| evidence_node(envelope))
        .collect();

    let mut artifacts = Vec::new();
    let mut handoff_receipts = Vec::new();
    for message in messages {
        let Some(source_envelope_id) = message
            .data_envelope
            .as_ref()
            .map(|envelope| envelope.envelope_id.clone())
        else {
            continue;
        };
        let completion = serde_json::from_str::<ComputerActionCompleted>(&message.text).ok();
        match completion.and_then(|completion| completion.output) {
            Some(ComputerActionOutput::FileArtifact(artifact))
                if artifact.validate().is_ok() && artifacts.len() < MAX_EVIDENCE_RECEIPTS =>
            {
                artifacts.push(EvidenceArtifactDto {
                    source_envelope_id: source_envelope_id.clone(),
                    artifact_id: artifact.file.token,
                    file_name: artifact.file_name,
                    media_type: artifact.media_type,
                    size_bytes: artifact.size_bytes,
                    digest_sha256: artifact.digest_sha256,
                });
            }
            Some(ComputerActionOutput::CommunicationHandoff(handoff))
                if handoff.validate().is_ok() && handoff_receipts.len() < MAX_EVIDENCE_RECEIPTS =>
            {
                handoff_receipts.push(EvidenceHandoffReceiptDto {
                    source_envelope_id: source_envelope_id.clone(),
                    handoff_id: handoff.handoff_id,
                    run_id: handoff.run_id,
                    surface_kind: enum_wire_name(&handoff.surface.kind),
                    prepared_payload_sha256: handoff.prepared_payload_sha256,
                    readback_payload_sha256: handoff.readback_payload_sha256,
                    verification: enum_wire_name(&handoff.verification),
                    send_authority: enum_wire_name(&handoff.send_authority),
                    handed_off_at_unix_ms: handoff.handed_off_at_unix_ms,
                });
            }
            _ => {}
        }
        if let Ok(handoff) = serde_json::from_str::<CommunicationDraftHandoff>(&message.text)
            && handoff.validate().is_ok()
            && handoff_receipts.len() < MAX_EVIDENCE_RECEIPTS
        {
            handoff_receipts.push(EvidenceHandoffReceiptDto {
                source_envelope_id,
                handoff_id: handoff.handoff_id,
                run_id: handoff.run_id,
                surface_kind: enum_wire_name(&handoff.surface.kind),
                prepared_payload_sha256: handoff.prepared_payload_sha256,
                readback_payload_sha256: handoff.readback_payload_sha256,
                verification: enum_wire_name(&handoff.verification),
                send_authority: enum_wire_name(&handoff.send_authority),
                handed_off_at_unix_ms: handoff.handed_off_at_unix_ms,
            });
        }
    }
    let missing_source_envelope_ids = missing.into_iter().collect::<Vec<_>>();
    EvidenceSummaryDto {
        schema_version: EVIDENCE_SUMMARY_SCHEMA_VERSION,
        nodes,
        artifacts,
        handoff_receipts,
        graph_complete: !truncated && missing_source_envelope_ids.is_empty(),
        missing_source_envelope_ids,
        truncated,
    }
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
pub struct DeviceAssistantSessionSummaryDto {
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
pub struct DeviceAssistantSessionListDto {
    pub sessions: Vec<DeviceAssistantSessionSummaryDto>,
}

fn not_accessible() -> HttpResponse {
    HttpResponse::Ok().json(RestResponse::<()>::failed(
        DeskErrorCode::PERMISSION_ERROR,
        "Device Assistant session not found or not accessible".to_string(),
    ))
}

#[utoipa::path(
    tag = TAG,
    summary = "Read a Device Assistant conversation snapshot (browser view)",
    params(
        ("connection" = String, Query, description = "Target connection id"),
        ("conversation" = Option<String>, Query, description = "Client conversation intent"),
        ("session" = Option<String>, Query, description = "Opaque session id from history list"),
    ),
    responses((status = 200, description = "Conversation snapshot, or a uniform \
        not-found/not-accessible response", body = RestResponse<DeviceAssistantSessionSnapshotDto>)),
)]
#[get("/my/device-assistant-session")]
pub async fn get_device_assistant_session(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<DeviceAssistantSessionQuery>,
) -> Result<HttpResponse, DeskSignalError> {
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let store = SignalAgentSessionStore::new(crate::db::get_db().clone());
    let Some((session_id, target_audience)) = recovery::resolve(
        &store,
        &connection_map,
        &actor_id,
        &query.connection,
        query.session.as_deref(),
        query.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let snapshot = store
        .read_assistant_snapshot_for_subject(&session_id, &actor_id, &target_audience)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    match snapshot {
        Some(snapshot) => {
            let background_tasks = snapshot.background_tasks;
            let capability_grants = snapshot.capability_grants;
            let snapshot = snapshot.session;
            let evidence_summary =
                build_evidence_summary(&snapshot.messages, &snapshot.context_attachments);
            Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
                DeviceAssistantSessionSnapshotDto {
                    session_id,
                    seq: snapshot.seq,
                    active: snapshot.active,
                    request_id: snapshot.request_id,
                    active_execution_generation: snapshot.active_execution_generation,
                    unresolved_outcome: snapshot.unresolved_action.map(|action| {
                        UnknownOutcomeDto {
                            work_id: action.work_id,
                            action_request_id: action.action_request_id,
                            execution_id: action.execution_id,
                            work_kind: action.kind.as_str().to_string(),
                        }
                    }),
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
                    evidence_summary,
                    messages: snapshot
                        .messages
                        .into_iter()
                        .filter(|message| {
                            !desk_diagnose_core::permission_resume::is_permission_resume_message(
                                message,
                            )
                        })
                        .map(Into::into)
                        .collect(),
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
    summary = "Manually dispose one exact unknown Device Assistant action",
    request_body = UnknownOutcomeDispositionBody,
    responses((status = 200, description = "Owner disposition recorded; no retry or grant restoration occurs", body = RestResponse<UnknownOutcomeDispositionResponse>)),
)]
#[post("/my/device-assistant-session/outcome-unknown/dispose")]
pub async fn dispose_device_assistant_unknown_outcome(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<UnknownOutcomeDispositionBody>,
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
    let session_store = SignalAgentSessionStore::new(crate::db::get_db().clone());
    let Some(snapshot) = session_store
        .read_snapshot_for_subject(&session_id, &actor_id, &target_audience)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?
    else {
        return Ok(not_accessible());
    };
    if !snapshot.unresolved_action.as_ref().is_some_and(|action| {
        action.work_id == body.work_id && action.execution_id == body.execution_id
    }) {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "the exact unknown action is no longer pending disposition",
        ));
    }

    let now_dt = chrono::Utc::now();
    let now_unix_ms = u64::try_from(now_dt.timestamp_millis()).map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "system clock predates Unix epoch",
        )
    })?;
    use crate::capability_grant_store::CapabilityManualDispositionResult;
    match crate::capability_grant_store::SignalCapabilityGrantStore::new(
        crate::db::get_db().clone(),
    )
    .manually_dispose_unknown_for_subject(
        body.work_id,
        &body.execution_id,
        &session_id,
        &actor_id,
        &target_audience,
        now_unix_ms,
    )
    .await
    .map_err(|error| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("record unknown-action disposition: {error}"),
        )
    })? {
        CapabilityManualDispositionResult::Applied
        | CapabilityManualDispositionResult::AlreadyResolved => {}
        CapabilityManualDispositionResult::SubjectMismatch
        | CapabilityManualDispositionResult::StateMismatch => {
            return Err(DeskSignalError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "the exact unknown action cannot be manually disposed",
            ));
        }
    }

    let now = now_dt.to_rfc3339();
    let disposition = session_store
        .manually_dispose_unknown_for_subject(
            &session_id,
            &actor_id,
            &target_audience,
            body.work_id,
            &body.execution_id,
            &now,
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    if disposition == crate::agent_session_store::EventAppend::Busy {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::ACTION_NEED_RETRY,
            "the conversation is still active; retry the disposition",
        ));
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        UnknownOutcomeDispositionResponse { disposed: true },
    )))
}

#[utoipa::path(
    tag = TAG,
    summary = "Revoke one owner-scoped Device Assistant capability grant",
    request_body = CapabilityGrantRevokeBody,
    responses((status = 200, description = "Revoked grant metadata; no work is dispatched", body = RestResponse<CapabilityGrantDto>)),
)]
#[post("/my/device-assistant-session/capability-grant/revoke")]
pub async fn revoke_device_assistant_capability_grant(
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
#[post("/my/device-assistant-session/permission-decision")]
pub async fn decide_device_assistant_permission(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<PermissionDecisionBody>,
) -> Result<HttpResponse, DeskSignalError> {
    decide_permission_on(crate::db::get_db(), connection_map, body).await
}

pub(crate) async fn decide_permission_on(
    db: &sea_orm::DatabaseConnection,
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
    let store = SignalAgentSessionStore::new(db.clone());
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
    let decisions: Vec<desk_diagnose_core::dynamic_run::PermissionDecisionItem> =
        body.items.clone().into_iter().map(Into::into).collect();
    if let Some(state) = store
        .replay_permission_decision(
            &session_id,
            &actor_id,
            &target_audience,
            &body.request_id,
            &decisions,
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &error.message)
        })?
    {
        return Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
            PermissionDecisionResponse {
                state: state.into(),
            },
        )));
    }
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
    let implicit_fresh_object_refs = readiness
        .readiness
        .context_references
        .iter()
        .filter(|reference| {
            matches!(
                reference.object_ref.object_kind,
                desk_agent_protocol::computer_use::ObjectKind::BrowserSurface
                    | desk_agent_protocol::computer_use::ObjectKind::Application
                    | desk_agent_protocol::computer_use::ObjectKind::Range
                    | desk_agent_protocol::computer_use::ObjectKind::Document
                    | desk_agent_protocol::computer_use::ObjectKind::Slide
            )
        })
        .map(|reference| reference.object_ref.clone())
        .collect::<Vec<_>>();
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
    let decision = store
        .decide_permission_request(
            &session_id,
            &actor_id,
            &target_audience,
            &body.request_id,
            decisions,
            PermissionGrantIssuanceContext {
                surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision,
                now_unix_ms,
                implicit_fresh_object_refs: &implicit_fresh_object_refs,
            },
            &now,
        )
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &error.message)
        })?;
    if decision.newly_recorded
        && let Some(snapshot) = store
            .read_snapshot_for_subject(&session_id, &actor_id, &target_audience)
            .await
            .map_err(|error| {
                DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
            })?
        && let Some(question) =
            desk_diagnose_core::permission_resume::latest_user_requirement(&snapshot.messages)
                .map(|message| message.text.clone())
    {
        let resume_request_id = format!("permission-resume-{}", body.request_id);
        let resume_ask = DeviceAssistantAsk {
            question,
            client_message_id: resume_request_id.clone(),
            conversation_id: snapshot.client_conversation_id,
            locale: None,
            // The orchestrator reloads the original input selection. Current
            // active context is not authority to expand a permission resume.
            selected_capability_ids: Vec::new(),
            selected_attachment_ids: Vec::new(),
        };
        let resume_connections = connection_map.clone();
        let resume_db = db.clone();
        let resume_target_connection = body.connection.clone();
        let resume_target_audience = target_audience.clone();
        let resume_session_id = session_id.clone();
        let resume_permission_request_id = body.request_id.clone();
        actix_web::rt::spawn(async move {
            crate::device_assistant_orchestrator::resume_after_permission_decision(
                resume_connections,
                resume_db,
                resume_request_id,
                resume_target_connection,
                SINGLE_ACCOUNT_USER_ID,
                resume_target_audience,
                resume_session_id,
                resume_permission_request_id,
                resume_ask,
            )
            .await;
        });
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        PermissionDecisionResponse {
            state: decision.state.into(),
        },
    )))
}

#[utoipa::path(
    tag = TAG,
    summary = "Request cancellation of an authorized Device Assistant background task",
    request_body = BackgroundCancelBody,
    responses((status = 200, description = "Durable cancellation intent; Provider delivery is asynchronous", body = RestResponse<BackgroundTaskDto>)),
)]
#[post("/my/device-assistant-session/background-task/cancel")]
pub async fn cancel_device_assistant_background_task(
    connection_map: web::Data<SharedConnectionMap>,
    body: web::Json<BackgroundCancelBody>,
) -> Result<HttpResponse, DeskSignalError> {
    let actor_id = SINGLE_ACCOUNT_USER_ID.to_string();
    let Some((session_id, target_audience)) = recovery::resolve(
        &SignalAgentSessionStore::new(crate::db::get_db().clone()),
        &connection_map,
        &actor_id,
        &body.connection,
        body.session.as_deref(),
        body.conversation.as_deref(),
    )
    .await?
    else {
        return Ok(not_accessible());
    };
    let original =
        crate::capability_grant_store::SignalCapabilityGrantStore::new(crate::db::get_db().clone())
            .request_computer_background_cancel(
                &body.task_id,
                &session_id,
                &actor_id,
                &target_audience,
                &body.request_id,
                &body.reason,
            )
            .await
            .map_err(|error| {
                DeskSignalError::new_custom_error(
                    DeskErrorCode::PRECONDITION_FAILED,
                    &error.to_string(),
                )
            })?;
    if let Some(record) = original {
        return Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
            BackgroundTaskDto::from(record),
        )));
    }
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
    summary = "List recent Device Assistant conversations for a device",
    params(
        ("connection" = String, Query, description = "Target connection id"),
        ("limit" = Option<u64>, Query, description = "Maximum rows, default 30 and capped at 100"),
    ),
    responses((status = 200, description = "Authorized recent Device Assistant sessions",
        body = RestResponse<DeviceAssistantSessionListDto>)),
)]
#[get("/my/device-assistant-sessions")]
pub async fn list_device_assistant_sessions(
    connection_map: web::Data<SharedConnectionMap>,
    query: web::Query<DeviceAssistantSessionListQuery>,
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
        .list_device_assistant_sessions(&actor_id, &target_audience, limit)
        .await
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
    let sessions = summaries
        .into_iter()
        .map(|summary| DeviceAssistantSessionSummaryDto {
            session_id: summary.session_id,
            conversation_id: summary.client_conversation_id,
            first_question: summary.first_question,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            active: summary.active,
            message_count: summary.message_count,
        })
        .collect();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        DeviceAssistantSessionListDto { sessions },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::browser_control::{BrowserOrigin, BrowserOriginKind};
    use desk_agent_protocol::communication::{
        COMMUNICATION_SCHEMA_VERSION, CommunicationChannel, CommunicationPrepareVerification,
        CommunicationSendAuthority, CommunicationSurfaceKind, CommunicationSurfaceRef,
        CommunicationSurfaceScope,
    };
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
    fn evidence_summary_is_bounded_metadata_only_and_checks_graph_integrity() {
        fn envelope(id: &str, sources: Vec<String>, digest: char) -> DataEnvelope {
            DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: id.into(),
                content: ContentRef::ImmutableBlob {
                    blob_id: format!("{id}-blob"),
                    sha256: digest.to_string().repeat(64),
                    size_bytes: 7,
                    media_type: "application/json".into(),
                },
                provenance: DataProvenance {
                    source_provider_id: "test.provider".into(),
                    source_tool_name: "test_tool".into(),
                    source_object_id: None,
                    source_envelope_ids: sources,
                },
                digest_sha256: digest.to_string().repeat(64),
                sensitivity: Sensitivity::Sensitive,
                allowed_destinations: Vec::new(),
                retention: RetentionBoundary {
                    expires_at_unix_ms: None,
                    delete_with_run: false,
                },
            }
        }

        let mut user = ChatMessage::text(
            "user-1",
            ChatRole::User,
            "SECRET BODY C:\\private\\report.docx",
        );
        user.data_envelope = Some(envelope("user-envelope", Vec::new(), 'a'));
        let mut result = ChatMessage::tool_result(
            "result-1",
            "call-1",
            "SECRET TOOL OUTPUT C:\\private\\report.docx",
        );
        result.data_envelope = Some(envelope(
            "result-envelope",
            vec!["user-envelope".into()],
            'b',
        ));

        let summary = build_evidence_summary(&[user.clone(), result.clone()], &[]);
        assert!(summary.graph_complete);
        assert!(!summary.truncated);
        assert_eq!(summary.nodes.len(), 2);
        assert!(summary.missing_source_envelope_ids.is_empty());
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("SECRET"));
        assert!(!json.contains("private"));

        let handoff = CommunicationDraftHandoff {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff_id: "gmail-handoff-1".into(),
            run_id: "run-1".into(),
            surface: CommunicationSurfaceRef {
                channel: CommunicationChannel::Email,
                kind: CommunicationSurfaceKind::ChromeExtension,
                scope: CommunicationSurfaceScope::WebOrigin {
                    origin: BrowserOrigin {
                        kind: BrowserOriginKind::Https,
                        host_ascii: "mail.google.com".into(),
                        port: 443,
                    },
                },
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                adapter_id: "lcxl-browser-extension".into(),
                adapter_version: "0.1.0".into(),
                profile_id: "profile-1".into(),
                account_id: "gmail-web-current-profile".into(),
                revision: 1,
            },
            compose_id: "compose-1".into(),
            prepared_payload_sha256: "d".repeat(64),
            verification: CommunicationPrepareVerification::SemanticExact,
            readback_payload_sha256: Some("d".repeat(64)),
            send_authority: CommunicationSendAuthority::ManualOnly,
            handed_off_at_unix_ms: 42,
        };
        handoff.validate().unwrap();
        let mut handoff_message = ChatMessage::tool_result(
            "handoff-result",
            "gmail-call",
            serde_json::to_string(&handoff).unwrap(),
        );
        handoff_message.data_envelope = Some(envelope(
            "handoff-envelope",
            vec!["result-envelope".into()],
            'd',
        ));
        let with_handoff = build_evidence_summary(&[user, result, handoff_message], &[]);
        assert!(with_handoff.graph_complete);
        assert_eq!(with_handoff.handoff_receipts.len(), 1);
        assert_eq!(
            with_handoff.handoff_receipts[0].surface_kind,
            "chrome_extension"
        );
        assert_eq!(
            with_handoff.handoff_receipts[0].source_envelope_id,
            "handoff-envelope"
        );

        let orphan = ChatMessage {
            data_envelope: Some(envelope(
                "orphan-envelope",
                vec!["missing-envelope".into()],
                'c',
            )),
            ..ChatMessage::text("orphan", ChatRole::Assistant, "hidden")
        };
        let incomplete = build_evidence_summary(&[orphan], &[]);
        assert!(!incomplete.graph_complete);
        assert_eq!(
            incomplete.missing_source_envelope_ids,
            vec!["missing-envelope"]
        );
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
