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
    pub conversation: String,
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
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<SnapshotToolCallDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl From<ChatMessage> for SnapshotMessageDto {
    fn from(message: ChatMessage) -> Self {
        Self {
            id: message.message_id,
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
        ("conversation" = String, Query, description = "Client conversation intent"),
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

    let key = derive_conversation_key(
        &SINGLE_ACCOUNT_USER_ID.to_string(),
        &target_audience,
        Some(query.conversation.as_str()),
        "",
    );
    let snapshot = SignalAgentSessionStore::new(crate::db::get_db().clone())
        .read_snapshot(&key)
        .await
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
            },
        ))),
        None => Ok(not_accessible()),
    }
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
        let value = serde_json::to_value(SnapshotMessageDto::from(message)).unwrap();
        assert_eq!(value["id"], "m1");
        assert_eq!(value["toolCalls"][0]["argumentsJson"], "{}");
        assert!(value.get("tool_calls").is_none());
    }
}
