//! The held native input owns catalog projection and external read results.

use super::*;
use crate::session::{AgentSessionSurface, PersistedAgentSession};

pub fn validate_held_input(
    held: &PersistedAgentSession,
    current: &PersistedAgentSession,
    destination: &DestinationIdentity,
    call_id: Option<&str>,
    now_ms: i64,
) -> Result<(), AgentError> {
    if current.surface != AgentSessionSurface::AiAssistant
        || held.conversation_id != current.conversation_id
        || held.actor_id != current.actor_id
        || held.device_id != current.device_id
        || held.policy_revision != current.policy_revision
        || held.version != current.version
        || held.lease_token != current.lease_token
        || held.input_revision != current.input_revision
        || held.control_revision != current.control_revision
        || held.current_turn_id != current.current_turn_id
        || current.current_turn_id.is_none()
        || !current.turn_state.is_active()
        || current.input_revision == 0
        || current.main_stopped
        || current
            .agent_role
            .binding()
            .is_some_and(|binding| binding.deadline_ms <= now_ms)
        || current
            .authorization_requirement()
            .and_then(|message| message.data_envelope.as_ref())
            .is_none_or(|envelope| !envelope.allowed_destinations.contains(destination))
        || call_id.is_some_and(|id| {
            !current
                .unclosed_tool_call_ids()
                .iter()
                .any(|open| open == id)
        })
    {
        return Err(invalid(
            "External read no longer belongs to the held native input",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, ToolCallRef};

    #[test]
    fn central_issue_read_keeps_the_original_turn_and_requires_an_open_call() {
        let destination = DestinationIdentity::Model {
            connection_id: "model".into(),
            connection_revision: 1,
            model_id: "analysis".into(),
            profile_revision: 1,
        };
        let scope = desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        };
        let mut held =
            PersistedAgentSession::new("root", "owner", "", 1, scope, "2026-11-04T00:00:00Z");
        held.surface = AgentSessionSurface::AiAssistant;
        held.input_revision = 1;
        held.version = 1;
        held.lease_token = 1;
        held.current_turn_id = Some("turn".into());
        held.turn_state = crate::session::TurnState::Running;
        held.conversation.push(
            crate::model_message_labels::model_bound_user_message(
                "input".into(),
                "Read the selected issue".into(),
                destination.clone(),
            )
            .unwrap(),
        );
        assert!(validate_held_input(&held, &held, &destination, None, 1).is_ok());
        assert!(validate_held_input(&held, &held, &destination, Some("read-1"), 1).is_err());
        held.conversation.push(ChatMessage::assistant_tool_calls(
            "model",
            "",
            vec![ToolCallRef {
                id: "read-1".into(),
                name: TOOL_NAME.into(),
                arguments_json: "{}".into(),
            }],
        ));
        assert!(validate_held_input(&held, &held, &destination, Some("read-1"), 1).is_ok());
        for change in 0..5 {
            let mut current = held.clone();
            match change {
                0 => current.version += 1,
                1 => current.lease_token += 1,
                2 => current.input_revision += 1,
                3 => current.control_revision += 1,
                _ => current.main_stopped = true,
            }
            assert!(validate_held_input(&held, &current, &destination, Some("read-1"), 1).is_err());
        }
        let mut changed = destination;
        if let DestinationIdentity::Model { model_id, .. } = &mut changed {
            *model_id = "another".into();
        }
        assert!(validate_held_input(&held, &held, &changed, None, 1).is_err());
    }
}
