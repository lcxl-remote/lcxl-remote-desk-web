//! Read-only metadata for listing and removing an unsupported session snapshot.
//! This projection is never an execution session or an authorization snapshot.

use super::{AgentSessionSurface, TurnState};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct SessionManagementMetadata {
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub surface: AgentSessionSurface,
    #[serde(default)]
    #[serde(rename = "agent_role")]
    role: ManagementRole,
    #[serde(default)]
    pub turn_state: TurnState,
    #[serde(default)]
    pub current_request_id: Option<String>,
    #[serde(default)]
    conversation: Vec<ManagementMessage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ManagementRole {
    #[default]
    Main,
    SubAgent,
}

#[derive(Debug, Deserialize)]
struct ManagementMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    data_envelope: Option<serde_json::Value>,
}

impl SessionManagementMetadata {
    pub fn decode(raw: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(raw)
    }

    pub fn matches_subject(&self, conversation: &str, actor: &str, device: &str) -> bool {
        self.conversation_id == conversation && self.actor_id == actor && self.device_id == device
    }

    pub fn is_main(&self) -> bool {
        matches!(self.role, ManagementRole::Main)
    }

    pub fn first_question(&self) -> Option<String> {
        self.conversation
            .iter()
            .find(|message| message.is_visible_user())
            .map(|message| message.text.clone())
    }

    pub fn message_count(&self) -> usize {
        self.conversation
            .iter()
            .filter(|message| !message.is_runtime_control())
            .count()
    }
}

impl ManagementMessage {
    fn is_runtime_control(&self) -> bool {
        self.role == "user"
            && self
                .data_envelope
                .as_ref()
                .and_then(|value| value.pointer("/provenance/source_provider_id"))
                .and_then(serde_json::Value::as_str)
                == Some("assistant-runtime-control")
    }

    fn is_visible_user(&self) -> bool {
        self.role == "user" && !self.is_runtime_control()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::PersistedAgentSession;
    use desk_agent_protocol::AgentScope;
    use desk_agent_protocol::ExecutionMode;

    #[test]
    fn metadata_survives_retired_fields_without_restoring_execution() {
        let mut value = serde_json::to_value(PersistedAgentSession::new(
            "session",
            "actor",
            "device",
            1,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ConfirmEachAction,
                expires_at: None,
                policy_name: None,
            },
            "2026-01-01T00:00:00Z",
        ))
        .unwrap();
        value["surface"] = serde_json::json!("ai_assistant");
        value["assistant_data"] = serde_json::json!({"model_history_exclusions": []});
        value["conversation"] =
            serde_json::json!([{ "role": "user", "text": "Original question" }]);
        let raw = value.to_string();
        assert!(PersistedAgentSession::decode_json(&raw).is_err());
        let metadata = SessionManagementMetadata::decode(&raw).unwrap();
        assert!(metadata.matches_subject("session", "actor", "device"));
        assert!(!metadata.matches_subject("session", "another", "device"));
        assert!(metadata.is_main());
        assert_eq!(
            metadata.first_question().as_deref(),
            Some("Original question")
        );
        assert_eq!(metadata.message_count(), 1);
    }
}
