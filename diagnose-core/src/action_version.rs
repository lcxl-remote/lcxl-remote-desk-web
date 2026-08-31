//! Version handoff for transactions performed by the tool runtime on behalf of
//! the held loop. This is concurrency evidence, never execution authority.

use desk_agent_protocol::{AgentError, AgentErrorKind};

use crate::{
    action_turn_fence::AssistantTurnFence,
    session::{PersistedAgentSession, TurnState},
};

/// Captured after the loop saves its pending call. An executor must compare
/// this exact version in the transaction, not refresh to the latest row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionVersion {
    pub turn_fence: AssistantTurnFence,
    pub tool_call_id: String,
    pub version: i64,
}

impl ActionVersion {
    pub fn capture(
        session: &PersistedAgentSession,
        tool_call_id: &str,
    ) -> Result<Option<Self>, AgentError> {
        let Some(turn_fence) = AssistantTurnFence::from_session(session)? else {
            return Ok(None);
        };
        if session.turn_state != TurnState::AwaitingApproval
            || session.version < 0
            || tool_call_id.is_empty()
            || tool_call_id.len() > 512
            || tool_call_id.chars().any(char::is_control)
            || !session
                .unclosed_tool_call_ids()
                .iter()
                .any(|id| id == tool_call_id)
        {
            return Err(invalid());
        }
        Ok(Some(Self {
            turn_fence,
            tool_call_id: tool_call_id.into(),
            version: session.version,
        }))
    }

    /// Construct only after one transaction's commit succeeds. Do not adopt
    /// another writer's version to bridge a gap in the compare-and-swap chain.
    pub fn committed(&self, version: i64) -> Result<ActionVersionAdvance, AgentError> {
        self.turn_fence.validate()?;
        if self.version < 0 || self.version.checked_add(1) != Some(version) {
            return Err(invalid());
        }
        Ok(ActionVersionAdvance {
            before: self.clone(),
            version,
        })
    }
}

/// Describes only the runtime's committed version advancement. The result may
/// still be an error; losing this receipt would strand a successful Prepare.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionVersionAdvance {
    before: ActionVersion,
    version: i64,
}

impl ActionVersionAdvance {
    pub fn version(&self) -> i64 {
        self.version
    }

    /// Join consecutive commits by this runtime without accepting a gap or a
    /// different action's advancement. Each commit still compares storage.
    pub fn chain(self, next: Self) -> Result<Self, AgentError> {
        let mut expected = self.before.clone();
        expected.version = self.version;
        if next.before != expected {
            return Err(invalid());
        }
        Ok(Self {
            before: self.before,
            version: next.version,
        })
    }

    /// Only advance the held version. In particular, never copy newer input,
    /// authority, conversation content or a replacement lease into this loop.
    pub fn apply(
        &self,
        session: &mut PersistedAgentSession,
        expected: Option<&ActionVersion>,
    ) -> Result<(), AgentError> {
        if expected != Some(&self.before)
            || ActionVersion::capture(session, &self.before.tool_call_id)?.as_ref()
                != Some(&self.before)
            || self.version <= self.before.version
        {
            return Err(invalid());
        }
        session.version = self.version;
        Ok(())
    }
}

fn invalid() -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: "invalid Assistant action version handoff".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{ChatMessage, ToolCallRef},
        session::AgentSessionSurface,
    };
    use desk_agent_protocol::{AgentScope, ExecutionMode};

    pub(super) fn session() -> PersistedAgentSession {
        let mut session = PersistedAgentSession::new(
            "run",
            "actor",
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
        session.surface = AgentSessionSurface::DeviceAssistant;
        session.turn_state = TurnState::AwaitingApproval;
        session.current_turn_id = Some("turn".into());
        session.input_revision = 3;
        session.lease_token = 4;
        session.version = 10;
        session.conversation.push(ChatMessage::assistant_tool_calls(
            "proposal",
            "",
            vec![ToolCallRef {
                id: "call".into(),
                name: "write".into(),
                arguments_json: "{}".into(),
            }],
        ));
        session
    }

    #[test]
    fn own_consecutive_commits_only_change_held_version_and_cannot_replay() {
        let mut held = session();
        let before = ActionVersion::capture(&held, "call").unwrap().unwrap();
        let first = before.committed(11).unwrap();
        let mut next = before.clone();
        next.version = 11;
        let joined = first.chain(next.committed(12).unwrap()).unwrap();
        joined.apply(&mut held, Some(&before)).unwrap();
        let mut expected = session();
        expected.version = 12;
        assert_eq!(
            held.encode_json_for_storage().unwrap(),
            expected.encode_json_for_storage().unwrap()
        );
        assert!(joined.apply(&mut held, Some(&before)).is_err());
        for bad in [-1, 9, 10, 12, i64::MAX] {
            assert!(before.committed(bad).is_err());
        }
        next.version = i64::MAX;
        assert!(next.committed(i64::MIN).is_err());
        next.version = 12;
        assert!(
            before
                .committed(11)
                .unwrap()
                .chain(next.committed(13).unwrap())
                .is_err()
        );
    }

    #[test]
    fn receipt_rejects_every_changed_identity_and_nonassistant_surface() {
        for field in [
            "run", "actor", "device", "turn", "input", "lease", "version", "state", "call",
            "surface",
        ] {
            let mut held = session();
            let before = ActionVersion::capture(&held, "call").unwrap().unwrap();
            let receipt = before.committed(11).unwrap();
            match field {
                "run" => held.conversation_id.push('x'),
                "actor" => held.actor_id.push('x'),
                "device" => held.device_id.push('x'),
                "turn" => held.current_turn_id = Some("other".into()),
                "input" => held.input_revision += 1,
                "lease" => held.lease_token += 1,
                "version" => held.version += 1,
                "state" => held.turn_state = TurnState::Running,
                "call" => held.conversation.clear(),
                "surface" => held.surface = AgentSessionSurface::default(),
                _ => unreachable!(),
            }
            let original = held.encode_json_for_storage().unwrap();
            assert!(receipt.apply(&mut held, Some(&before)).is_err(), "{field}");
            assert_eq!(held.encode_json_for_storage().unwrap(), original);
        }
        let mut held = session();
        let before = ActionVersion::capture(&held, "call").unwrap().unwrap();
        assert!(
            before
                .committed(11)
                .unwrap()
                .apply(&mut held, None)
                .is_err()
        );
        let mut other = before.clone();
        other.tool_call_id = "other".into();
        assert!(
            other
                .committed(11)
                .unwrap()
                .apply(&mut held, Some(&before))
                .is_err()
        );
        assert!(
            before
                .committed(11)
                .unwrap()
                .chain(other.committed(11).unwrap())
                .is_err()
        );
    }
}
