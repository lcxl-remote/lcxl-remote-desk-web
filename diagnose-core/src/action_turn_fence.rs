//! Frozen turn identity for a Device Assistant action. This is concurrency
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
    /// Freeze the loop's held snapshot, not a newly loaded session that might
    /// already belong to a different input or leaseholder.
    pub fn from_session(session: &PersistedAgentSession) -> Result<Option<Self>, AgentError> {
        if session.surface != AgentSessionSurface::DeviceAssistant {
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
        message: "invalid Device Assistant action turn fence".into(),
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
        session.surface = AgentSessionSurface::DeviceAssistant;
        assert!(AssistantTurnFence::from_session(&session).is_err());
        session.input_revision = 1;
        session.latest_input_seq = 1;
        session
            .begin_turn("turn", None, None, 1, session.scope_snapshot.clone(), "now")
            .unwrap();
        let frozen = AssistantTurnFence::from_session(&session).unwrap().unwrap();
        session.input_revision += 1;
        session.lease_token += 1;
        assert_eq!(frozen.input_revision, 1);
        assert_ne!(
            frozen,
            AssistantTurnFence::from_session(&session).unwrap().unwrap()
        );
    }
}
