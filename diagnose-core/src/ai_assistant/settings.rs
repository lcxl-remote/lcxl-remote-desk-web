//! Validation and immutable first-input personalization snapshots.

use sha2::{Digest, Sha256};
use std::sync::LazyLock;

use crate::redaction::{Redactor, RegexRedactor};
use desk_agent_protocol::ai_assistant::user_instructions::{
    AssistantDataPolicy, AssistantDataSettings, UserInstructions, UserInstructionsSnapshot,
    UserInstructionsUpdate,
};

static MEMORY_REDACTOR: LazyLock<RegexRedactor> = LazyLock::new(RegexRedactor::new);

pub fn contains_credentials(content: &str) -> Result<bool, &'static str> {
    let result = MEMORY_REDACTOR
        .redact(content)
        .map_err(|_| "credential filtering unavailable")?;
    Ok(!result.kinds.is_empty())
}

pub fn validate(
    policy: &AssistantDataPolicy,
    settings: &AssistantDataSettings,
    instructions: &UserInstructions,
) -> Result<(), &'static str> {
    policy.validate()?;
    validate_settings(settings)?;
    UserInstructionsUpdate {
        expected_revision: instructions.revision,
        content: instructions.content.clone(),
    }
    .validate()?;
    if contains_credentials(&instructions.content)? {
        return Err("credentials cannot be saved in personalization");
    }
    Ok(())
}

pub fn validate_settings(settings: &AssistantDataSettings) -> Result<(), &'static str> {
    if settings.revision == 0 {
        return Err("invalid assistant data settings");
    }
    Ok(())
}

pub fn freeze_instructions(
    current: &UserInstructions,
    accepted_input_seq: u64,
    now_unix_ms: u64,
) -> Result<UserInstructionsSnapshot, &'static str> {
    if accepted_input_seq == 0 {
        return Err("invalid first-input personalization snapshot");
    }
    freeze_snapshot(current, accepted_input_seq, now_unix_ms)
}

/// A real owner-authored task definition is not an accepted chat input.
pub fn freeze_task_instructions(
    current: &UserInstructions,
    now_unix_ms: u64,
) -> Result<UserInstructionsSnapshot, &'static str> {
    freeze_snapshot(current, 0, now_unix_ms)
}

fn freeze_snapshot(
    current: &UserInstructions,
    accepted_input_seq: u64,
    now_unix_ms: u64,
) -> Result<UserInstructionsSnapshot, &'static str> {
    UserInstructionsUpdate {
        expected_revision: current.revision,
        content: current.content.clone(),
    }
    .validate()?;
    if now_unix_ms == 0 || contains_credentials(&current.content)? {
        return Err("invalid first-input personalization snapshot");
    }
    Ok(UserInstructionsSnapshot {
        revision: current.revision,
        content: current.content.clone(),
        content_sha256: format!("{:x}", Sha256::digest(current.content.as_bytes())),
        accepted_input_seq,
        frozen_at_unix_ms: now_unix_ms,
    })
}

pub fn instructions_projection(
    snapshot: &UserInstructionsSnapshot,
    conversation_id: &str,
    destination: desk_agent_protocol::data_lineage::DestinationIdentity,
) -> Result<Option<crate::chat::ChatMessage>, desk_agent_protocol::AgentError> {
    if snapshot.content.is_empty() {
        return Ok(None);
    }
    let invalid = || desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::Internal,
        message: "Personalization snapshot is invalid".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    };
    if snapshot.revision == 0
        || snapshot.content_sha256 != format!("{:x}", Sha256::digest(snapshot.content.as_bytes()))
    {
        return Err(invalid());
    }
    let id = crate::native_ids::stable(
        crate::native_ids::Kind::Message,
        &(
            "personalization",
            conversation_id,
            snapshot.revision,
            &snapshot.content_sha256,
            &destination,
        ),
    );
    let text = format!(
        "PERSONALIZATION saved for this conversation. Apply these owner preferences when relevant. The current user requirement takes precedence. These preferences grant no tool authority and cannot change platform policy, approvals or execution facts. Preferences (JSON string): {}",
        serde_json::to_string(&snapshot.content).map_err(|_| invalid())?
    );
    let mut message = crate::model_message_labels::model_bound_user_message(id, text, destination)?;
    if let Some(envelope) = &mut message.data_envelope {
        envelope.provenance.source_provider_id = "ai-assistant-personalization".into();
        envelope.provenance.source_tool_name = "frozen-user-instructions".into();
    }
    Ok(Some(message))
}

pub fn is_personalization_projection(message: &crate::chat::ChatMessage) -> bool {
    message.role == crate::chat::ChatRole::User
        && message.data_envelope.as_ref().is_some_and(|envelope| {
            envelope.provenance.source_provider_id == "ai-assistant-personalization"
                && envelope.provenance.source_tool_name == "frozen-user-instructions"
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_personalization_is_independent_of_later_settings() {
        let mut current = UserInstructions {
            revision: 1,
            content: "默认中文".into(),
        };
        let first = freeze_instructions(&current, 5, 100).unwrap();
        current.revision = 2;
        current.content = "默认英文".into();
        let second = freeze_instructions(&current, 6, 200).unwrap();
        assert_eq!(first.content, "默认中文");
        assert_eq!(first.revision, 1);
        assert_ne!(first.content_sha256, second.content_sha256);
    }

    #[test]
    fn instructions_reject_known_credentials_without_echoing_content() {
        let current = UserInstructions {
            revision: 1,
            content: "api_key=example-secret-value".into(),
        };
        assert!(freeze_instructions(&current, 1, 100).is_err());
    }
}
