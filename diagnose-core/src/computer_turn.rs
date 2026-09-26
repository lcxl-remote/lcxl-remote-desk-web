//! Classify a device's frozen turn against authoritative persisted state.
use crate::session::{AgentSessionSurface, PersistedAgentSession};
use desk_agent_protocol::computer_turn::{ComputerActionTurnQuery, ComputerActionTurnState};

/// `device_id` is resolved by the central server from the authenticated host.
/// A terminal or superseded turn is revoked even if a native effect is unknown.
/// Revocation releases input control; it does not report an action as successful.
pub fn classify(
    query: &ComputerActionTurnQuery,
    device_id: &str,
    session: Option<&PersistedAgentSession>,
) -> ComputerActionTurnState {
    use ComputerActionTurnState::*;
    if query.validate().is_err() || device_id.is_empty() {
        return Unavailable;
    }
    let Some(session) = session else {
        return Revoked;
    };
    if session.surface != AgentSessionSurface::AiAssistant
        || session.actor_id != query.actor_id
        || session.device_id != device_id
        || session.conversation_id != query.scope.conversation_id
    {
        return Unavailable;
    }
    if session.turn_state.is_settled()
        || session.current_turn_id.as_deref() != Some(query.scope.turn_id.as_str())
        || session.input_revision != query.scope.input_revision
        || session.lease_token != query.scope.lease_token
    {
        return Revoked;
    }
    Current
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{action_turn_fence::AssistantTurnFence, session::TurnState};
    use desk_agent_protocol::{AgentScope, ExecutionMode};

    fn fixture() -> (PersistedAgentSession, ComputerActionTurnQuery) {
        let mut session = PersistedAgentSession::new(
            "conversation",
            "owner",
            "device",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "now",
        );
        session.surface = AgentSessionSurface::AiAssistant;
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session
            .begin_turn("turn", None, None, 1, session.scope_snapshot.clone(), "now")
            .unwrap();
        let scope = AssistantTurnFence::from_session(&session)
            .unwrap()
            .unwrap()
            .computer_action_scope()
            .unwrap();
        (
            session,
            ComputerActionTurnQuery {
                actor_id: "owner".into(),
                scope,
            },
        )
    }

    #[test]
    fn normal_finish_cancel_failure_deletion_and_supersession_revoke() {
        use ComputerActionTurnState::*;
        let (original, query) = fixture();
        assert_eq!(classify(&query, "device", Some(&original)), Current);
        assert_eq!(classify(&query, "device", None), Revoked);
        for state in [TurnState::Idle, TurnState::Cancelled, TurnState::Failed] {
            let mut session = original.clone();
            session.finish_turn(state, "later");
            assert_eq!(classify(&query, "device", Some(&session)), Revoked);
        }
        for field in 0..3 {
            let mut session = original.clone();
            match field {
                0 => session.current_turn_id = Some("next-turn".into()),
                1 => session.input_revision += 1,
                _ => session.lease_token += 1,
            }
            assert_eq!(classify(&query, "device", Some(&session)), Revoked);
        }
        let mut waiting = original;
        waiting.turn_state = TurnState::AwaitingApproval;
        assert_eq!(classify(&query, "device", Some(&waiting)), Current);
    }

    #[test]
    fn wrong_subject_or_invalid_query_is_not_a_revocation_proof() {
        let (original, mut query) = fixture();
        for field in 0..4 {
            let mut session = original.clone();
            match field {
                0 => session.actor_id = "other".into(),
                1 => session.device_id = "other".into(),
                2 => session.conversation_id = "other".into(),
                _ => session.surface = AgentSessionSurface::TerminalAiAssistant,
            }
            assert_eq!(
                classify(&query, "device", Some(&session)),
                ComputerActionTurnState::Unavailable
            );
        }
        query.scope.lease_token = 0;
        assert_eq!(
            classify(&query, "device", None),
            ComputerActionTurnState::Unavailable
        );
    }
}
