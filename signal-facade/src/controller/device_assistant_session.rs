//! Shared browser/mobile wire DTOs and pure projections for Device Assistant sessions.

use desk_agent_protocol::communication::{
    CommunicationChannel, CommunicationDraftHandoff, GmailWebExactSendInput, SlackWebExactSendInput,
};
use desk_agent_protocol::computer_use::{ComputerActionCompleted, ComputerActionOutput};
use desk_agent_protocol::data_lineage::{ContentRef, DataEnvelope};
use desk_diagnose_core::chat::ChatMessage;
use desk_diagnose_core::context_attachment::{
    AttachmentStaleReason, AttachmentState, ContextAttachment, ContextAttachmentKind,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

const EVIDENCE_SUMMARY_SCHEMA_VERSION: u16 = 1;
const MAX_EVIDENCE_NODES: usize = 128;
const MAX_EVIDENCE_RECEIPTS: usize = 32;
pub const DEFAULT_SNAPSHOT_MESSAGE_PAGE_SIZE: usize = 100;
pub const MAX_SNAPSHOT_MESSAGE_PAGE_SIZE: usize = 100;
pub const MAX_SNAPSHOT_MESSAGE_PAGE_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceAssistantSessionQuery {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    /// Exclusive message cursor. Omit for the newest page.
    pub message_before: Option<String>,
    pub message_limit: Option<usize>,
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
        #[serde(alias = "resourceScope")]
        resource_scope: Vec<String>,
        #[serde(alias = "operationScope")]
        operation_scope: Vec<String>,
        #[serde(default, alias = "exportDestinations")]
        export_destinations: Vec<desk_agent_protocol::data_lineage::DestinationIdentity>,
        #[serde(alias = "ttlSeconds")]
        ttl_seconds: u32,
        #[serde(alias = "maxUses")]
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
    /// Stable message id used to key and deduplicate rendered turns.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Wire role token. An `assistant` message is AI-generated.
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

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMessagePageDto {
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before_message_id: Option<String>,
    pub limit: usize,
}

pub struct SnapshotMessagePageProjection {
    pub messages: Vec<SnapshotMessageDto>,
    pub page: SnapshotMessagePageDto,
}

pub fn project_snapshot_message_page(
    messages: Vec<ChatMessage>,
    before_message_id: Option<&str>,
    requested_limit: Option<usize>,
) -> Result<SnapshotMessagePageProjection, &'static str> {
    let limit = requested_limit.unwrap_or(DEFAULT_SNAPSHOT_MESSAGE_PAGE_SIZE);
    if limit == 0 || limit > MAX_SNAPSHOT_MESSAGE_PAGE_SIZE {
        return Err("invalid message page limit");
    }
    let visible = messages
        .into_iter()
        .filter(|message| {
            !desk_diagnose_core::permission_resume::is_permission_resume_message(message)
        })
        .collect::<Vec<_>>();
    let end = match before_message_id {
        Some(cursor) => visible
            .iter()
            .position(|message| message.message_id == cursor)
            .ok_or("invalid message page cursor")?,
        None => visible.len(),
    };
    let mut groups = Vec::new();
    let mut index = 0;
    while index < visible.len() {
        let start = index;
        index += 1;
        if visible[start].role == desk_diagnose_core::chat::ChatRole::Assistant
            && !visible[start].tool_calls.is_empty()
        {
            while index < visible.len()
                && visible[index].role == desk_diagnose_core::chat::ChatRole::Tool
            {
                index += 1;
            }
        }
        groups.push((start, index));
    }
    if end != visible.len() && !groups.iter().any(|(start, _)| *start == end) {
        return Err("message page cursor is not a group boundary");
    }
    let mut start = end;
    let mut count = 0usize;
    let mut bytes = 2usize;
    for (group_start, group_end) in groups
        .iter()
        .copied()
        .filter(|(_, group_end)| *group_end <= end)
        .rev()
    {
        let group_count = group_end - group_start;
        let group = visible[group_start..group_end]
            .iter()
            .cloned()
            .map(SnapshotMessageDto::from)
            .collect::<Vec<_>>();
        let group_bytes = serde_json::to_vec(&group)
            .map_err(|_| "message page projection failed")?
            .len();
        if count + group_count > limit
            || bytes.saturating_add(group_bytes) > MAX_SNAPSHOT_MESSAGE_PAGE_BYTES
        {
            if start == end {
                return Err("one transcript message group exceeds the page budget");
            }
            break;
        }
        start = group_start;
        count += group_count;
        bytes = bytes.saturating_add(group_bytes);
    }
    let has_more = start > 0;
    let next_before_message_id = has_more.then(|| visible[start].message_id.clone());
    Ok(SnapshotMessagePageProjection {
        messages: visible[start..end]
            .iter()
            .cloned()
            .map(Into::into)
            .collect(),
        page: SnapshotMessagePageDto {
            has_more,
            next_before_message_id,
            limit,
        },
    })
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
    pub fn from_at(attachment: ContextAttachment, now_unix_ms: u64) -> Self {
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
        ContextAttachmentKind::WindowSelection => "window_selection",
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
    /// Monotonic session-store snapshot version used for out-of-order reconciliation.
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
    /// Recent screen observations, bound to epoch/turn/tool/frame. Durable
    /// snapshots contain no pixel bytes; live previews arrive on AgentEvent.
    pub visual_evidence: Vec<desk_agent_protocol::visual_evidence::VisualEvidenceFrame>,
    /// The persisted conversation, oldest first.
    pub messages: Vec<SnapshotMessageDto>,
    /// Cursor metadata for the bounded `messages` page.
    pub message_page: SnapshotMessagePageDto,
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

pub fn build_evidence_summary(
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
    pub input_revision: u64,
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
            input_revision: grant.input_revision,
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

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSendAttachmentDto {
    pub file_name: String,
    pub size_bytes: u64,
}

/// Safe owner-facing projection of a frozen exact-send request. Browser
/// references, canonical permission JSON, credentials, and pairing secrets are
/// deliberately excluded.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExternalSendConfirmationDto {
    pub channel: CommunicationChannel,
    pub account_id: String,
    pub destination: String,
    pub subject: Option<String>,
    pub body_plain_text: String,
    pub body_size_bytes: u64,
    pub attachments: Vec<ExternalSendAttachmentDto>,
    pub one_shot: bool,
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
    pub external_send_confirmation: Option<ExternalSendConfirmationDto>,
}

fn external_send_confirmation(
    tool_name: &str,
    canonical_input_json: Option<&str>,
) -> Option<ExternalSendConfirmationDto> {
    let canonical_input_json = canonical_input_json?;
    let (snapshot, body_plain_text) = match tool_name {
        "send_gmail_web_exact" => {
            let input: GmailWebExactSendInput = serde_json::from_str(canonical_input_json).ok()?;
            desk_diagnose_core::communication::verify_gmail_web_exact_send_input(&input).ok()?;
            (
                input.handoff.send_payload_snapshot?,
                input.draft.body_plain_text,
            )
        }
        "send_slack_web_exact" => {
            let input: SlackWebExactSendInput = serde_json::from_str(canonical_input_json).ok()?;
            desk_diagnose_core::communication::verify_slack_web_exact_send_input(&input).ok()?;
            (input.handoff.send_payload_snapshot?, input.body_plain_text)
        }
        _ => return None,
    };
    let destination = snapshot
        .payload
        .recipients
        .first()?
        .canonical_address
        .clone();
    Some(ExternalSendConfirmationDto {
        channel: snapshot.payload.surface.channel,
        account_id: snapshot.payload.surface.account_id,
        destination,
        subject: (snapshot.payload.surface.channel == CommunicationChannel::Email)
            .then_some(snapshot.payload.subject),
        body_size_bytes: body_plain_text.len() as u64,
        body_plain_text,
        attachments: snapshot
            .payload
            .attachments
            .into_iter()
            .map(|attachment| ExternalSendAttachmentDto {
                file_name: attachment.file_name,
                size_bytes: attachment.size_bytes,
            })
            .collect(),
        one_shot: true,
    })
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
                .map(|item| {
                    let external_send_confirmation = external_send_confirmation(
                        &item.tool_name,
                        item.canonical_input_json.as_deref(),
                    );
                    GrantRequestItemDto {
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
                        external_send_confirmation,
                    }
                })
                .collect(),
            created_at: request.created_at,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DeviceAssistantSessionSummaryDto {
    /// Opaque server-side selector. Authorization is rechecked when it is used.
    pub session_id: String,
    /// Validated client continuation intent.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn slack_exact_send_json() -> String {
        use desk_agent_protocol::{
            browser_control::{
                BROWSER_CONTROL_SCHEMA_VERSION, BrowserAdapterRef, BrowserElementRef,
                BrowserElementRole, BrowserEngineKind, BrowserOrigin, BrowserOriginKind,
                BrowserPageRef,
            },
            communication::{
                COMMUNICATION_SCHEMA_VERSION, CommunicationChannel, CommunicationDraftHandoff,
                CommunicationPayload, CommunicationPrepareVerification, CommunicationSendAuthority,
                CommunicationSurfaceKind, CommunicationSurfaceRef, CommunicationSurfaceScope,
                ImmutableBodySnapshot, RecipientIdentity, RecipientKind, RecipientRole,
                SlackWebExactSendInput,
            },
            data_lineage::ContentRef,
        };

        let page = BrowserPageRef {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeExtension,
                device_id: "device".into(),
                os_session_id: "session".into(),
                browser_major_version: 151,
                browser_version: "151.0".into(),
                adapter_id: "chrome-extension".into(),
                adapter_version: "1".into(),
                profile_incarnation: "profile".into(),
                connection_revision: 7,
            },
            page_id: "page".into(),
            page_incarnation: "page-incarnation".into(),
            origin: BrowserOrigin {
                kind: BrowserOriginKind::Https,
                host_ascii: "app.slack.com".into(),
                port: 443,
            },
            document_revision: 3,
            url_sha256: "a".repeat(64),
            observed_at_unix_ms: 200,
        };
        let composer = BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: "composer".into(),
            role: BrowserElementRole::Textbox,
            accessible_name: "Message #review".into(),
            value: None,
            element_revision: 1,
        };
        let send_control = BrowserElementRef {
            element_id: "send".into(),
            role: BrowserElementRole::Button,
            accessible_name: "Send message".into(),
            ..composer.clone()
        };
        let surface = CommunicationSurfaceRef {
            channel: CommunicationChannel::Chat,
            kind: CommunicationSurfaceKind::ChromeExtension,
            scope: CommunicationSurfaceScope::WebOrigin {
                origin: page.origin.clone(),
            },
            device_id: page.adapter.device_id.clone(),
            os_session_id: page.adapter.os_session_id.clone(),
            adapter_id: "slack-web".into(),
            adapter_version: "1".into(),
            profile_id: page.adapter.profile_incarnation.clone(),
            account_id: "slack-current-profile".into(),
            revision: page.adapter.connection_revision,
        };
        let body_plain_text = "Reviewed Slack body".to_string();
        let body_digest =
            "93c68073d6928d5cdb4b7e77a45fb5171f3cb32931eed876c10fe96c319b5d49".to_string();
        let payload = CommunicationPayload {
            surface: surface.clone(),
            recipients: vec![RecipientIdentity {
                role: RecipientRole::ChatDestination,
                kind: RecipientKind::ChatChannel,
                stable_id: "slack-channel-review".into(),
                canonical_address: composer.accessible_name.clone(),
                display_name: None,
                display_warnings: Vec::new(),
                resolved_members: Vec::new(),
                member_snapshot_sha256: None,
            }],
            subject: String::new(),
            body: ImmutableBodySnapshot {
                content: ContentRef::ImmutableBlob {
                    blob_id: "slack-body".into(),
                    sha256: body_digest.clone(),
                    size_bytes: body_plain_text.len() as u64,
                    media_type: "text/plain; charset=utf-8".into(),
                },
                media_type: "text/plain; charset=utf-8".into(),
                size_bytes: body_plain_text.len() as u64,
                digest_sha256: body_digest,
            },
            attachments: Vec::new(),
        };
        let snapshot = desk_diagnose_core::communication::seal_send_payload(
            "slack-send-snapshot".into(),
            "run".into(),
            payload,
            100,
        )
        .unwrap();
        serde_json::to_string(&SlackWebExactSendInput {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff: CommunicationDraftHandoff {
                schema_version: COMMUNICATION_SCHEMA_VERSION,
                handoff_id: "slack-handoff".into(),
                run_id: "run".into(),
                surface,
                compose_id: "slack-compose".into(),
                prepared_payload_sha256: "b".repeat(64),
                verification: CommunicationPrepareVerification::SemanticExact,
                readback_payload_sha256: Some("b".repeat(64)),
                send_authority: CommunicationSendAuthority::ExactGrantEligible,
                send_payload_snapshot: Some(snapshot),
                handed_off_at_unix_ms: 100,
            },
            page,
            composer,
            send_control,
            body_plain_text,
        })
        .unwrap()
    }

    #[test]
    fn permission_decision_accepts_mobile_camel_case_and_legacy_snake_case() {
        for fields in [
            serde_json::json!({
                "resourceScope": ["selected:server_resolved"],
                "operationScope": ["use_selected_object"],
                "exportDestinations": [],
                "ttlSeconds": 300,
                "maxUses": 1
            }),
            serde_json::json!({
                "resource_scope": ["selected:server_resolved"],
                "operation_scope": ["use_selected_object"],
                "export_destinations": [],
                "ttl_seconds": 300,
                "max_uses": 1
            }),
        ] {
            let mut item = serde_json::json!({
                "itemId": "inspect",
                "decision": "approve"
            });
            item.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let body: PermissionDecisionBody = serde_json::from_value(serde_json::json!({
                "connection": "host",
                "conversation": "conversation",
                "requestId": "permission",
                "items": [item]
            }))
            .unwrap();
            assert_eq!(body.items.len(), 1);
        }
    }

    #[test]
    fn exact_send_confirmation_projects_only_verified_owner_fields() {
        let canonical = slack_exact_send_json();
        let confirmation =
            external_send_confirmation("send_slack_web_exact", Some(&canonical)).unwrap();
        assert_eq!(confirmation.channel, CommunicationChannel::Chat);
        assert_eq!(confirmation.account_id, "slack-current-profile");
        assert_eq!(confirmation.destination, "Message #review");
        assert_eq!(confirmation.subject, None);
        assert_eq!(confirmation.body_plain_text, "Reviewed Slack body");
        assert_eq!(confirmation.body_size_bytes, 19);
        assert!(confirmation.attachments.is_empty());
        assert!(confirmation.one_shot);

        let mut changed = serde_json::from_str::<serde_json::Value>(&canonical).unwrap();
        changed["body_plain_text"] = serde_json::json!("Changed after review");
        assert!(
            external_send_confirmation("send_slack_web_exact", Some(&changed.to_string()))
                .is_none()
        );
        assert!(external_send_confirmation("send_slack_web_exact", None).is_none());
        assert!(external_send_confirmation("browser_activate_element", Some(&canonical)).is_none());
    }

    #[test]
    fn thousand_message_snapshot_pages_are_stable_and_bounded() {
        let messages = (0..1_000)
            .map(|index| {
                ChatMessage::text(
                    format!("message-{index:04}"),
                    desk_diagnose_core::chat::ChatRole::User,
                    format!("text-{index}"),
                )
            })
            .collect::<Vec<_>>();
        let mut before = None;
        let mut recovered = Vec::new();
        loop {
            let projection =
                project_snapshot_message_page(messages.clone(), before.as_deref(), None).unwrap();
            assert!(projection.messages.len() <= MAX_SNAPSHOT_MESSAGE_PAGE_SIZE);
            assert!(
                serde_json::to_vec(&projection.messages).unwrap().len()
                    <= MAX_SNAPSHOT_MESSAGE_PAGE_BYTES
            );
            recovered.splice(
                0..0,
                projection.messages.iter().map(|message| message.id.clone()),
            );
            if !projection.page.has_more {
                break;
            }
            before = projection.page.next_before_message_id;
        }
        assert_eq!(recovered.len(), 1_000);
        assert_eq!(recovered.first().map(String::as_str), Some("message-0000"));
        assert_eq!(recovered.last().map(String::as_str), Some("message-0999"));
        assert!(project_snapshot_message_page(Vec::new(), None, Some(101)).is_err());
    }
}
