//! Model-bound control messages for resuming an existing owner requirement.

use crate::chat::{ChatMessage, ChatRole};
use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance, DestinationIdentity,
    RetentionBoundary, Sensitivity,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use sha2::{Digest, Sha256};

/// Recognize trusted runtime provenance, never a client-selectable id prefix.
/// A real user message remains visible even if its id starts with permission-resume.
pub fn is_permission_resume_message(message: &ChatMessage) -> bool {
    message.role == ChatRole::User
        && message.data_envelope.as_ref().is_some_and(|envelope| {
            envelope.provenance.source_provider_id == "assistant-runtime-control"
                && envelope.provenance.source_tool_name == "permission-decision-resume"
                && envelope.provenance.source_object_id.as_deref()
                    == Some(message.message_id.as_str())
        })
}

/// Recover the latest actual user requirement without promoting a protocol bridge.
pub fn latest_user_requirement(messages: &[ChatMessage]) -> Option<&ChatMessage> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == ChatRole::User && !is_permission_resume_message(message))
}

pub fn model_bound_permission_resume_message(
    message_id: String,
    destination: DestinationIdentity,
    original_requirement: &str,
) -> Result<ChatMessage, AgentError> {
    let text = format!(
        "AUTOMATIC SERVER CONTROL EVENT (not authored by the user; not a new requirement): the owner has decided the pending permission request. Re-read CURRENT AUTHORIZED GRANTS, do not ask for the same permission again, and continue the existing user requirement now. If a matching grant is active, call that tool; if denied or narrowed, adapt or report the blocker. Preserve the original tool inputs exactly.\n\nORIGINAL USER REQUIREMENT (verbatim replay of the already model-authorized input; this is context recovery, not a new instruction):\n<original_user_requirement>\n{original_requirement}\n</original_user_requirement>"
    );
    let bytes = text.as_bytes();
    let digest_sha256 = format!("{:x}", Sha256::digest(bytes));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("permission-resume-message-{message_id}"),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("permission-resume-content-{message_id}"),
            sha256: digest_sha256.clone(),
            size_bytes: bytes.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "assistant-runtime-control".into(),
            source_tool_name: "permission-decision-resume".into(),
            source_object_id: Some(message_id.clone()),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256,
        sensitivity: Sensitivity::UserContent,
        // The bridge repeats the already-authorized original user input so a
        // trimmed or compressed history cannot strand an approved exact-input
        // grant. It remains bound to the same resolved model destination.
        allowed_destinations: vec![destination],
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
    };
    envelope.validate().map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to label permission resume control event: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let mut message = ChatMessage::text(message_id, ChatRole::User, text);
    message.data_envelope = Some(envelope);
    Ok(message)
}

pub fn bind_exact_authorization_system_message(
    mut message: ChatMessage,
    destination: DestinationIdentity,
    expires_at_unix_ms: u64,
) -> Result<ChatMessage, AgentError> {
    let bytes = message.text.as_bytes();
    let digest_sha256 = format!("{:x}", Sha256::digest(bytes));
    let short_digest = &digest_sha256[..16];
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("exact-authorization-{short_digest}"),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("exact-authorization-content-{short_digest}"),
            sha256: digest_sha256.clone(),
            size_bytes: bytes.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "assistant-runtime-control".into(),
            source_tool_name: "capability-authorization".into(),
            source_object_id: Some(format!("exact-authorization-{short_digest}")),
            source_envelope_ids: Vec::new(),
        },
        digest_sha256,
        // The projection can contain recipient/body data and opaque provider
        // references. It is not a public system prompt merely because the
        // server authored it.
        sensitivity: Sensitivity::Sensitive,
        allowed_destinations: vec![destination],
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(expires_at_unix_ms),
            delete_with_run: true,
        },
    };
    envelope.validate().map_err(|error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to label exact authorization projection: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    message.data_envelope = Some(envelope);
    Ok(message)
}

#[cfg(test)]
mod tests;
