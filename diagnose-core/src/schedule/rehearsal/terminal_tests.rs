use super::*;
use desk_agent_protocol::{AgentScope, ExecutionMode};

fn source() -> RehearsalSource<'static> {
    RehearsalSource {
        actor: "1",
        device: "device",
        conversation: "session",
        client_conversation: "rehearsal_reserved",
        input_message_id: "input",
        prompt: "Task",
    }
}

fn failed() -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new(
        "session",
        "1",
        "device",
        1,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::SuggestOnly,
            expires_at: None,
            policy_name: None,
        },
        "now",
    );
    session.surface = AgentSessionSurface::DeviceAssistant;
    session.client_conversation_id = Some("rehearsal_reserved".into());
    session.current_turn_id = Some("turn".into());
    session.turn_state = TurnState::Failed;
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session.handled_input_seq = 0;
    session
        .conversation
        .push(ChatMessage::text("input", ChatRole::User, "Task").with_turn_id("turn"));
    session
}

#[test]
fn distinguishes_failed_cancelled_and_active_or_successful_turns() {
    for state in [
        TurnState::Failed,
        TurnState::Cancelled,
        TurnState::Idle,
        TurnState::Running,
        TurnState::AwaitingApproval,
    ] {
        let mut session = failed();
        session.turn_state = state;
        assert_eq!(
            terminal_without_tools(&session, &source()),
            matches!(state, TurnState::Failed | TurnState::Cancelled).then_some(state)
        );
    }
}

#[test]
fn rejects_changed_identity_input_and_control_continuation() {
    for reason in [
        "actor",
        "device",
        "conversation",
        "client",
        "input",
        "prompt",
        "turn",
        "revision",
        "handled",
        "extra",
        "permission",
        "image",
    ] {
        let mut session = failed();
        match reason {
            "actor" => session.actor_id = "2".into(),
            "device" => session.device_id = "other".into(),
            "conversation" => session.conversation_id = "other".into(),
            "client" => session.client_conversation_id = None,
            "input" => session.conversation[0].message_id = "other".into(),
            "prompt" => session.conversation[0].text = "Other task".into(),
            "turn" => session.current_turn_id = Some("other".into()),
            "revision" => session.input_revision = 2,
            "handled" => session.handled_input_seq = 2,
            "extra" => {
                session
                    .conversation
                    .push(ChatMessage::text("extra", ChatRole::User, "More"))
            }
            "permission" => session.trigger_origin = TriggerOrigin::PermissionDecision,
            "image" => session.conversation[0].image_data_url = Some("image".into()),
            _ => unreachable!(),
        }
        assert_eq!(
            terminal_without_tools(&session, &source()),
            None,
            "{reason}"
        );
    }
}

#[test]
fn rejects_tool_receipts_even_when_all_calls_appear_closed() {
    for role in [ChatRole::Tool, ChatRole::Assistant] {
        let mut session = failed();
        let mut message = ChatMessage::text("result", role, "done");
        message.tool_call_id = Some("call".into());
        session.conversation.push(message);
        assert_eq!(terminal_without_tools(&session, &source()), None);
    }
    let mut session = failed();
    session.terminal_permission_request_id = Some("pending".into());
    assert_eq!(terminal_without_tools(&session, &source()), None);
}

#[test]
fn rejects_unresolved_execution_even_without_visible_tool_history() {
    use crate::session::ActionIdentity;
    let action = ActionIdentity::agent_exec(1, "request", "execution");
    for execution in [
        ExecutionState::Executing {
            action: action.clone(),
        },
        ExecutionState::OutcomeUnknown {
            action,
            placeholder_message_id: "placeholder".into(),
            since: "now".into(),
        },
        ExecutionState::Interrupted {
            since: "now".into(),
        },
    ] {
        let mut session = failed();
        session.execution_state = execution;
        assert_eq!(terminal_without_tools(&session, &source()), None);
    }
}
