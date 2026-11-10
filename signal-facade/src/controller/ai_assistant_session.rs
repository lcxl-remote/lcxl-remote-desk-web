//! Shared browser/mobile wire DTOs and pure projections for AI Assistant sessions.

mod subagents;
pub use subagents::*;

pub use desk_agent_protocol::ai_assistant::{MainRequestReceipt, MainRequestState};
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
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

#[cfg(test)]
const PAGE_BYTES: usize = 256 * 1024;

const EVIDENCE_SUMMARY_SCHEMA_VERSION: u16 = 1;
const MAX_EVIDENCE_NODES: usize = 128;
const MAX_EVIDENCE_RECEIPTS: usize = 32;
pub const DEFAULT_SNAPSHOT_MESSAGE_PAGE_SIZE: usize = 100;
pub const MAX_SNAPSHOT_MESSAGE_PAGE_SIZE: usize = 100;

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantDeviceRosterBody {
    pub conversation: String,
    pub expected_revision: u64,
    pub target_refs: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AiAssistantDeviceRosterQuery {
    pub conversation: String,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantDeviceRosterDto {
    pub session_id: String,
    pub revision: u64,
    pub target_refs: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AiAssistantMainRequestQuery {
    pub connection: Option<String>,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub request: Option<String>,
}

impl AiAssistantMainRequestQuery {
    pub fn session_selector(&self) -> Option<AiAssistantSnapshotSelection<'_>> {
        use AiAssistantSnapshotSelection::{DeviceIntent, PersonalIntent, Saved};
        match (
            self.session.as_deref(),
            self.conversation.as_deref(),
            self.connection.as_deref(),
        ) {
            (Some(session), None, None) if desk_diagnose_core::subagent::valid_id(session) => {
                Some(Saved { session })
            }
            (None, Some(intent), connection)
                if desk_diagnose_core::conversation_key::is_valid_client_conversation_id(
                    intent.trim(),
                ) =>
            {
                let conversation = intent.trim();
                Some(match connection {
                    Some(connection) => DeviceIntent {
                        connection,
                        conversation,
                    },
                    None => PersonalIntent { conversation },
                })
            }
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantMainRequestsDto {
    pub session_id: String,
    pub items: Vec<MainRequestReceipt>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantMainRequestCancelBody {
    pub session: String,
    pub request_id: String,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantMainRequestResumeBody {
    pub session: String,
    pub expected_control_revision: u64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantMainRequestResumeDto {
    pub control_revision: u64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantSessionQuery {
    pub connection: Option<String>,
    pub scheduled_task: Option<String>,
    pub scheduled_run: Option<String>,
    pub conversation: Option<String>,
    pub session: Option<String>,
    /// Exclusive message cursor. Omit for the newest page.
    pub message_before: Option<String>,
    /// Original native message anchor; mutually exclusive with message_before.
    pub message_around: Option<String>,
    pub message_limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiAssistantSnapshotSelection<'a> {
    Scheduled {
        task: &'a str,
        run: &'a str,
    },
    Saved {
        session: &'a str,
    },
    PersonalIntent {
        conversation: &'a str,
    },
    DeviceIntent {
        connection: &'a str,
        conversation: &'a str,
    },
}

impl AiAssistantSessionQuery {
    /// Schedule selectors cannot be combined with connection/session selectors.
    pub fn snapshot_selection(&self) -> Result<AiAssistantSnapshotSelection<'_>, &'static str> {
        use AiAssistantSnapshotSelection::{DeviceIntent, PersonalIntent, Saved, Scheduled};

        match (
            self.scheduled_task.as_deref(),
            self.scheduled_run.as_deref(),
        ) {
            (Some(task), Some(run))
                if self.connection.is_none()
                    && self.conversation.is_none()
                    && self.session.is_none()
                    && !task.is_empty()
                    && task.len() <= 256
                    && !run.is_empty()
                    && run.len() <= 256 =>
            {
                Ok(Scheduled { task, run })
            }
            (None, None) => {
                if let Some(session) = self.session.as_deref().filter(|id| !id.is_empty()) {
                    return Ok(Saved { session });
                }
                let conversation = self
                    .conversation
                    .as_deref()
                    .ok_or("missing conversation intent")?;
                if let Some(connection) = self.connection.as_deref() {
                    return Ok(DeviceIntent {
                        connection,
                        conversation,
                    });
                }
                let conversation = conversation.trim();
                if !desk_diagnose_core::conversation_key::is_valid_client_conversation_id(
                    conversation,
                ) {
                    return Err("invalid personal conversation intent");
                }
                Ok(PersonalIntent { conversation })
            }
            _ => Err("invalid scheduled conversation selector"),
        }
    }
}

pub fn selected_permission_session_id(
    actor_id: &str,
    device_id: &str,
    session: Option<&str>,
    conversation: Option<&str>,
) -> Option<String> {
    use desk_diagnose_core::conversation_key::{
        AssistantConversationScope, derive_assistant_conversation_key, derive_conversation_key,
        is_valid_client_conversation_id,
    };
    if let Some(id) = session.filter(|id| !id.is_empty()) {
        return Some(id.to_owned());
    }
    let intent = conversation?;
    if !device_id.is_empty() {
        return Some(derive_conversation_key(
            actor_id,
            device_id,
            Some(intent),
            "",
        ));
    }
    let intent = intent.trim();
    is_valid_client_conversation_id(intent).then(|| {
        derive_assistant_conversation_key(actor_id, AssistantConversationScope::Personal, intent)
    })
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantSessionListQuery {
    pub connection: Option<String>,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct DeleteAiAssistantSessionBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    pub session: String,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct DeleteAiAssistantSessionResponse {
    pub deleted: bool,
    /// Cleanup intent is durable; physical removal awaits a device acknowledgment.
    pub backup_cleanup_pending: bool,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDecisionBody {
    /// Omit for an owner conversation without a device target.
    pub connection: Option<String>,
    pub conversation: Option<String>,
    pub session: Option<String>,
    /// Optional concurrency fence: snapshot.requestId, or the selected scheduled
    /// run ID. Distinct from the permission request ID below. A mismatch rejects
    /// the submission, including a replay, without issuing grants.
    pub expected_run_request_id: Option<String>,
    pub request_id: String,
    pub items: Vec<PermissionDecisionItemBody>,
    /// Ready terminal carrier of the approving client. Required to approve an
    /// interactive (PTY) command, which is always requested alone; rejected
    /// for any other approval. Rejecting a PTY command needs no carrier.
    #[serde(default)]
    pub carrier_id: Option<String>,
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
    /// Omit for an owner conversation without a device target.
    pub connection: Option<String>,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub grant_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDelegationOpenBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub expected_input_revision: u64,
    /// Client-generated operation identity recorded with the owner's decision.
    pub owner_authorization_id: String,
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDelegationCloseBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub delegation_id: String,
    pub owner_decision_id: String,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDelegationDto {
    pub delegation_id: String,
    pub status: String,
    pub reviews_used: u64,
    pub tokens_used: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_used_micros: Option<u64>,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalModelReadinessDto {
    pub available: bool,
    pub reason: Option<String>,
}

impl From<&desk_diagnose_core::approval_delegation::ApprovalDelegation> for ApprovalDelegationDto {
    fn from(value: &desk_diagnose_core::approval_delegation::ApprovalDelegation) -> Self {
        Self {
            delegation_id: value.delegation_id.clone(),
            status: value.status.as_str().into(),
            reviews_used: value.usage.reviews_used,
            tokens_used: value.usage.tokens_used,
            cost_used_micros: value.usage.cost_used_micros,
            created_at_unix_ms: value.created_at_unix_ms,
        }
    }
}

/// Receipt correlation for independent automation run review, not a conversation write gate.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnknownOutcomeDto {
    pub work_id: i64,
    pub action_request_id: String,
    pub execution_id: String,
    pub work_kind: String,
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
    pub citations: Vec<desk_agent_protocol::ai_assistant::source::AssistantCitation>,
    #[schema(required = false)]
    pub citations_incomplete: bool,
    /// Reviewed readable reasoning; no signatures or opaque provider replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<SnapshotToolCallDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_ok: Option<bool>,
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

impl SnapshotMessagePageProjection {
    /// Keep citations and original tool groups whole. Optional neighboring
    /// groups are paged away; one indivisible group may exceed the page target.
    pub fn with_citations(
        mut self,
        mut projections: std::collections::BTreeMap<
            String,
            desk_diagnose_core::ai_assistant::citation::AnswerCitations,
        >,
        page_bytes: usize,
        anchor: Option<&str>,
    ) -> Result<Self, &'static str> {
        for message in &mut self.messages {
            if message.role != "assistant" {
                continue;
            }
            let projection = projections.remove(&message.id).unwrap_or_else(|| {
                desk_diagnose_core::ai_assistant::citation::for_answer(
                    &message.id,
                    &message.text,
                    &[],
                    &Default::default(),
                )
            });
            message.citations = projection.citations;
            message.citations_incomplete |= projection.incomplete;
        }
        while serde_json::to_vec(&self.messages)
            .map_err(|_| "invalid message page")?
            .len()
            > page_bytes
            && !self.messages.is_empty()
        {
            let mut first_end = 1;
            if self.messages[0].role == "assistant" && !self.messages[0].tool_calls.is_empty() {
                while first_end < self.messages.len() && self.messages[first_end].role == "tool" {
                    first_end += 1;
                }
            }
            if first_end == self.messages.len() {
                break;
            }
            if anchor.is_some_and(|id| {
                self.messages[..first_end]
                    .iter()
                    .any(|message| message.id == id)
            }) {
                self.messages.truncate(first_end);
                break;
            }
            self.messages.drain(..first_end);
            self.page.has_more = true;
        }
        self.page.next_before_message_id = self
            .page
            .has_more
            .then(|| self.messages.first().map(|message| message.id.clone()))
            .flatten();
        Ok(self)
    }
}

pub fn project_snapshot_message_page(
    messages: Vec<ChatMessage>,
    before_message_id: Option<&str>,
    requested_limit: Option<usize>,
    page_bytes: usize,
) -> Result<SnapshotMessagePageProjection, &'static str> {
    let limit = requested_limit.unwrap_or(DEFAULT_SNAPSHOT_MESSAGE_PAGE_SIZE);
    if limit == 0 || limit > MAX_SNAPSHOT_MESSAGE_PAGE_SIZE {
        return Err("invalid message page limit");
    }
    let visible = messages
        .into_iter()
        .filter(visible_snapshot_message)
        .collect::<Vec<_>>();
    let end = match before_message_id {
        Some(cursor) => visible
            .iter()
            .position(|message| message.message_id == cursor)
            .ok_or("invalid message page cursor")?,
        None => visible.len(),
    };
    let groups = snapshot_message_groups(&visible);
    if end != visible.len() && !groups.iter().any(|(start, _)| *start == end) {
        return Err("message page cursor is not a group boundary");
    }
    let mut start = end;
    let mut count = 0usize;
    let mut bytes = 2usize;
    let mut selected_groups = Vec::new();
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
        if count + group_count > limit {
            if start == end {
                return Err("one transcript message group exceeds the page count limit");
            }
            break;
        }
        if start != end && bytes.saturating_add(group_bytes) > page_bytes {
            break;
        }
        start = group_start;
        count += group_count;
        bytes = bytes.saturating_add(group_bytes);
        selected_groups.push(group);
    }
    let has_more = start > 0;
    let next_before_message_id = has_more.then(|| visible[start].message_id.clone());
    Ok(SnapshotMessagePageProjection {
        messages: selected_groups.into_iter().rev().flatten().collect(),
        page: SnapshotMessagePageDto {
            has_more,
            next_before_message_id,
            limit,
        },
    })
}

fn visible_snapshot_message(message: &ChatMessage) -> bool {
    if desk_diagnose_core::permission_resume::is_permission_resume_message(message) {
        return false;
    }
    if message.role != desk_diagnose_core::chat::ChatRole::SystemEvent {
        return true;
    }
    !message.data_envelope.as_ref().is_some_and(|envelope| {
        envelope.provenance.source_provider_id
            == desk_diagnose_core::dynamic_run::RUN_CONTROL_PROVIDER_ID
            && envelope.provenance.source_tool_name == "owner_context_time"
    })
}

fn snapshot_message_groups(visible: &[ChatMessage]) -> Vec<(usize, usize)> {
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
    groups
}

/// Locate an original message inside a bounded owner-only history window.
/// Native tool groups stay whole; surrounding oversized groups are optional.
/// This projection grants no model read, device connection or tool invocation.
pub fn project_snapshot_message_window(
    messages: Vec<ChatMessage>,
    before_message_id: Option<&str>,
    around_message_id: Option<&str>,
    requested_limit: Option<usize>,
    page_bytes: usize,
) -> Result<SnapshotMessagePageProjection, &'static str> {
    let Some(anchor) = around_message_id else {
        return project_snapshot_message_page(
            messages,
            before_message_id,
            requested_limit,
            page_bytes,
        );
    };
    let limit = requested_limit.unwrap_or(DEFAULT_SNAPSHOT_MESSAGE_PAGE_SIZE);
    if before_message_id.is_some() || limit == 0 || limit > MAX_SNAPSHOT_MESSAGE_PAGE_SIZE {
        return Err("invalid message window selector or limit");
    }
    desk_agent_protocol::ai_assistant::source::validate_source_id(anchor)?;
    let visible = messages
        .into_iter()
        .filter(visible_snapshot_message)
        .collect::<Vec<_>>();
    let mut found = visible
        .iter()
        .enumerate()
        .filter(|(_, message)| message.message_id == anchor);
    let target = found
        .next()
        .ok_or("message window anchor is unavailable")?
        .0;
    if found.next().is_some() {
        return Err("duplicate message window anchor");
    }
    let groups = snapshot_message_groups(&visible);
    let group = groups
        .iter()
        .position(|(start, end)| *start <= target && target < *end)
        .unwrap();
    let (mut left, end) = groups[group];
    let mut count = end - left;
    if count > limit {
        return Err("message window anchor group exceeds its limit");
    }
    for (start, end) in groups[..group].iter().copied().rev() {
        if count + end - start > limit {
            break;
        }
        left = start;
        count += end - start;
    }
    let mut page =
        project_snapshot_message_page(visible[left..end].to_vec(), None, Some(limit), page_bytes)?;
    let mut following = 0;
    for (start, end) in groups[group + 1..].iter().copied() {
        if following + end - start > limit / 2 {
            break;
        }
        let Ok(next) = project_snapshot_message_page(
            visible[left..end].to_vec(),
            None,
            Some(limit),
            page_bytes,
        ) else {
            break;
        };
        if !next.messages.iter().any(|message| message.id == anchor) {
            break;
        }
        page = next;
        following += end - start;
    }
    page.page.has_more |= left > 0;
    page.page.next_before_message_id = page.page.has_more.then(|| page.messages[0].id.clone());
    Ok(page)
}

impl From<ChatMessage> for SnapshotMessageDto {
    fn from(message: ChatMessage) -> Self {
        Self {
            id: message.message_id,
            turn_id: message.turn_id,
            role: message.role.as_str().to_string(),
            reasoning: if message.role == desk_diagnose_core::chat::ChatRole::Assistant {
                message.reasoning
            } else {
                None
            },
            text: message.text,
            citations: Vec::new(),
            citations_incomplete: false,
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
            tool_ok: message.tool_ok,
            background_task_id: message.background_task_id,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextNoticeKindDto {
    Refreshed,
    Restricted,
    Trimmed,
    Compacted,
    AutomationLimit,
}

impl From<desk_diagnose_core::model_context::ContextNoticeKind> for ContextNoticeKindDto {
    fn from(kind: desk_diagnose_core::model_context::ContextNoticeKind) -> Self {
        match kind {
            desk_diagnose_core::model_context::ContextNoticeKind::Refreshed => Self::Refreshed,
            desk_diagnose_core::model_context::ContextNoticeKind::Restricted => Self::Restricted,
            desk_diagnose_core::model_context::ContextNoticeKind::Trimmed => Self::Trimmed,
            desk_diagnose_core::model_context::ContextNoticeKind::Compacted => Self::Compacted,
            desk_diagnose_core::model_context::ContextNoticeKind::AutomationLimit => {
                Self::AutomationLimit
            }
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextNoticeDto {
    pub created_at: Option<String>,
    pub after_message_id: Option<String>,
    pub id: String,
    pub turn_id: String,
    pub kind: ContextNoticeKindDto,
    pub checkpoint_generation: Option<u32>,
    pub covered_message_count: Option<u32>,
}

impl From<desk_diagnose_core::model_context::ContextNotice> for ContextNoticeDto {
    fn from(notice: desk_diagnose_core::model_context::ContextNotice) -> Self {
        Self {
            created_at: notice.created_at,
            after_message_id: notice.after_message_id,
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
    pub device_ref: Option<String>,
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
            device_ref: Some(attachment.device_id),
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
pub struct AiAssistantSessionSnapshotDto {
    /// None until the first owner input freezes this conversation's mode.
    #[schema(required = false)]
    pub temporary_conversation: Option<bool>,
    pub control_revision: u64,
    pub main_stopped: bool,
    pub subagents: desk_agent_protocol::ai_assistant::subagent::AiAssistantDelegationSnapshot,
    /// Original permission request reasons, keyed by the server-bound work ID.
    pub action_permission_reasons: std::collections::BTreeMap<String, String>,
    pub file_scope: FileScopeDto,
    /// Persisted owner-visible failure for the current terminal turn.
    pub terminal_error: Option<desk_agent_protocol::AgentError>,
    /// Server-owned durable goal status; independent of the current turn.
    pub goal: Option<AiAssistantGoalDto>,
    /// An AI proposal remains inert until the owner decides this exact request.
    pub pending_goal_open_request: Option<AiAssistantGoalOpenRequestDto>,
    /// Current/latest owner delegation; it does not grant any tool authority.
    pub approval_delegation: Option<ApprovalDelegationDto>,
    pub approval_model_readiness: ApprovalModelReadinessDto,

    pub context_usage: Option<ContextUsageDto>,
    /// Opaque recovery selector; ownership is rechecked on every read or stop.
    pub session_id: String,
    /// Monotonic session-store snapshot version used for out-of-order reconciliation.
    pub seq: i64,
    /// Whether the persisted turn is still running or awaiting approval.
    pub active: bool,
    /// AI Assistant request currently represented by the persisted turn.
    pub request_id: Option<String>,
    /// Running background command generation that may be cancelled.
    pub active_execution_generation: Option<String>,
    /// Inconclusive receipt identity for independent automation history.
    pub unresolved_outcome: Option<UnknownOutcomeDto>,
    /// Latest durably accepted user input and the model-processing watermark.
    pub latest_input_seq: u64,
    pub input_revision: u64,
    pub handled_input_seq: u64,
    /// Pending main requests have their own receipt even before native activation.
    pub main_requests: Vec<MainRequestReceipt>,
    /// AI-maintained UX projection; never an authorization or execution fact.
    pub task_status_projection: Option<TaskStatusProjectionDto>,
    /// Model-proposed, server-normalized requests. Pending is approvable;
    /// NeedsRevalidation is display-only until the model replaces/reissues it.
    pub permission_requests: Vec<PermissionRequestDto>,
    /// Durable Provider executions that outlived their foreground wait.
    pub background_tasks: Vec<BackgroundTaskDto>,
    /// Persisted command executions, including terminal results after reconnect.
    pub command_tasks: Vec<CommandTaskDto>,
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

#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantGoalDto {
    pub goal_id: String,
    pub previous_completed_goal_id: Option<String>,
    pub previous_completion_summary: Option<String>,
    pub goal_revision: u64,
    pub goal_text: String,
    pub device_id: String,
    pub state: String,
    pub state_version: u64,
    pub status_reason: Option<String>,
    pub pause_reason: Option<String>,
    pub slice_seq: u32,
    pub used_slices: u32,
    #[schema(required = true)]
    pub limit_slices: Option<u32>,
    pub used_model_tokens: u64,
    #[schema(required = true)]
    pub limit_model_tokens: Option<u64>,
    pub used_model_calls: u32,
    pub used_tool_calls: u32,
    pub used_active_time_ms: u64,
    pub limits: AiAssistantGoalLimitsDto,
    pub created_at_unix_ms: u64,
    #[schema(required = true)]
    pub deadline_unix_ms: Option<u64>,
    pub next_attempt_unix_ms: Option<u64>,
    pub updated_at_unix_ms: u64,
    pub checkpoint_summary: Option<String>,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantGoalOpenRequestDto {
    pub request_id: String,
    pub goal_text: String,
    pub previous_completed_goal_id: Option<String>,
    pub target_goal_id: Option<String>,
    pub target_goal_revision: Option<u64>,
    pub device_id: String,
    pub input_revision: u64,
    pub limits: AiAssistantGoalLimitsDto,
    pub expires_at_unix_ms: u64,
}

impl From<&desk_diagnose_core::goal::GoalOpenRequest> for AiAssistantGoalOpenRequestDto {
    fn from(request: &desk_diagnose_core::goal::GoalOpenRequest) -> Self {
        Self {
            request_id: request.request_id.clone(),
            goal_text: request.goal_text.clone(),
            previous_completed_goal_id: request.previous_completed_goal_id.clone(),
            target_goal_id: request.target_goal_id.clone(),
            target_goal_revision: request.target_goal_revision,
            device_id: request.execution_target.device_id.clone(),
            input_revision: request.input_revision,
            limits: request.limits.into(),
            expires_at_unix_ms: request.expires_at_unix_ms,
        }
    }
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantGoalOpenDecisionBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub request_id: String,
    pub approve: bool,
}

#[derive(Clone, Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantGoalOpenDecisionDto {
    pub request_id: String,
    pub state: String,
    pub goal: Option<AiAssistantGoalDto>,
}

impl AiAssistantGoalOpenDecisionDto {
    pub fn from_result(
        request: &desk_diagnose_core::goal::GoalOpenRequest,
        goal: Option<&desk_diagnose_core::goal::GoalRun>,
    ) -> Self {
        Self {
            request_id: request.request_id.clone(),
            state: request.state.as_str().to_owned(),
            goal: goal.map(AiAssistantGoalDto::from),
        }
    }
}

#[derive(Clone, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantGoalControlBody {
    pub connection: String,
    pub conversation: Option<String>,
    pub session: Option<String>,
    pub goal_id: String,
    pub expected_state_version: u64,
    pub action: AiAssistantGoalOwnerAction,
}

#[derive(Clone, Copy, Debug, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantGoalOwnerAction {
    Pause,
    Resume,
    RetryStalled,
    Cancel,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantGoalLimitsDto {
    #[schema(required = true)]
    pub active_time_ms: Option<u64>,
    #[schema(required = true)]
    pub model_tokens: Option<u64>,
    #[schema(required = true)]
    pub model_calls: Option<u32>,
    #[schema(required = true)]
    pub tool_calls: Option<u32>,
    #[schema(required = true)]
    pub slices: Option<u32>,
    #[schema(required = true)]
    pub stalled_slices: Option<u32>,
}

impl From<desk_diagnose_core::goal::GoalLimits> for AiAssistantGoalLimitsDto {
    fn from(value: desk_diagnose_core::goal::GoalLimits) -> Self {
        Self {
            active_time_ms: (value.active_time_ms != u64::MAX).then_some(value.active_time_ms),
            model_tokens: (value.model_tokens != u64::MAX).then_some(value.model_tokens),
            model_calls: (value.model_calls != u32::MAX).then_some(value.model_calls),
            tool_calls: (value.tool_calls != u32::MAX).then_some(value.tool_calls),
            slices: (value.slices != u32::MAX).then_some(value.slices),
            stalled_slices: (value.stalled_slices != u32::MAX).then_some(value.stalled_slices),
        }
    }
}

impl AiAssistantGoalControlBody {
    pub fn owner_action(&self) -> desk_diagnose_core::goal::GoalOwnerAction {
        use desk_diagnose_core::goal::GoalOwnerAction;
        match self.action {
            AiAssistantGoalOwnerAction::Pause => GoalOwnerAction::Pause,
            AiAssistantGoalOwnerAction::Resume => GoalOwnerAction::Resume,
            AiAssistantGoalOwnerAction::RetryStalled => GoalOwnerAction::RetryStalled,
            AiAssistantGoalOwnerAction::Cancel => GoalOwnerAction::Cancel,
        }
    }
}

impl From<&desk_diagnose_core::goal::GoalRun> for AiAssistantGoalDto {
    fn from(goal: &desk_diagnose_core::goal::GoalRun) -> Self {
        Self {
            goal_id: goal.goal_id.clone(),
            previous_completed_goal_id: goal
                .previous_completion
                .as_ref()
                .map(|previous| previous.goal_id.clone()),
            previous_completion_summary: goal
                .previous_completion
                .as_ref()
                .map(|previous| previous.summary.clone()),
            goal_revision: goal.goal_revision,
            goal_text: goal.goal_text.clone(),
            device_id: goal.execution_target.device_id.clone(),
            state: goal.status_code().into(),
            state_version: goal.state_version,
            status_reason: goal.status_reason.clone(),
            pause_reason: match goal.state {
                desk_diagnose_core::goal::GoalState::Paused(reason) => Some(reason.as_str().into()),
                _ => None,
            },
            slice_seq: goal.slice_seq,
            used_slices: goal.used.slices,
            limit_slices: (goal.limits.slices != u32::MAX).then_some(goal.limits.slices),
            used_model_tokens: goal.used.total_tokens().unwrap_or(u64::MAX),
            limit_model_tokens: (goal.limits.model_tokens != u64::MAX)
                .then_some(goal.limits.model_tokens),
            used_model_calls: goal.used.model_calls,
            used_tool_calls: goal.used.tool_calls,
            used_active_time_ms: goal.used.active_time_ms,
            limits: goal.limits.into(),
            created_at_unix_ms: goal.created_at_unix_ms,
            deadline_unix_ms: (goal.deadline_unix_ms != i64::MAX as u64)
                .then_some(goal.deadline_unix_ms),
            next_attempt_unix_ms: goal.next_attempt_unix_ms,
            updated_at_unix_ms: goal.updated_at_unix_ms,
            checkpoint_summary: goal
                .checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.summary.clone()),
        }
    }
}

#[derive(Debug, Default, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileScopeDto {
    pub revision: u64,
    pub directories: Vec<DirectoryConsentDto>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DirectoryConsentDto {
    pub request_id: String,
    pub device_ref: Option<String>,
    pub canonical_path: String,
    pub purpose: String,
    pub state: String,
    pub source: String,
    pub reference_expires_at: String,
    pub decision_actor: Option<String>,
    pub decision_reason: Option<String>,
    pub review_id: Option<String>,
}

impl FileScopeDto {
    pub fn with_device_refs(
        scope: &desk_diagnose_core::file_scope::SessionFileScope,
        device_ref: impl Fn(&str) -> Option<String>,
    ) -> Self {
        use desk_diagnose_core::file_scope::{DirectoryConsentSource, DirectoryConsentState};
        Self {
            revision: scope.revision(),
            directories: scope
                .records()
                .iter()
                .map(|record| DirectoryConsentDto {
                    request_id: record.proposal.request_id.clone(),
                    device_ref: device_ref(&record.device_id),
                    canonical_path: record.proposal.canonical_path.clone(),
                    purpose: record.proposal.purpose.clone(),
                    state: match record.state {
                        DirectoryConsentState::Pending => "pending",
                        DirectoryConsentState::Approved => "approved",
                        DirectoryConsentState::Rejected => "rejected",
                        DirectoryConsentState::Revoked => "revoked",
                    }
                    .into(),
                    source: match record.proposal.source {
                        DirectoryConsentSource::ModelProposal => "model_proposal",
                        DirectoryConsentSource::OwnerSelection => "owner_selection",
                        DirectoryConsentSource::TaskContract => "task_contract",
                    }
                    .into(),
                    reference_expires_at: record.proposal.directory.expires_at.clone(),
                    decision_actor: record.decision.as_ref().map(|decision| match decision.source {
                        desk_diagnose_core::dynamic_run::PermissionDecisionSource::UserDecision => "owner",
                        desk_diagnose_core::dynamic_run::PermissionDecisionSource::AutomaticReview { .. } => "ai_approval",
                        desk_diagnose_core::dynamic_run::PermissionDecisionSource::SystemDecision { .. } | desk_diagnose_core::dynamic_run::PermissionDecisionSource::OwnerNoReview { .. } => "system",
                    }.into()),
                    decision_reason: record.decision.as_ref().map(|decision| decision.reason.clone()),
                    review_id: record.decision.as_ref().and_then(|decision| decision.candidate_id.clone()),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelContextObservationDto {
    pub observed_at_unix_ms: i64,
    pub input_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cleared_thinking_turns: Option<u64>,
    pub cleared_input_tokens: Option<u64>,
    pub request_bytes: Option<u64>,
    pub stale: bool,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageDto {
    pub prepared_request_bytes: Option<u64>,
    pub prepared_context_bytes: Option<u64>,
    pub history_is_estimated: Option<bool>,
    pub latest_model_call: Option<ModelContextObservationDto>,
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub strategy: String,
    pub breakdown: ContextUsageBreakdownDto,
    pub request_budget: Option<ContextRequestBudgetDto>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextRequestBudgetDto {
    pub total_bytes: u64,
    pub system_prompt_bytes: u64,
    pub tool_definitions_bytes: u64,
    pub other_overhead_bytes: u64,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageBreakdownDto {
    pub protocol_bytes: Option<i64>,
    pub messages_bytes: u64,
    pub tools_bytes: u64,
    pub replay_bytes: u64,
    pub projected_bytes: u64,
}

impl From<desk_diagnose_core::context_usage::ContextUsage> for ContextUsageDto {
    fn from(value: desk_diagnose_core::context_usage::ContextUsage) -> Self {
        Self {
            prepared_request_bytes: value.prepared_request_bytes.map(|bytes| bytes as u64),
            prepared_context_bytes: value.prepared_context_bytes.map(|bytes| bytes as u64),
            history_is_estimated: Some(value.history_is_estimated),
            latest_model_call: value
                .latest_model_call
                .map(|call| ModelContextObservationDto {
                    observed_at_unix_ms: call.observed_at_unix_ms,
                    input_tokens: call.input_tokens,
                    reasoning_tokens: call.reasoning_tokens,
                    cleared_thinking_turns: call.cleared_thinking_turns,
                    cleared_input_tokens: call.cleared_input_tokens,
                    request_bytes: call.request_bytes,
                    stale: call.stale,
                }),
            used_bytes: value.used_bytes as u64,
            limit_bytes: value.limit_bytes as u64,
            strategy: value.strategy,
            breakdown: ContextUsageBreakdownDto {
                protocol_bytes: Some(value.breakdown.protocol_bytes),
                messages_bytes: value.breakdown.messages_bytes as u64,
                tools_bytes: value.breakdown.tools_bytes as u64,
                replay_bytes: value.breakdown.replay_bytes as u64,
                projected_bytes: value.breakdown.projected_bytes as u64,
            },
            request_budget: value.request_budget.map(|budget| ContextRequestBudgetDto {
                total_bytes: budget.total_bytes as u64,
                system_prompt_bytes: budget.system_prompt_bytes as u64,
                tool_definitions_bytes: budget.tool_definitions_bytes as u64,
                other_overhead_bytes: budget.other_overhead_bytes as u64,
            }),
        }
    }
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
        ContentRef::Message {
            size_bytes,
            media_type,
            ..
        } => ("message", None, *size_bytes, Some(media_type.clone())),
        ContentRef::Projection {
            size_bytes,
            media_type,
            ..
        } => ("projection", None, *size_bytes, Some(media_type.clone())),
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
pub struct CommandTaskDto {
    pub device_id: String,
    pub task_id: String,
    pub call_id: String,
    pub execution_generation: String,
    pub state: BackgroundTaskStateDto,
    pub updated_at: String,
    pub result: Option<String>,
    pub result_truncated: bool,
}

impl CommandTaskDto {
    pub fn project(
        device_id: String,
        task_id: String,
        call_id: String,
        execution_generation: String,
        status: &str,
        updated_at: String,
        outcome: Option<desk_agent_protocol::AgentOutcome>,
    ) -> Self {
        use desk_agent_protocol::{AgentErrorKind, AgentOutcome, OperationOutput};
        let state = match &outcome {
            Some(AgentOutcome::Err(error)) if error.kind == AgentErrorKind::Cancelled => {
                BackgroundTaskStateDto::Cancelled
            }
            Some(AgentOutcome::Err(_)) => BackgroundTaskStateDto::Failed,
            Some(AgentOutcome::Ok(OperationOutput::Exec(output))) if output.outcome_unknown() => {
                BackgroundTaskStateDto::OutcomeUnknown
            }
            Some(AgentOutcome::Ok(OperationOutput::Exec(output)))
                if output
                    .failure
                    .as_ref()
                    .is_some_and(|e| e.kind == AgentErrorKind::Cancelled) =>
            {
                BackgroundTaskStateDto::Cancelled
            }
            Some(AgentOutcome::Ok(OperationOutput::Exec(output))) if !output.succeeded() => {
                BackgroundTaskStateDto::Failed
            }
            Some(AgentOutcome::Ok(_)) => BackgroundTaskStateDto::Succeeded,
            None => match status {
                "running" | "dispatching" | "dispatched" => BackgroundTaskStateDto::Running,
                "cancel_requested" => BackgroundTaskStateDto::CancelRequested,
                "cancelled" => BackgroundTaskStateDto::Cancelled,
                "failed" | "rejected" | "expired" => BackgroundTaskStateDto::Failed,
                _ => BackgroundTaskStateDto::OutcomeUnknown,
            },
        };
        let mut result = outcome.and_then(|outcome| serde_json::to_string_pretty(&outcome).ok());
        let mut result_truncated = false;
        if let Some(text) = result.as_mut() {
            const MAX_RESULT_BYTES: usize = 16 * 1024;
            if text.len() > MAX_RESULT_BYTES {
                let mut end = MAX_RESULT_BYTES;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                result_truncated = true;
            }
        }
        Self {
            device_id,
            task_id,
            call_id,
            execution_generation,
            state,
            updated_at,
            result,
            result_truncated,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskDto {
    pub device_id: String,
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
            device_id: record.device_id,
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
    pub selected_read_device_ref: Option<String>,
    pub external_send_confirmation: Option<ExternalSendConfirmationDto>,
    pub command_confirmation: Option<CommandConfirmationDto>,
    pub launch_confirmation: Option<LaunchConfirmationDto>,
    pub application_scope: Option<desk_agent_protocol::computer_use::UiApplicationScope>,
    pub text_file_confirmation: Option<TextFileConfirmationDto>,
    pub wayland_output_confirmation: Option<WaylandOutputConfirmationDto>,
}

mod wayland_output_confirmation;
pub use wayland_output_confirmation::WaylandOutputConfirmationDto;

mod text_file_confirmation;
pub use text_file_confirmation::TextFileConfirmationDto;

impl PermissionRequestDto {
    pub fn with_decision(
        mut self,
        event: Option<&desk_diagnose_core::dynamic_run::PermissionDecidedEvent>,
    ) -> Self {
        use desk_diagnose_core::dynamic_run::{PermissionDecisionSource, PermissionItemDecision};
        self.decision = event
            .filter(|event| event.request_id == self.request_id)
            .map(|event| {
                let source = match event.decision_source.actor() {
                    desk_diagnose_core::dynamic_run::PermissionDecisionActor::Owner => {
                        PermissionDecisionSourceDto::Owner
                    }
                    desk_diagnose_core::dynamic_run::PermissionDecisionActor::OwnerNoReview => {
                        PermissionDecisionSourceDto::OwnerNoReview
                    }
                    desk_diagnose_core::dynamic_run::PermissionDecisionActor::AiApproval => {
                        PermissionDecisionSourceDto::AiApproval
                    }
                    desk_diagnose_core::dynamic_run::PermissionDecisionActor::System => {
                        PermissionDecisionSourceDto::System
                    }
                    desk_diagnose_core::dynamic_run::PermissionDecisionActor::Mixed => {
                        PermissionDecisionSourceDto::MixedReview
                    }
                };
                let (reviews, failures, system_reason) = match &event.decision_source {
                    PermissionDecisionSource::UserDecision
                    | PermissionDecisionSource::OwnerNoReview { .. } => (None, None, None),
                    PermissionDecisionSource::AutomaticReview {
                        reviews, failures, ..
                    } => (Some(reviews.as_slice()), Some(failures.as_slice()), None),
                    PermissionDecisionSource::SystemDecision {
                        reason_code,
                        reason,
                    } => (None, None, Some((reason_code, reason))),
                };
                PermissionDecisionSummaryDto {
                    source,
                    decided_at: event.event.created_at.clone(),
                    items: event
                        .items
                        .iter()
                        .map(|item| {
                            let review = reviews.and_then(|reviews| {
                                reviews.iter().find(|review| review.item_id == item.item_id)
                            });
                            let failure = failures.and_then(|rows| {
                                rows.iter().find(|row| row.item_id == item.item_id)
                            });
                            let approved =
                                matches!(&item.decision, PermissionItemDecision::Approve { .. });
                            let reason_code = review
                                .map(|r| r.reason_code.clone())
                                .or_else(|| failure.map(|f| f.reason_code.clone()))
                                .or_else(|| system_reason.map(|(code, _)| code.to_string()));
                            PermissionItemDecisionSummaryDto {
                                source: if failure.is_some() || system_reason.is_some() {
                                    PermissionDecisionSourceDto::System
                                } else if review.is_some() {
                                    PermissionDecisionSourceDto::AiApproval
                                } else if matches!(
                                    source,
                                    PermissionDecisionSourceDto::OwnerNoReview
                                ) {
                                    PermissionDecisionSourceDto::OwnerNoReview
                                } else {
                                    PermissionDecisionSourceDto::Owner
                                },
                                outcome: if approved {
                                    "approved"
                                } else if failure.is_some() || system_reason.is_some() {
                                    desk_diagnose_core::approval_review::system_review_outcome(
                                        reason_code.as_deref().unwrap_or("approval_processing"),
                                    )
                                } else {
                                    "denied"
                                }
                                .into(),
                                item_id: item.item_id.clone(),
                                approved,
                                reason_code,
                                reason: review
                                    .map(|r| r.reason.clone())
                                    .or_else(|| failure.map(|f| f.reason.clone()))
                                    .or_else(|| {
                                        system_reason.map(|(_, reason)| reason.to_string())
                                    }),
                            }
                        })
                        .collect(),
                }
            });
        self
    }

    /// Projects a stored request for the owner. `run_id` binds interactive
    /// commands to their derived execution id.
    pub fn with_file_evidence(
        request: desk_diagnose_core::dynamic_run::PermissionRequest,
        run_id: &str,
        messages: &[desk_diagnose_core::chat::ChatMessage],
    ) -> Self {
        let interactive_ids = request
            .items
            .iter()
            .map(|item| {
                item.command_confirmation
                    .as_ref()
                    .filter(|confirmation| confirmation.is_interactive())
                    .map(|confirmation| {
                        desk_diagnose_core::command_confirmation::interactive_exec_request_id(
                            run_id,
                            &request.request_id,
                            &confirmation.canonical_input_digest_sha256,
                        )
                    })
            })
            .collect::<Vec<_>>();
        let reviews = request
            .items
            .iter()
            .map(|item| {
                if item.provider_id != desk_diagnose_core::ai_assistant::TEXT_FILE_PROVIDER_ID
                    || item.validate().is_err()
                {
                    return None;
                }
                text_file_confirmation::project(
                    &item.tool_name,
                    item.canonical_input_json.as_deref(),
                    messages,
                )
            })
            .collect::<Vec<_>>();
        let mut dto = Self::from(request);
        for (item, review) in dto.items.iter_mut().zip(reviews) {
            item.text_file_confirmation = review;
        }
        for (item, exec_request_id) in dto.items.iter_mut().zip(interactive_ids) {
            if let (Some(interactive), Some(exec_request_id)) = (
                item.command_confirmation
                    .as_mut()
                    .and_then(|confirmation| confirmation.interactive.as_mut()),
                exec_request_id,
            ) {
                interactive.exec_request_id = exec_request_id;
            }
        }
        dto
    }
}

/// Owner-visible native launch identity; no opaque approval token is exposed.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LaunchConfirmationDto {
    pub target: desk_agent_protocol::application_launch::ApplicationTarget,
    pub resolved_target: String,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub run_as_admin: bool,
    pub target_device_id: String,
    pub target_session_id: String,
    pub one_shot: bool,
}

/// Owner-visible projection of the exact persisted command plan.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CommandConfirmationDto {
    pub command: String,
    pub shell: String,
    pub cwd: Option<String>,
    pub target_device_id: String,
    pub target_session_id: String,
    pub timeout_ms: u32,
    pub max_stdout_bytes: u32,
    pub max_stderr_bytes: u32,
    pub execution_basis: desk_agent_protocol::exec::ExecExecutionBasis,
    pub one_shot: bool,
    /// Present for an interactive (PTY) command: the client opens a terminal
    /// carrier for exactly this execution before approving.
    pub interactive: Option<InteractiveCommandDto>,
}

/// Binding a client needs to prepare the terminal carrier of a pending
/// interactive command.
#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveCommandDto {
    /// Server-derived execution id the carrier is prepared for.
    pub exec_request_id: String,
    /// Host connection the command runs on.
    pub target_connection_id: String,
    /// The command runs `sudo`/`doas`; the owner may be asked for a password.
    pub elevated: bool,
}

fn external_send_confirmation(
    tool_name: &str,
    canonical_input_json: Option<&str>,
) -> Option<ExternalSendConfirmationDto> {
    let canonical_input_json = canonical_input_json?;
    let (snapshot, body_plain_text) = match tool_name {
        "send_gmail_message" => {
            let input: GmailWebExactSendInput = serde_json::from_str(canonical_input_json).ok()?;
            desk_diagnose_core::communication::verify_gmail_web_exact_send_input(&input).ok()?;
            (
                input.handoff.send_payload_snapshot?,
                input.draft.body_plain_text,
            )
        }
        "send_slack_message" => {
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
    pub decision: Option<PermissionDecisionSummaryDto>,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionDecisionSummaryDto {
    pub source: PermissionDecisionSourceDto,
    pub decided_at: String,
    pub items: Vec<PermissionItemDecisionSummaryDto>,
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecisionSourceDto {
    Owner,
    OwnerNoReview,
    AiApproval,
    System,
    MixedReview,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PermissionItemDecisionSummaryDto {
    pub source: PermissionDecisionSourceDto,
    pub outcome: String,
    pub item_id: String,
    pub approved: bool,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
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
                    let launch_confirmation = item
                        .launch_confirmation
                        .as_ref()
                        .filter(|_| {
                            item.tool_name == "launch_application" && item.validate().is_ok()
                        })
                        .map(|binding| LaunchConfirmationDto {
                            target: binding.request().target.clone(),
                            resolved_target: binding.identity().canonical_target.clone(),
                            args: binding.request().args.clone(),
                            cwd: binding.identity().resolved_cwd.clone(),
                            run_as_admin: binding.request().run_as_admin,
                            target_device_id: binding.subject().device_id.clone(),
                            target_session_id: binding.subject().session_id.clone(),
                            one_shot: true,
                        });
                    let command_confirmation =
                        item.command_confirmation
                            .as_ref()
                            .filter(|confirmation| {
                                item.tool_name
                                    == desk_diagnose_core::command_confirmation::COMMAND_TOOL
                                    && item.canonical_input_json.as_deref().is_some_and(
                                        |canonical| confirmation.validate(canonical).is_ok(),
                                    )
                            })
                            .map(|confirmation| CommandConfirmationDto {
                                command: confirmation.proposal.command.clone(),
                                shell: confirmation.proposal.shell.clone(),
                                cwd: confirmation.plan.cwd.clone(),
                                target_device_id: confirmation.target_device_id.clone(),
                                target_session_id: confirmation.target_session_id.clone(),
                                timeout_ms: confirmation.plan.timeout_ms,
                                max_stdout_bytes: confirmation.plan.max_stdout_bytes,
                                max_stderr_bytes: confirmation.plan.max_stderr_bytes,
                                execution_basis: confirmation.plan.execution_basis,
                                one_shot: true,
                                interactive: confirmation.is_interactive().then(|| {
                                    InteractiveCommandDto {
                                        exec_request_id: String::new(),
                                        target_connection_id: confirmation
                                            .target_connection_id()
                                            .to_owned(),
                                        elevated: confirmation.is_elevated(),
                                    }
                                }),
                            });
                    let application_scope = desk_diagnose_core::application_ui::from_canonical(
                        &item.tool_name,
                        item.canonical_input_json.as_deref(),
                    );
                    let wayland_output_confirmation = wayland_output_confirmation::project(&item);
                    GrantRequestItemDto {
                        wayland_output_confirmation,
                        application_scope,
                        item_id: item.item_id,
                        provider_id: item.provider_id,
                        tool_name: item.tool_name,
                        expected_effect: item.expected_effect,
                        resource_scope: item.resource_scope,
                        operation_scope: item.operation_scope,
                        export_destinations: item.export_destinations,
                        suggested_ttl_seconds: item.suggested_ttl_seconds,
                        suggested_max_uses: item.suggested_max_uses,
                        selected_read_device_ref: item
                            .selected_device_target
                            .map(|target| target.target_ref),
                        reason: item.reason,
                        external_send_confirmation,
                        command_confirmation,
                        launch_confirmation,
                        text_file_confirmation: None,
                    }
                })
                .collect(),
            created_at: request.created_at,
            decision: None,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantSessionSummaryDto {
    /// Opaque server-side selector. Authorization is rechecked when it is used.
    pub session_id: String,
    /// Validated client continuation intent.
    pub conversation_id: Option<String>,
    pub title: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub active: bool,
    pub message_count: usize,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantSessionListDto {
    pub sessions: Vec<AiAssistantSessionSummaryDto>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AiAssistantAttentionQuery {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// Durable owner action needed outside the currently open conversation. The
/// identifier changes with the authoritative state version, so clients can
/// deduplicate reminders without persisting a second copy of session content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AiAssistantAttentionReason {
    GoalOpenApproval,
    PermissionApproval,
    GoalNeedsInput,
    GoalBudget,
    GoalStalled,
    GoalBlocked,
    GoalDeadlineSoon,
    SubAgentResult,
    WorkerQuestion,
    IssueReplyNeedsAttention,
}

impl AiAssistantAttentionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GoalOpenApproval => "goal_open_approval",
            Self::PermissionApproval => "permission_approval",
            Self::GoalNeedsInput => "goal_needs_input",
            Self::GoalBudget => "goal_budget",
            Self::GoalStalled => "goal_stalled",
            Self::GoalBlocked => "goal_blocked",
            Self::GoalDeadlineSoon => "goal_deadline_soon",
            Self::SubAgentResult => "subagent_result",
            Self::WorkerQuestion => "worker_question",
            Self::IssueReplyNeedsAttention => "issue_reply_needs_attention",
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantAttentionItemDto {
    pub attention_id: String,
    pub session_id: String,
    pub client_conversation_id: Option<String>,
    pub device_id: String,
    pub goal_id: Option<String>,
    pub request_id: Option<String>,
    pub reason: AiAssistantAttentionReason,
    pub state_version: u64,
    pub updated_at_unix_ms: u64,
    pub deadline_unix_ms: Option<u64>,
    pub workbench: Option<AiAssistantAttentionWorkRef>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantAttentionWorkRef {
    pub workbench_id: String,
    pub request_id: String,
    pub worker_task_id: Option<String>,
}

impl AiAssistantAttentionWorkRef {
    pub fn from_session(
        parent: &desk_diagnose_core::session::PersistedAgentSession,
        worker_task_id: Option<String>,
    ) -> Option<Self> {
        let workbench_id = parent.agent_role.workbench_id()?;
        let request_id = parent.agent_role.workbench_request_id()?;
        (parent.conversation_id
            == desk_diagnose_core::native_ids::workbench_work_conversation(
                &parent.actor_id,
                workbench_id,
                request_id,
            ))
        .then(|| Self {
            workbench_id: workbench_id.into(),
            request_id: request_id.into(),
            worker_task_id,
        })
    }
}

impl AiAssistantAttentionItemDto {
    pub fn from_issue_reply(
        actor: &str,
        workbench: &str,
        request: &str,
        artifact: &str,
        updated_at: i64,
    ) -> Self {
        let updated_at = u64::try_from(updated_at).unwrap_or_default();
        let identity = format!("issue-reply:{artifact}:{updated_at}");
        Self {
            attention_id: format!("attention-{:x}", Sha256::digest(identity.as_bytes())),
            session_id: desk_diagnose_core::native_ids::workbench_work_conversation(
                actor, workbench, request,
            ),
            client_conversation_id: None,
            device_id: String::new(),
            goal_id: None,
            request_id: Some(artifact.to_owned()),
            reason: AiAssistantAttentionReason::IssueReplyNeedsAttention,
            state_version: updated_at,
            updated_at_unix_ms: updated_at,
            deadline_unix_ms: None,
            workbench: Some(AiAssistantAttentionWorkRef {
                workbench_id: workbench.to_owned(),
                request_id: request.to_owned(),
                worker_task_id: None,
            }),
        }
    }

    pub fn for_goal(goal: &desk_diagnose_core::goal::GoalRun, now_unix_ms: u64) -> Vec<Self> {
        use desk_diagnose_core::goal::{GoalPauseReason, GoalState, GoalWaitReason};
        let reason = match goal.state {
            GoalState::Waiting(GoalWaitReason::User)
            | GoalState::Paused(GoalPauseReason::NeedsNextStep) => {
                Some(AiAssistantAttentionReason::GoalNeedsInput)
            }
            GoalState::Paused(GoalPauseReason::Budget) => {
                Some(AiAssistantAttentionReason::GoalBudget)
            }
            GoalState::Paused(GoalPauseReason::Stalled) => {
                Some(AiAssistantAttentionReason::GoalStalled)
            }
            GoalState::Paused(
                GoalPauseReason::Recovery
                | GoalPauseReason::ContextTooSmall
                | GoalPauseReason::AttachmentMissing
                | GoalPauseReason::AttachmentCapacity,
            )
            | GoalState::Blocked => Some(AiAssistantAttentionReason::GoalBlocked),
            _ => None,
        };
        let mut items = reason
            .map(|reason| vec![Self::from_goal(goal, reason)])
            .unwrap_or_default();
        if !goal.state.is_terminal()
            && goal.deadline_unix_ms != i64::MAX as u64
            && now_unix_ms < goal.deadline_unix_ms
            && goal.deadline_unix_ms - now_unix_ms <= 24 * 60 * 60 * 1_000
        {
            items.push(Self::from_goal(
                goal,
                AiAssistantAttentionReason::GoalDeadlineSoon,
            ));
        }
        items
    }

    pub fn from_goal(
        goal: &desk_diagnose_core::goal::GoalRun,
        reason: AiAssistantAttentionReason,
    ) -> Self {
        let identity = format!(
            "goal:{}:{}:{}",
            goal.goal_id,
            goal.state_version,
            reason.as_str()
        );
        Self {
            attention_id: format!("attention-{:x}", Sha256::digest(identity.as_bytes())),
            session_id: goal.conversation_id.clone(),
            client_conversation_id: None,
            device_id: goal.device_id.clone(),
            goal_id: Some(goal.goal_id.clone()),
            request_id: None,
            reason,
            state_version: goal.state_version,
            updated_at_unix_ms: goal.updated_at_unix_ms,
            deadline_unix_ms: (goal.deadline_unix_ms != i64::MAX as u64)
                .then_some(goal.deadline_unix_ms),
            workbench: None,
        }
    }

    pub fn from_goal_open(request: &desk_diagnose_core::goal::GoalOpenRequest) -> Self {
        let identity = format!(
            "goal-open:{}:{}",
            request.request_id, request.input_revision
        );
        Self {
            attention_id: format!("attention-{:x}", Sha256::digest(identity.as_bytes())),
            session_id: request.conversation_id.clone(),
            client_conversation_id: None,
            device_id: request.device_id.clone(),
            goal_id: request.target_goal_id.clone(),
            request_id: Some(request.request_id.clone()),
            reason: AiAssistantAttentionReason::GoalOpenApproval,
            state_version: request.input_revision,
            updated_at_unix_ms: request.created_at_unix_ms,
            deadline_unix_ms: Some(request.expires_at_unix_ms),
            workbench: None,
        }
    }

    pub fn from_permission(
        session_id: &str,
        device_id: &str,
        request: &desk_diagnose_core::dynamic_run::PermissionRequest,
        updated_at_unix_ms: u64,
    ) -> Self {
        let identity = format!(
            "permission:{}:{}",
            request.request_id, request.input_revision
        );
        Self {
            attention_id: format!("attention-{:x}", Sha256::digest(identity.as_bytes())),
            session_id: session_id.to_owned(),
            client_conversation_id: None,
            device_id: device_id.to_owned(),
            goal_id: None,
            request_id: Some(request.request_id.clone()),
            reason: AiAssistantAttentionReason::PermissionApproval,
            state_version: request.input_revision,
            updated_at_unix_ms,
            deadline_unix_ms: None,
            workbench: None,
        }
    }

    pub fn from_command_approval(
        session_id: &str,
        device_id: &str,
        request_id: &str,
        updated_at_unix_ms: u64,
        deadline_unix_ms: Option<u64>,
    ) -> Self {
        let identity = format!("command-approval:{request_id}");
        Self {
            attention_id: format!("attention-{:x}", Sha256::digest(identity.as_bytes())),
            session_id: session_id.to_owned(),
            client_conversation_id: None,
            device_id: device_id.to_owned(),
            goal_id: None,
            request_id: Some(request_id.to_owned()),
            reason: AiAssistantAttentionReason::PermissionApproval,
            state_version: 1,
            updated_at_unix_ms,
            deadline_unix_ms,
            workbench: None,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AiAssistantAttentionListDto {
    pub items: Vec<AiAssistantAttentionItemDto>,
    pub has_more: bool,
    pub off_page_reminder_available: bool,
}

#[cfg(test)]
mod tests {
    #[test]
    fn approval_projection_omits_oss_money_and_preserves_manager_costs() {
        use desk_diagnose_core::approval_delegation::ApprovalDelegation;
        let oss = ApprovalDelegation::new_usage_only(
            "delegation".into(),
            "conversation".into(),
            "owner".into(),
            "device".into(),
            "owner-enabled".into(),
            1000,
        )
        .unwrap();
        let public = serde_json::to_value(super::ApprovalDelegationDto::from(&oss)).unwrap();
        assert!(public.get("costUsedMicros").is_none());
        assert_eq!(public["tokensUsed"], 0);
        let mut manager = ApprovalDelegation::new(
            "delegation".into(),
            "conversation".into(),
            "owner".into(),
            "device".into(),
            "owner-enabled".into(),
            1000,
        )
        .unwrap();
        manager.reserve(100, 200).unwrap();
        manager.settle(100, 200, Some(7), Some(9)).unwrap();
        let public = serde_json::to_value(super::ApprovalDelegationDto::from(&manager)).unwrap();
        assert_eq!(public["costUsedMicros"], 9);
        assert_eq!(public["tokensUsed"], 7);
    }

    use super::*;

    #[test]
    fn attention_identity_tracks_goal_state_version_and_reason() {
        use desk_diagnose_core::goal::{
            GoalLimits, GoalModelBinding, GoalOpening, GoalPauseReason, GoalRun, GoalState,
        };
        let mut goal = GoalRun::new(
            "goal".into(),
            "run".into(),
            "owner".into(),
            desk_diagnose_core::dynamic_run::AssistantInputTarget::fixed("device".into()),
            "device".into(),
            "Finish the report".into(),
            "message".into(),
            GoalOpening::OwnerRequest,
            GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
            1,
            1_000,
            GoalLimits::default(),
        )
        .unwrap();
        goal.state = GoalState::Paused(GoalPauseReason::Budget);
        let first = AiAssistantAttentionItemDto::for_goal(&goal, 1_000);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].reason, AiAssistantAttentionReason::GoalBudget);
        let same = AiAssistantAttentionItemDto::for_goal(&goal, 1_000);
        assert_eq!(first[0].attention_id, same[0].attention_id);
        goal.state_version += 1;
        let revised = AiAssistantAttentionItemDto::for_goal(&goal, 1_000);
        assert_ne!(first[0].attention_id, revised[0].attention_id);
        assert_eq!(first[0].session_id, "run");
    }

    #[test]
    fn approaching_deadline_is_a_separate_action_without_goal_text() {
        use desk_diagnose_core::goal::{GoalLimits, GoalModelBinding, GoalOpening, GoalRun};
        let mut goal = GoalRun::new(
            "goal".into(),
            "run".into(),
            "owner".into(),
            desk_diagnose_core::dynamic_run::AssistantInputTarget::fixed("device".into()),
            "device".into(),
            "Private target details".into(),
            "message".into(),
            GoalOpening::OwnerRequest,
            GoalModelBinding {
                connection_id: "gateway".into(),
                connection_revision: 1,
                profile_revision: 1,
                model_id: "model".into(),
            },
            1,
            1_000,
            GoalLimits::default(),
        )
        .unwrap();
        goal.deadline_unix_ms = 1_000 + 86_400_000;
        let soon = AiAssistantAttentionItemDto::for_goal(&goal, goal.deadline_unix_ms - 60_000);
        assert_eq!(soon.len(), 1);
        assert_eq!(soon[0].reason, AiAssistantAttentionReason::GoalDeadlineSoon);
        let serialized = serde_json::to_string(&soon).unwrap();
        assert!(!serialized.contains("Private target details"));
    }

    #[test]
    fn owner_context_time_is_model_metadata_and_does_not_hide_owner_text_or_other_events() {
        use desk_diagnose_core::chat::ChatMessage;
        let destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "owner-context".into(),
            "The August report".into(),
            destination.clone(),
        )
        .unwrap();
        let mut native = ChatMessage::system_event(
            "owner-time",
            r#"{"native_control":"owner_context_supplied","recorded_at_unix_ms":1792713602000}"#,
        );
        native.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                owner.data_envelope.as_ref(),
                "owner-time",
                &native.text,
                "owner_context_time",
            )
            .unwrap();
        let literal = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "literal-json".into(),
            native.text.clone(),
            destination,
        )
        .unwrap();
        let page = project_snapshot_message_page(
            vec![owner.clone(), native.clone(), literal.clone()],
            None,
            Some(100),
            PAGE_BYTES,
        )
        .unwrap();
        assert_eq!(
            page.messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["owner-context", "literal-json"]
        );
        assert!(!page.page.has_more);
        assert!(
            project_snapshot_message_page(
                vec![owner, native, literal],
                Some("owner-time"),
                Some(100),
                PAGE_BYTES
            )
            .is_err()
        );
    }

    #[test]
    fn context_usage_exposes_only_budget_metadata() {
        let dto = ContextUsageDto::from(desk_diagnose_core::context_usage::ContextUsage {
            prepared_request_bytes: None,
            prepared_context_bytes: None,
            history_is_estimated: true,
            latest_model_call: None,
            used_bytes: 42,
            limit_bytes: 128,
            strategy: "checkpoint_summary".into(),
            breakdown: desk_diagnose_core::context_usage::ContextUsageBreakdown {
                protocol_bytes: 0,
                messages_bytes: 12,
                tools_bytes: 10,
                replay_bytes: 20,
                projected_bytes: 0,
            },
            request_budget: Some(desk_diagnose_core::context_usage::ContextRequestBudget {
                total_bytes: 256,
                system_prompt_bytes: 64,
                tool_definitions_bytes: 32,
                other_overhead_bytes: 32,
            }),
        });
        assert_eq!(
            serde_json::to_value(dto).unwrap(),
            serde_json::json!({
                "usedBytes": 42, "limitBytes": 128, "strategy": "checkpoint_summary",
                "preparedRequestBytes": null, "preparedContextBytes": null,
                "historyIsEstimated": true, "latestModelCall": null,
                "breakdown": {"messagesBytes": 12, "toolsBytes": 10, "replayBytes": 20, "projectedBytes": 0, "protocolBytes": 0},
                "requestBudget": {"totalBytes": 256, "systemPromptBytes": 64, "toolDefinitionsBytes": 32, "otherOverheadBytes": 32}
            })
        );
    }

    #[test]
    fn unmeasured_request_budget_is_null_instead_of_zero() {
        let dto = ContextUsageDto::from(desk_diagnose_core::context_usage::ContextUsage {
            prepared_request_bytes: None,
            prepared_context_bytes: None,
            history_is_estimated: true,
            latest_model_call: None,
            used_bytes: 0,
            limit_bytes: 128,
            strategy: "window".into(),
            breakdown: Default::default(),
            request_budget: None,
        });
        let value = serde_json::to_value(dto).unwrap();
        assert!(value["requestBudget"].is_null());
        assert_eq!(value["limitBytes"], 128);
        assert_eq!(value["usedBytes"], 0);
    }

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
            account_id: Some("slack-web:T123:U456".into()),
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
            account_id: desk_diagnose_core::communication::slack_web_account_id(&page).unwrap(),
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
        let input: SlackWebExactSendInput = serde_json::from_str(&canonical).unwrap();
        desk_diagnose_core::communication::verify_slack_web_exact_send_input(&input).unwrap();
        let confirmation =
            external_send_confirmation("send_slack_message", Some(&canonical)).unwrap();
        assert_eq!(confirmation.channel, CommunicationChannel::Chat);
        let input: SlackWebExactSendInput = serde_json::from_str(&canonical).unwrap();
        assert_eq!(
            confirmation.account_id,
            desk_diagnose_core::communication::slack_web_account_id(&input.page).unwrap()
        );
        assert_eq!(confirmation.destination, "Message #review");
        assert_eq!(confirmation.subject, None);
        assert_eq!(confirmation.body_plain_text, "Reviewed Slack body");
        assert_eq!(confirmation.body_size_bytes, 19);
        assert!(confirmation.attachments.is_empty());
        assert!(confirmation.one_shot);

        let mut changed = serde_json::from_str::<serde_json::Value>(&canonical).unwrap();
        changed["body_plain_text"] = serde_json::json!("Changed after review");
        assert!(
            external_send_confirmation("send_slack_message", Some(&changed.to_string())).is_none()
        );
        assert!(external_send_confirmation("send_slack_message", None).is_none());
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
            let projection = project_snapshot_message_page(
                messages.clone(),
                before.as_deref(),
                None,
                PAGE_BYTES,
            )
            .unwrap();
            assert!(projection.messages.len() <= MAX_SNAPSHOT_MESSAGE_PAGE_SIZE);
            assert!(serde_json::to_vec(&projection.messages).unwrap().len() <= PAGE_BYTES);
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
        assert!(project_snapshot_message_page(Vec::new(), None, Some(101), PAGE_BYTES).is_err());
    }
}

#[cfg(test)]
mod scheduled_query_tests {
    use super::*;
    #[test]
    fn personal_snapshot_accepts_a_valid_intent_without_a_connection() {
        let query: AiAssistantSessionQuery =
            serde_json::from_value(serde_json::json!({"conversation":" personal-thread "}))
                .unwrap();
        assert_eq!(
            query.snapshot_selection().unwrap(),
            AiAssistantSnapshotSelection::PersonalIntent {
                conversation: "personal-thread"
            }
        );
        let invalid: AiAssistantSessionQuery =
            serde_json::from_value(serde_json::json!({"conversation":"invalid intent"})).unwrap();
        assert!(invalid.snapshot_selection().is_err());
    }
    #[test]
    fn scheduled_selector_is_complete_and_excludes_other_selectors() {
        let valid = serde_json::json!({"scheduled_task":"task", "scheduled_run":"run"});
        let query: AiAssistantSessionQuery = serde_json::from_value(valid.clone()).unwrap();
        assert_eq!(
            query.snapshot_selection().unwrap(),
            AiAssistantSnapshotSelection::Scheduled {
                task: "task",
                run: "run"
            }
        );
        for key in ["connection", "conversation", "session"] {
            let mut mixed = valid.clone();
            mixed[key] = serde_json::json!("");
            let query: AiAssistantSessionQuery = serde_json::from_value(mixed).unwrap();
            assert!(query.snapshot_selection().is_err());
        }
        for key in ["scheduled_task", "scheduled_run"] {
            let mut partial = valid.clone();
            partial.as_object_mut().unwrap().remove(key);
            let query: AiAssistantSessionQuery = serde_json::from_value(partial).unwrap();
            assert!(query.snapshot_selection().is_err());
        }
        let query: AiAssistantSessionQuery =
            serde_json::from_value(serde_json::json!({"connection":"host", "conversation":"chat"}))
                .unwrap();
        assert_eq!(
            query.snapshot_selection().unwrap(),
            AiAssistantSnapshotSelection::DeviceIntent {
                connection: "host",
                conversation: "chat"
            }
        );
    }

    #[test]
    fn saved_snapshot_selection_uses_the_session_even_with_a_device_selected() {
        let query: AiAssistantSessionQuery = serde_json::from_value(serde_json::json!({
            "session": "saved-session", "connection": "another-device", "conversation": "chat"
        }))
        .unwrap();
        assert_eq!(
            query.snapshot_selection().unwrap(),
            AiAssistantSnapshotSelection::Saved {
                session: "saved-session"
            }
        );
    }
}

#[cfg(test)]
mod command_task_projection_tests {
    use super::*;
    use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome};

    #[test]
    fn command_projection_preserves_terminal_failure_and_bounds_unicode_output() {
        let task = CommandTaskDto::project(
            "device".into(),
            "task".into(),
            "call".into(),
            "generation".into(),
            "done",
            "2026-09-08T00:00:00Z".into(),
            Some(AgentOutcome::Err(AgentError {
                kind: AgentErrorKind::Cancelled,
                message: "取消".repeat(20_000),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            })),
        );
        assert!(matches!(task.state, BackgroundTaskStateDto::Cancelled));
        assert!(task.result_truncated);
        assert!(task.result.as_ref().unwrap().len() <= 16 * 1024);
        assert_eq!(task.execution_generation, "generation");
    }

    #[test]
    fn execution_receipt_is_not_automatically_a_success() {
        for (started, exit_code, expected) in [
            (true, None, BackgroundTaskStateDto::OutcomeUnknown),
            (false, None, BackgroundTaskStateDto::Failed),
            (true, Some(3), BackgroundTaskStateDto::Failed),
        ] {
            let receipt = desk_agent_protocol::ExecOutput {
                started,
                exit_code,
                termination_signal: None,
                failure: Some(AgentError {
                    kind: AgentErrorKind::Internal,
                    message: "native failure".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }),
                diagnostics: vec![],
                streams: desk_agent_protocol::ExecOutputStreams::Split {
                    stdout: "retained".into(),
                    stderr: "".into(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
                duration_ms: 1,
                redactions: vec![],
            };
            let task = CommandTaskDto::project(
                "device".into(),
                "task".into(),
                "call".into(),
                "generation".into(),
                "done",
                "now".into(),
                Some(AgentOutcome::Ok(
                    desk_agent_protocol::OperationOutput::Exec(receipt),
                )),
            );
            assert_eq!(
                std::mem::discriminant(&task.state),
                std::mem::discriminant(&expected)
            );
            assert!(task.result.unwrap().contains("retained"));
        }
    }
    #[test]
    fn missing_or_invalid_result_is_unknown_not_success() {
        let task = CommandTaskDto::project(
            "device".into(),
            "task".into(),
            "call".into(),
            "generation".into(),
            "done",
            "now".into(),
            None,
        );
        assert!(matches!(task.state, BackgroundTaskStateDto::OutcomeUnknown));
        assert!(task.result.is_none());
    }
}

#[cfg(test)]
mod reasoning_display_tests {
    use super::*;
    #[test]
    fn reasoning_display_never_makes_an_existing_answer_unreadable() {
        let original = "x".repeat(PAGE_BYTES - 1024);
        let mut message = ChatMessage::text(
            "answer",
            desk_diagnose_core::chat::ChatRole::Assistant,
            original.clone(),
        );
        message.reasoning = Some("思考".repeat(10_000));
        let page = project_snapshot_message_page(vec![message], None, None, PAGE_BYTES).unwrap();
        assert_eq!(page.messages[0].text, original);
        assert_eq!(
            page.messages[0].reasoning.as_deref(),
            Some("思考".repeat(10_000).as_str())
        );
        assert!(serde_json::to_vec(&page.messages).unwrap().len() > PAGE_BYTES);
    }

    #[test]
    fn reasoning_display_projects_reviewed_text_and_never_opaque_replay() {
        use desk_diagnose_core::chat::ChatRole;
        use desk_diagnose_core::replay::{
            ProviderReplayEnvelope, ReplayCodec, ReplayDisposition, SourceContextKey,
        };
        let mut message = ChatMessage::text("answer", ChatRole::Assistant, "answer");
        message.reasoning = Some("reviewed text".into());
        message.replay_disposition = Some(ReplayDisposition::Present {
            envelope: ProviderReplayEnvelope::new(
                ReplayCodec::AnthropicContentBlocks,
                SourceContextKey::derive(
                    desk_diagnose_core::model_profile::WireProtocol::AnthropicMessages,
                    "connection",
                    "model",
                    "model",
                ),
                serde_json::json!([{"type":"redacted_thinking", "data":"opaque-secret"}]),
            ),
        });
        let value = serde_json::to_value(SnapshotMessageDto::from(message)).unwrap();
        assert_eq!(value["reasoning"], "reviewed text");
        assert!(!value.to_string().contains("opaque-secret"));
        let mut user = ChatMessage::text("user", ChatRole::User, "question");
        user.reasoning = Some("not assistant content".into());
        assert!(SnapshotMessageDto::from(user).reasoning.is_none());
    }

    #[test]
    fn tool_completion_outcome_is_projected_without_inferencing_from_output() {
        let mut completed = ChatMessage::tool_result("result", "call", "opaque result");
        completed.tool_ok = Some(true);
        let projected = SnapshotMessageDto::from(completed);
        assert_eq!(projected.tool_ok, Some(true));
        assert_eq!(serde_json::to_value(projected).unwrap()["toolOk"], true);

        let old = SnapshotMessageDto::from(ChatMessage::tool_result(
            "old-result",
            "old-call",
            "opaque result",
        ));
        assert!(old.tool_ok.is_none());
        assert!(serde_json::to_value(old).unwrap().get("toolOk").is_none());
    }
}

/// Opaque conversation id is a selector, never access authority.
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct AssistantImageQuery {
    pub session: String,
    pub attachment: Option<String>,
    pub before: Option<String>,
}

/// Raw image response; the HTTP body contains bytes rather than a JSON array.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
pub struct AssistantImageBytes(pub Vec<u8>);

#[cfg(test)]
mod goal_time_contract_tests {
    use super::*;
    use desk_diagnose_core::goal::{GoalLimits, GoalModelBinding, GoalOpening, GoalRun};

    #[test]
    fn finite_and_unlimited_goal_snapshots_match_the_mobile_contract() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/goal_time_contract.json"))
                .unwrap();
        for unlimited in [false, true] {
            let limits = if unlimited {
                GoalLimits {
                    active_time_ms: u64::MAX,
                    model_tokens: u64::MAX,
                    model_calls: u32::MAX,
                    tool_calls: u32::MAX,
                    slices: u32::MAX,
                    stalled_slices: u32::MAX,
                }
            } else {
                GoalLimits {
                    active_time_ms: 60000,
                    model_tokens: 1000,
                    model_calls: 10,
                    tool_calls: 20,
                    slices: 3,
                    stalled_slices: 2,
                }
            };
            let mut goal = GoalRun::new(
                "goal".into(),
                "run".into(),
                "owner".into(),
                desk_diagnose_core::dynamic_run::AssistantInputTarget::fixed("device".into()),
                "device".into(),
                "Verify recovery".into(),
                "message".into(),
                GoalOpening::OwnerRequest,
                GoalModelBinding {
                    connection_id: "gateway".into(),
                    connection_revision: 1,
                    profile_revision: 1,
                    model_id: "model".into(),
                },
                1,
                1000,
                limits,
            )
            .unwrap();
            if !unlimited {
                goal.deadline_unix_ms = 86401000;
            }
            let value = serde_json::to_value(AiAssistantGoalDto::from(&goal)).unwrap();
            assert_eq!(
                value,
                fixture[if unlimited { "unlimited" } else { "finite" }]
            );
            let attention = AiAssistantAttentionItemDto::from_goal(
                &goal,
                AiAssistantAttentionReason::GoalBudget,
            );
            assert_eq!(attention.deadline_unix_ms, (!unlimited).then_some(86401000));
            if unlimited {
                assert!(AiAssistantAttentionItemDto::for_goal(&goal, 1000).is_empty());
            }
        }
    }
}

#[cfg(test)]
mod source_window_tests {
    use super::*;
    use desk_diagnose_core::chat::{ChatRole, ToolCall};

    #[test]
    fn one_original_answer_keeps_every_citation_when_it_exceeds_the_page_target() {
        use desk_diagnose_core::{
            ai_assistant::{citation::AnswerCitations, seam::AssistantReadRoot},
            native_ids,
        };
        let text = "x".repeat(PAGE_BYTES - 300);
        let page = project_snapshot_message_page(
            vec![ChatMessage::text("answer", ChatRole::Assistant, &text)],
            None,
            None,
            PAGE_BYTES,
        )
        .unwrap();
        let roots = (0..20)
            .map(|value| AssistantReadRoot {
                source_id: native_ids::fixture_source_id(&value),
                source_revision: 1,
            })
            .collect::<Vec<_>>();
        let citations = roots
            .iter()
            .map(
                |root| desk_agent_protocol::ai_assistant::source::AssistantCitation {
                    answer_message_id: "answer".into(),
                    answer_revision: 1,
                    source_id: root.source_id.clone(),
                    source_revision: 1,
                    source_device: None,
                },
            )
            .collect::<Vec<_>>();
        let expected = citations.len();
        let page = page
            .with_citations(
                std::collections::BTreeMap::from([(
                    "answer".into(),
                    AnswerCitations {
                        citations,
                        incomplete: false,
                    },
                )]),
                PAGE_BYTES,
                None,
            )
            .unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].text, text);
        assert_eq!(page.messages[0].citations.len(), expected);
        assert!(!page.messages[0].citations_incomplete);
        assert!(serde_json::to_vec(&page.messages).unwrap().len() > PAGE_BYTES);
    }

    #[test]
    fn source_window_locates_old_messages_and_keeps_the_original_older_cursor() {
        let messages = (0..1000)
            .map(|index| {
                ChatMessage::text(
                    format!("message-{index:04}"),
                    ChatRole::User,
                    format!("原文 {index}"),
                )
            })
            .collect();
        let page = project_snapshot_message_window(
            messages,
            None,
            Some("message-0500"),
            Some(10),
            PAGE_BYTES,
        )
        .unwrap();
        assert_eq!(page.messages.len(), 10);
        assert_eq!(page.messages.first().unwrap().id, "message-0496");
        assert_eq!(page.messages.last().unwrap().id, "message-0505");
        assert_eq!(page.messages[4].text, "原文 500");
        assert!(page.page.has_more);
        assert_eq!(
            page.page.next_before_message_id.as_deref(),
            Some("message-0496")
        );
    }

    #[test]
    fn source_window_keeps_the_complete_native_group_and_rejects_bad_selectors() {
        let messages = vec![
            ChatMessage::text("input", ChatRole::User, "original requirement"),
            ChatMessage::assistant_tool_calls(
                "producer",
                "",
                vec![
                    ToolCall {
                        id: "native-call".into(),
                        name: "read_system_info".into(),
                        arguments_json: "{}".into(),
                    }
                    .to_ref(),
                ],
            ),
            ChatMessage::tool_result("native-result", "native-call", "original native result"),
            ChatMessage::text("answer", ChatRole::Assistant, "later answer"),
        ];
        let page = project_snapshot_message_window(
            messages.clone(),
            None,
            Some("native-result"),
            Some(3),
            PAGE_BYTES,
        )
        .unwrap();
        assert_eq!(
            page.messages
                .iter()
                .map(|message| message.id.as_str())
                .collect::<Vec<_>>(),
            ["producer", "native-result", "answer"]
        );
        assert_eq!(
            page.page.next_before_message_id.as_deref(),
            Some("producer")
        );
        assert!(
            project_snapshot_message_window(
                messages.clone(),
                Some("input"),
                Some("native-result"),
                None,
                PAGE_BYTES
            )
            .is_err()
        );
        assert!(
            project_snapshot_message_window(
                messages.clone(),
                None,
                Some("native-result"),
                Some(1),
                PAGE_BYTES
            )
            .is_err()
        );
        assert!(
            project_snapshot_message_window(
                messages.clone(),
                None,
                Some("unknown"),
                None,
                PAGE_BYTES
            )
            .is_err()
        );
        assert!(
            project_snapshot_message_window(messages, None, Some(""), None, PAGE_BYTES).is_err()
        );
    }

    #[test]
    fn oversized_following_content_cannot_displace_the_requested_source() {
        let messages = vec![
            ChatMessage::text("source", ChatRole::Assistant, "requested original"),
            ChatMessage::text("large", ChatRole::User, "x".repeat(PAGE_BYTES + 1)),
        ];
        let page =
            project_snapshot_message_window(messages, None, Some("source"), Some(10), PAGE_BYTES)
                .unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, "source");
        assert!(!page.page.has_more);
    }
}

#[cfg(test)]
mod configured_page_budget_tests {
    use super::*;
    use desk_diagnose_core::ai_assistant::citation::AnswerCitations;
    use desk_diagnose_core::chat::{ChatRole, ToolCall};

    fn messages() -> Vec<ChatMessage> {
        let mut producer = ChatMessage::text("producer", ChatRole::Assistant, "working");
        producer.tool_calls = vec![
            ToolCall {
                id: "tool-call".into(),
                name: "read".into(),
                arguments_json: "{}".into(),
            }
            .to_ref(),
        ];
        let mut output = ChatMessage::text("output", ChatRole::Tool, "原文".repeat(60000));
        output.tool_call_id = Some("tool-call".into());
        vec![
            ChatMessage::text("earlier", ChatRole::User, "earlier input"),
            producer,
            output,
        ]
    }

    #[test]
    fn one_large_native_group_is_whole_and_previous_pages_make_progress() {
        let original = messages();
        let page = project_snapshot_message_page(original.clone(), None, None, 1024).unwrap();
        assert_eq!(
            page.messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["producer", "output"]
        );
        assert_eq!(page.messages[1].text, original[2].text);
        assert_eq!(
            page.page.next_before_message_id.as_deref(),
            Some("producer")
        );
        let previous =
            project_snapshot_message_page(original, Some("producer"), None, 1024).unwrap();
        assert_eq!(previous.messages[0].id, "earlier");
        assert!(!previous.page.has_more);
    }

    #[test]
    fn citations_page_whole_neighbors_without_removing_the_requested_source_group() {
        let original = vec![
            ChatMessage::text("older", ChatRole::User, "older"),
            ChatMessage::text("anchor", ChatRole::Assistant, "source"),
            ChatMessage::text("following", ChatRole::User, "later"),
        ];
        let page =
            project_snapshot_message_window(original, None, Some("anchor"), None, 4096).unwrap();
        let citation = desk_agent_protocol::ai_assistant::source::AssistantCitation {
            answer_message_id: "anchor".into(),
            answer_revision: 1,
            source_id: "source".repeat(500),
            source_revision: 1,
            source_device: None,
        };
        let page = page
            .with_citations(
                std::collections::BTreeMap::from([(
                    "anchor".into(),
                    AnswerCitations {
                        citations: vec![citation],
                        incomplete: false,
                    },
                )]),
                512,
                Some("anchor"),
            )
            .unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, "anchor");
        assert_eq!(page.messages[0].citations.len(), 1);
        assert!(!page.messages[0].citations_incomplete);
        assert_eq!(page.page.next_before_message_id.as_deref(), Some("anchor"));
    }

    #[test]
    fn a_nondefault_page_target_changes_paging_without_truncating_answers() {
        let original = (0..6)
            .map(|i| {
                ChatMessage::text(
                    format!("message-{i}"),
                    ChatRole::Assistant,
                    "x".repeat(2000),
                )
            })
            .collect::<Vec<_>>();
        let small = project_snapshot_message_page(original.clone(), None, None, 5000).unwrap();
        let large = project_snapshot_message_page(original, None, None, 20000).unwrap();
        assert_eq!(small.messages.len(), 2);
        assert_eq!(large.messages.len(), 6);
        assert!(small.messages.iter().all(|m| m.text.len() == 2000));
        assert!(small.page.has_more);
    }
}
