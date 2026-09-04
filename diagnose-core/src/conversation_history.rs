//! Bounded, explicit retrieval of older user-visible turns from the same session.
//!
//! The complete transcript remains durable for the owner UI, but it is not sent
//! wholesale to the model. This BaseTool pages backward through user/assistant
//! text only, omits tool inputs/results and internal control messages, and keeps
//! the source envelopes needed for the ordinary model-egress authorizer.

use std::collections::BTreeSet;

use desk_agent_protocol::{AgentError, AgentErrorKind, Capability};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    chat::{ChatMessage, ChatRole, ToolCall, ToolSpec},
    registry::{RegisteredTool, ToolEffect},
};

pub const LOAD_CONVERSATION_HISTORY_TOOL_NAME: &str = "load_conversation_history";
pub const MAX_HISTORY_MESSAGES_PER_CALL: usize = 20;
pub const MAX_HISTORY_RESULT_BYTES: usize = 32 * 1024;
pub const MAX_HISTORY_LOOKUPS_PER_FOCUS: u16 = 8;
pub const MAX_HISTORY_BYTES_PER_FOCUS: u64 = 128 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryInput {
    #[serde(default)]
    before_message_id: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    10
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessageProjection {
    pub message_id: String,
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub messages: Vec<HistoryMessageProjection>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_before_message_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HistoryLoadResult {
    pub content: String,
    pub source_messages: Vec<ChatMessage>,
}

pub fn conversation_history_tool_registry() -> Vec<RegisteredTool> {
    vec![RegisteredTool {
        spec: ToolSpec {
            name: LOAD_CONVERSATION_HISTORY_TOOL_NAME.into(),
            description: "Load one bounded page of older user/assistant text from this same conversation. The first call omits the current user requirement; pass next_before_message_id to page farther backward. Tool calls, permission authority, schemas, attachments, and raw evidence are never returned.".into(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "before_message_id": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 512
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_HISTORY_MESSAGES_PER_CALL,
                        "default": 10
                    }
                },
                "additionalProperties": false
            }),
        },
        required_capability: Capability::SystemInfo,
        effect: ToolEffect::ConversationHistory,
    }]
}

pub fn load_history_page(
    call: &ToolCall,
    conversation: &[ChatMessage],
) -> Result<HistoryLoadResult, AgentError> {
    if call.name != LOAD_CONVERSATION_HISTORY_TOOL_NAME {
        return Err(invalid("invalid history tool"));
    }
    let input: HistoryInput =
        serde_json::from_str(&call.arguments_json).map_err(|_| invalid("invalid history input"))?;
    if input.limit == 0 || input.limit > MAX_HISTORY_MESSAGES_PER_CALL {
        return Err(invalid("history limit is out of range"));
    }

    let before = match input.before_message_id {
        Some(id) => conversation
            .iter()
            .position(|message| message.message_id == id)
            .ok_or_else(|| invalid("unknown history cursor"))?,
        None => conversation
            .iter()
            .rposition(is_visible_user_message)
            .unwrap_or(conversation.len()),
    };
    let eligible = conversation[..before]
        .iter()
        .filter(|message| is_visible_history_message(message) && message.data_envelope.is_some())
        .collect::<Vec<_>>();
    let start = eligible.len().saturating_sub(input.limit);
    let selected = &eligible[start..];
    let page = HistoryPage {
        messages: selected
            .iter()
            .map(|message| HistoryMessageProjection {
                message_id: message.message_id.clone(),
                role: message.role.as_str().into(),
                text: message.text.clone(),
            })
            .collect(),
        has_more: start > 0,
        next_before_message_id: selected.first().map(|message| message.message_id.clone()),
    };
    let content = serde_json::to_string(&page).map_err(|_| invalid("history projection failed"))?;
    if content.len() > MAX_HISTORY_RESULT_BYTES {
        return Err(invalid("history page exceeds the byte limit"));
    }
    let mut envelope_ids = BTreeSet::new();
    let source_messages = selected
        .iter()
        .filter(|message| {
            message
                .data_envelope
                .as_ref()
                .is_some_and(|envelope| envelope_ids.insert(envelope.envelope_id.clone()))
        })
        .map(|message| (*message).clone())
        .collect();
    Ok(HistoryLoadResult {
        content,
        source_messages,
    })
}

fn is_visible_user_message(message: &ChatMessage) -> bool {
    message.role == ChatRole::User
        && !crate::permission_resume::is_permission_resume_message(message)
}

fn is_visible_history_message(message: &ChatMessage) -> bool {
    matches!(message.role, ChatRole::User | ChatRole::Assistant)
        && !crate::permission_resume::is_permission_resume_message(message)
}

fn invalid(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::DestinationIdentity;

    fn labeled(id: &str, role: ChatRole, text: &str) -> ChatMessage {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let mut message = crate::model_message_labels::model_bound_user_message(
            id.into(),
            text.into(),
            destination,
        )
        .unwrap();
        message.role = role;
        message
    }

    #[test]
    fn pages_backward_before_current_user_and_omits_tool_content() {
        let conversation = vec![
            labeled("u1", ChatRole::User, "first"),
            labeled("a1", ChatRole::Assistant, "answer one"),
            ChatMessage::tool_result("secret-tool-result", "call", "must not appear"),
            labeled("u2", ChatRole::User, "current"),
        ];
        let result = load_history_page(
            &ToolCall {
                id: "history".into(),
                name: LOAD_CONVERSATION_HISTORY_TOOL_NAME.into(),
                arguments_json: r#"{"limit":2}"#.into(),
            },
            &conversation,
        )
        .unwrap();
        let page: HistoryPage = serde_json::from_str(&result.content).unwrap();
        assert_eq!(
            page.messages
                .iter()
                .map(|message| message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["u1", "a1"]
        );
        assert!(!result.content.contains("current"));
        assert!(!result.content.contains("must not appear"));
        assert_eq!(result.source_messages.len(), 2);
    }

    #[test]
    fn rejects_unknown_cursor_and_oversized_single_page() {
        let conversation = vec![labeled("u1", ChatRole::User, &"x".repeat(40_000))];
        let unknown = ToolCall {
            id: "history".into(),
            name: LOAD_CONVERSATION_HISTORY_TOOL_NAME.into(),
            arguments_json: r#"{"before_message_id":"missing"}"#.into(),
        };
        assert!(load_history_page(&unknown, &conversation).is_err());
        let current = labeled("u2", ChatRole::User, "current");
        let oversized = [conversation[0].clone(), current];
        let first = ToolCall {
            id: "history".into(),
            name: LOAD_CONVERSATION_HISTORY_TOOL_NAME.into(),
            arguments_json: r#"{"limit":1}"#.into(),
        };
        assert!(load_history_page(&first, &oversized).is_err());
    }
}
