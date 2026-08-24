//! Browser-facing read of an OSS Signal AI-diagnose conversation.
//!
//! The browser supplies the same target connection and conversation intent used
//! by the signaling turn. The server resolves the target's authenticated client
//! id and re-derives the subject-namespaced conversation key, so a caller cannot
//! select an arbitrary SQLite row.

use actix_web::{HttpResponse, get, web};
use desk_diagnose_core::chat::ChatMessage;
use desk_diagnose_core::conversation_key::derive_conversation_key;
use desk_signal_facade::model::{
    auth_context::AuthKind, connection::SharedConnectionMap, signal::RemoteDeskTypeEnum,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::agent_session_store::SignalAgentSessionStore;
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
    pub messages: Vec<SnapshotMessageDto>,
    /// Durable transcript metadata for context-window changes. No omitted text is exposed.
    pub context_notices: Vec<ContextNoticeDto>,
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
    let snapshot = match (
        query.session.as_deref().filter(|value| !value.is_empty()),
        query.conversation.as_deref(),
    ) {
        (Some(session_id), _) => {
            store
                .read_snapshot_for_subject(session_id, &actor_id, &target_audience)
                .await
        }
        (None, Some(conversation)) => {
            let key = derive_conversation_key(&actor_id, &target_audience, Some(conversation), "");
            store
                .read_snapshot_for_subject(&key, &actor_id, &target_audience)
                .await
        }
        (None, None) => return Ok(not_accessible()),
    }
    .map_err(|error| {
        DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
    })?;
    match snapshot {
        Some(snapshot) => Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
            DiagnoseSessionSnapshotDto {
                seq: snapshot.seq,
                active: snapshot.active,
                request_id: snapshot.request_id,
                active_execution_generation: snapshot.active_execution_generation,
                messages: snapshot.messages.into_iter().map(Into::into).collect(),
                context_notices: snapshot
                    .context_notices
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            },
        ))),
        None => Ok(not_accessible()),
    }
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
    use desk_diagnose_core::chat::{ChatMessage, ChatRole, ToolCallRef};

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
}
