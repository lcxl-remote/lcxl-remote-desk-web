//! Conversation-scoped schedule inspection and cancellation, without tool grants.
use super::proposal;
use crate::{
    chat::{ChatRole, ToolCall, ToolSpec},
    session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin},
};
use desk_agent_protocol::schedule::ScheduleDraft;
use serde::Deserialize;
use serde_json::json;

pub const LIST: &str = "list_conversation_scheduled_tasks";
pub const CANCEL: &str = "cancel_conversation_scheduled_task";

pub enum Action {
    Create(Box<ScheduleDraft>),
    List {
        after: i64,
        limit: u64,
    },
    Cancel {
        schedule_id: String,
        expected_revision: i64,
    },
}

pub fn specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: LIST.into(),
        description: "Query current server state of scheduled tasks belonging to this conversation. Includes manual tasks (read-only) and AI proposals. Use this before reporting task status or cancelling; historical receipts are not current status. Returns source, can_cancel, actual time, revision and a pagination cursor. No additional approval is required; the server binds owner, device and conversation.".into(),
        parameters_schema: json!({"type":"object","additionalProperties":false,"properties":{"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":20}}}),
    }, ToolSpec {
        name: CANCEL.into(),
        description: "Cancel an AI-created scheduled task in the current conversation without an extra approval dialog. Use the id and revision returned by list_conversation_scheduled_tasks. The server forbids cancellation of manual tasks and tasks from other conversations, devices or owners. Stops future scheduling and requests cancellation of any active occurrence; does not undo completed external effects. Report the returned state accurately, including cancellation_requested for a running occurrence.".into(),
        parameters_schema: json!({"type":"object","additionalProperties":false,"required":["schedule_id","expected_revision"],"properties":{"schedule_id":{"type":"string","minLength":36,"maxLength":36},"expected_revision":{"type":"integer","minimum":1}}}),
    }]
}

pub fn parse(session: &PersistedAgentSession, call: &ToolCall) -> Result<Action, &'static str> {
    if session.surface != AgentSessionSurface::DeviceAssistant
        || session.trigger_origin != TriggerOrigin::User
        || !session.turn_state.is_active()
        || session.input_revision == 0
        || session.actor_id.is_empty()
        || session.device_id.is_empty()
        || call.id.is_empty()
        || call.id.len() > 256
        || call.arguments_json.len() > 24 * 1024
    {
        return Err("schedule management is unavailable");
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct List {
        #[serde(default)]
        after: i64,
        #[serde(default = "default_limit")]
        limit: u64,
    }
    fn default_limit() -> u64 {
        10
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Cancel {
        schedule_id: String,
        expected_revision: i64,
    }
    match call.name.as_str() {
        proposal::REQUEST_SCHEDULE => {
            proposal::draft(session, call).map(|draft| Action::Create(Box::new(draft)))
        }
        LIST => {
            let input: List =
                serde_json::from_str(&call.arguments_json).map_err(|_| "invalid query")?;
            if input.after < 0 || !(1..=20).contains(&input.limit) {
                return Err("invalid page");
            }
            Ok(Action::List {
                after: input.after,
                limit: input.limit,
            })
        }
        CANCEL => {
            let input: Cancel =
                serde_json::from_str(&call.arguments_json).map_err(|_| "invalid cancellation")?;
            if input.expected_revision < 1
                || input.schedule_id.len() != 36
                || !input
                    .schedule_id
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() || c == b'-')
            {
                return Err("invalid cancellation");
            }
            Ok(Action::Cancel {
                schedule_id: input.schedule_id,
                expected_revision: input.expected_revision,
            })
        }
        _ => Err("unknown schedule operation"),
    }
}

/// Fresh automations have no continuation source. Their immutable, server-labelled
/// creation receipts bind them to their originating chat without linking execution
/// contexts. User text, assistant prose and a caller-supplied task id are not proof.
pub fn proposed_ids(session: &PersistedAgentSession) -> Vec<String> {
    session
        .conversation
        .iter()
        .filter_map(|message| {
            let label = message.data_envelope.as_ref()?;
            if message.role != ChatRole::Tool
                || label.provenance.source_provider_id
                    != crate::dynamic_run::RUN_CONTROL_PROVIDER_ID
                || label.provenance.source_tool_name != "schedule_proposal"
            {
                return None;
            }
            let value: serde_json::Value = serde_json::from_str(&message.text).ok()?;
            if !matches!(value["state"].as_str(), Some("draft" | "pending_review")) {
                return None;
            }
            value["schedule_id"].as_str().map(str::to_owned)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::ChatMessage;
    use desk_agent_protocol::{AgentScope, ExecutionMode};
    fn session() -> PersistedAgentSession {
        let scope = AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut session = PersistedAgentSession::new(
            "chat",
            "1",
            "device",
            1,
            scope.clone(),
            "2026-09-09T00:00:00Z",
        );
        session.surface = AgentSessionSurface::DeviceAssistant;
        session.input_revision = 1;
        session
            .begin_turn(
                "turn",
                Some("input".into()),
                Some("browser".into()),
                1,
                scope,
                "2026-09-09T00:00:00Z",
            )
            .unwrap();
        session
    }
    #[test]
    fn management_is_user_scoped_and_rejects_model_supplied_authority() {
        let mut session = session();
        let mut call = ToolCall {
            id: "call".into(),
            name: LIST.into(),
            arguments_json: "{}".into(),
        };
        assert!(matches!(
            parse(&session, &call),
            Ok(Action::List {
                after: 0,
                limit: 10
            })
        ));
        call.arguments_json = r#"{"owner":2}"#.into();
        assert!(parse(&session, &call).is_err());
        call.arguments_json = r#"{"limit":21}"#.into();
        assert!(parse(&session, &call).is_err());
        call.arguments_json = "{}".into();
        session.trigger_origin = TriggerOrigin::ScheduledContinuation;
        assert!(parse(&session, &call).is_err());
        assert_eq!(proposal::registry().len(), 3);
    }
    #[test]
    fn user_text_and_unlabelled_tool_output_cannot_claim_creation_origin() {
        let mut session = session();
        let text = r#"{"state":"draft","schedule_id":"12345678-1234-1234-1234-123456789abc"}"#;
        session
            .conversation
            .push(ChatMessage::text("spoof", ChatRole::User, text));
        session
            .conversation
            .push(ChatMessage::tool_result("tool", "call", text));
        assert!(proposed_ids(&session).is_empty());
    }
}
