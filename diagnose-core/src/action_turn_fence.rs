//! Frozen turn identity for a AI Assistant action. This is concurrency
//! evidence, never a capability grant or permission to execute an operation.

use desk_agent_protocol::{AgentError, AgentErrorKind};
use serde::{Deserialize, Serialize};

use crate::session::{AgentSessionSurface, PersistedAgentSession};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantTurnFence {
    pub schema_version: u16,
    pub conversation_id: String,
    pub turn_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub input_revision: u64,
    pub lease_token: u64,
}

impl AssistantTurnFence {
    /// Never adopt a newer persisted turn on behalf of an older tool call.
    pub fn computer_action_scope_for_session(
        held: Option<&Self>,
        session: &PersistedAgentSession,
    ) -> Result<Option<desk_agent_protocol::computer_use::ComputerActionTurnScope>, AgentError>
    {
        let current = Self::from_session(session)?;
        if held != current.as_ref() {
            return Err(invalid());
        }
        held.map(Self::computer_action_scope).transpose()
    }

    pub fn computer_action_scope(
        &self,
    ) -> Result<desk_agent_protocol::computer_use::ComputerActionTurnScope, AgentError> {
        self.validate()?;
        Ok(desk_agent_protocol::computer_use::ComputerActionTurnScope {
            conversation_id: self.conversation_id.clone(),
            turn_id: self.turn_id.clone(),
            input_revision: self.input_revision,
            lease_token: self.lease_token,
        })
    }

    /// Freeze the loop's held snapshot, not a newly loaded session that might
    /// already belong to a different input or leaseholder.
    pub fn from_session(session: &PersistedAgentSession) -> Result<Option<Self>, AgentError> {
        if session.surface != AgentSessionSurface::AiAssistant {
            return Ok(None);
        }
        if !session.turn_state.is_active() {
            return Err(invalid());
        }
        let fence = Self {
            schema_version: 1,
            conversation_id: session.conversation_id.clone(),
            turn_id: session.current_turn_id.clone().ok_or_else(invalid)?,
            actor_id: session.actor_id.clone(),
            device_id: session.device_id.clone(),
            input_revision: session.input_revision,
            lease_token: session.lease_token,
        };
        fence.validate()?;
        Ok(Some(fence))
    }

    pub fn validate(&self) -> Result<(), AgentError> {
        if self.schema_version != 1
            || self.input_revision == 0
            || self.input_revision > i64::MAX as u64
            || self.lease_token == 0
            || self.lease_token > i64::MAX as u64
            || [
                &self.conversation_id,
                &self.turn_id,
                &self.actor_id,
                &self.device_id,
            ]
            .iter()
            .any(|id| id.trim().is_empty() || id.len() > 256 || id.chars().any(char::is_control))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

fn invalid() -> AgentError {
    AgentError {
        kind: AgentErrorKind::SessionUnavailable,
        message: "invalid AI Assistant action turn fence".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> AssistantTurnFence {
        AssistantTurnFence {
            schema_version: 1,
            conversation_id: "run".into(),
            turn_id: "turn".into(),
            actor_id: "actor".into(),
            device_id: "device".into(),
            input_revision: 3,
            lease_token: 4,
        }
    }

    #[test]
    fn computer_action_scope_preserves_frozen_identity_and_rejects_invalid_fences() {
        let fence = valid();
        let scope = fence.computer_action_scope().unwrap();
        scope.validate().unwrap();
        assert_eq!(scope.conversation_id, fence.conversation_id);
        assert_eq!(scope.turn_id, fence.turn_id);
        assert_eq!(scope.input_revision, fence.input_revision);
        assert_eq!(scope.lease_token, fence.lease_token);
        let mut invalid = fence;
        invalid.actor_id.clear();
        assert!(invalid.computer_action_scope().is_err());
    }

    #[test]
    fn strict_metadata_never_accepts_unknown_version_or_unbounded_identity() {
        let original = valid();
        original.validate().unwrap();
        let json = serde_json::to_string(&original).unwrap();
        assert_eq!(
            serde_json::from_str::<AssistantTurnFence>(&json).unwrap(),
            original
        );
        let mut unknown = serde_json::to_value(&original).unwrap();
        unknown["grant"] = serde_json::json!("not a grant");
        assert!(serde_json::from_value::<AssistantTurnFence>(unknown).is_err());
        for bad in [
            AssistantTurnFence {
                schema_version: 2,
                ..valid()
            },
            AssistantTurnFence {
                input_revision: 0,
                ..valid()
            },
            AssistantTurnFence {
                lease_token: u64::MAX,
                ..valid()
            },
            AssistantTurnFence {
                turn_id: "x".repeat(257),
                ..valid()
            },
            AssistantTurnFence {
                actor_id: "\n".into(),
                ..valid()
            },
        ] {
            assert!(bad.validate().is_err());
        }
    }

    #[test]
    fn freezing_requires_active_assistant_input_and_never_tracks_later_changes() {
        let mut session = PersistedAgentSession::new(
            "run",
            "actor",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            "now",
        );
        assert!(
            AssistantTurnFence::from_session(&session)
                .unwrap()
                .is_none()
        );
        session.surface = AgentSessionSurface::AiAssistant;
        assert!(AssistantTurnFence::from_session(&session).is_err());
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session
            .begin_turn("turn", None, None, 1, session.scope_snapshot.clone(), "now")
            .unwrap();
        let frozen = AssistantTurnFence::from_session(&session).unwrap().unwrap();
        assert!(AssistantTurnFence::computer_action_scope_for_session(None, &session).is_err());
        assert_eq!(
            AssistantTurnFence::computer_action_scope_for_session(Some(&frozen), &session).unwrap(),
            Some(frozen.computer_action_scope().unwrap())
        );
        for field in 0..6 {
            let mut stale = frozen.clone();
            match field {
                0 => stale.conversation_id.push_str("-other"),
                1 => stale.turn_id.push_str("-other"),
                2 => stale.actor_id.push_str("-other"),
                3 => stale.device_id.push_str("-other"),
                4 => stale.input_revision += 1,
                _ => stale.lease_token += 1,
            }
            assert!(
                AssistantTurnFence::computer_action_scope_for_session(Some(&stale), &session)
                    .is_err()
            );
        }
        session.input_revision += 1;
        session.lease_token += 1;
        assert!(
            AssistantTurnFence::computer_action_scope_for_session(Some(&frozen), &session).is_err()
        );
        assert_eq!(frozen.input_revision, 1);
        assert_ne!(
            frozen,
            AssistantTurnFence::from_session(&session).unwrap().unwrap()
        );
    }
}
