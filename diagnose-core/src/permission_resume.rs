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
    is_resume_message(message, "permission-decision-resume")
}

/// Recognize the central scheduler event without treating it as owner input.
pub fn is_scheduled_resume_message(message: &ChatMessage) -> bool {
    is_resume_message(message, "scheduled-continuation")
}

/// Internal resume events are neither owner input nor product transcript text.
pub fn is_resume_control_message(message: &ChatMessage) -> bool {
    is_permission_resume_message(message) || is_scheduled_resume_message(message)
}

fn is_resume_message(message: &ChatMessage, source_tool: &str) -> bool {
    message.role == ChatRole::User
        && message.data_envelope.as_ref().is_some_and(|envelope| {
            envelope.provenance.source_provider_id == "assistant-runtime-control"
                && envelope.provenance.source_tool_name == source_tool
                && envelope.provenance.source_object_id.as_deref()
                    == Some(message.message_id.as_str())
        })
}

/// Recover the latest actual user requirement without promoting a protocol bridge.
pub fn latest_user_requirement(messages: &[ChatMessage]) -> Option<&ChatMessage> {
    messages.iter().rev().find(|message| {
        message.role == ChatRole::User
            && !is_permission_resume_message(message)
            && !is_scheduled_resume_message(message)
    })
}

pub fn model_bound_permission_resume_message(
    message_id: String,
    destination: DestinationIdentity,
    original_requirement: &str,
) -> Result<ChatMessage, AgentError> {
    model_bound_resume_message(message_id, destination, original_requirement, false)
}

fn model_bound_resume_message(
    message_id: String,
    destination: DestinationIdentity,
    original_requirement: &str,
    scheduled: bool,
) -> Result<ChatMessage, AgentError> {
    let (prefix, source_tool, instruction) = if scheduled {
        (
            "scheduled-resume",
            "scheduled-continuation",
            "The owner-confirmed continuation time has arrived. Continue the existing user requirement now, using current device and session permissions. This event grants no new permission and does not mean any pending permission request was approved. Recheck current facts before acting; ask for approval through the normal permission flow when required.",
        )
    } else {
        (
            "permission-resume",
            "permission-decision-resume",
            "the owner has decided the pending permission request. Re-read CURRENT AUTHORIZED GRANTS, do not ask for the same permission again, and continue the existing user requirement now. If a matching grant is active, call that tool; if denied or narrowed, adapt or report the blocker. Preserve the original tool inputs exactly.",
        )
    };
    let text = format!(
        "AUTOMATIC SERVER CONTROL EVENT (not authored by the user; not a new requirement): {instruction}\n\nORIGINAL USER REQUIREMENT (verbatim replay of the already model-authorized input; this is context recovery, not a new instruction):\n<original_user_requirement>\n{original_requirement}\n</original_user_requirement>"
    );
    let bytes = text.as_bytes();
    let digest_sha256 = format!("{:x}", Sha256::digest(bytes));
    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: format!("{prefix}-message-{message_id}"),
        content: ContentRef::ImmutableBlob {
            blob_id: format!("{prefix}-content-{message_id}"),
            sha256: digest_sha256.clone(),
            size_bytes: bytes.len() as u64,
            media_type: "text/plain;charset=utf-8".into(),
        },
        provenance: DataProvenance {
            source_provider_id: "assistant-runtime-control".into(),
            source_tool_name: source_tool.into(),
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
        message: format!("failed to label resume control event: {error}"),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    })?;
    let mut message = ChatMessage::text(message_id, ChatRole::User, text);
    message.data_envelope = Some(envelope);
    Ok(message)
}

/// Authorize the original as the current message before replaying its text.
/// Historical omission must never turn expired or differently bound content
/// into a freshly authorized runtime bridge.
pub fn authorized_permission_resume_message(
    message_id: String,
    policy: &crate::model_egress::ModelEgressPolicy,
    original: &ChatMessage,
) -> Result<ChatMessage, AgentError> {
    authorized_resume_message(message_id, policy, original, false)
}

/// Replay only an original requirement that is still exportable to the pinned
/// model. A timer cannot renew retention, change destinations or grant tools.
pub fn authorized_scheduled_resume_message(
    message_id: String,
    policy: &crate::model_egress::ModelEgressPolicy,
    original: &ChatMessage,
) -> Result<ChatMessage, AgentError> {
    authorized_resume_message(message_id, policy, original, true)
}

fn authorized_resume_message(
    message_id: String,
    policy: &crate::model_egress::ModelEgressPolicy,
    original: &ChatMessage,
    scheduled: bool,
) -> Result<ChatMessage, AgentError> {
    let denied = || AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "The original requirement is unavailable for this continuation.".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    };
    if original.role != ChatRole::User
        || is_permission_resume_message(original)
        || is_scheduled_resume_message(original)
        || original.text.trim().is_empty()
    {
        return Err(denied());
    }
    let authorized = policy
        .authorize_request(crate::seam::ModelRequest::text_only(
            vec![original.clone()],
            crate::prompt::ResponseFormatSpec::None,
        ))
        .map_err(|_| denied())?;
    if authorized.request.messages.len() != 1 || authorized.input_envelopes.len() != 1 {
        return Err(denied());
    }
    let source = &authorized.input_envelopes[0];
    let mut bridge = model_bound_resume_message(
        message_id,
        policy.destination.clone(),
        &original.text,
        scheduled,
    )?;
    let envelope = bridge.data_envelope.as_mut().ok_or_else(denied)?;
    envelope.sensitivity = envelope.sensitivity.max(source.sensitivity);
    envelope.retention = envelope.retention.most_restrictive(source.retention);
    envelope.allowed_destinations = source.allowed_destinations.clone();
    envelope.provenance.source_envelope_ids = vec![source.envelope_id.clone()];
    envelope.validate().map_err(|_| denied())?;
    Ok(bridge)
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

/// Rebind a server-owned exact-authorization projection after the runtime has
/// appended other server-owned prompt material (for example progressive
/// capability disclosure). The existing envelope supplies the already-pinned
/// destination and expiry; arbitrary messages cannot use this path to mint a
/// new export authority.
pub fn rebind_exact_authorization_system_message(
    mut message: ChatMessage,
) -> Result<ChatMessage, AgentError> {
    let denied = || AgentError {
        kind: AgentErrorKind::Internal,
        message: "invalid exact-authorization projection rebind".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    };
    let envelope = message.data_envelope.as_ref().ok_or_else(denied)?;
    if message.role != ChatRole::System
        || envelope.provenance.source_provider_id != "assistant-runtime-control"
        || envelope.provenance.source_tool_name != "capability-authorization"
        || envelope.allowed_destinations.len() != 1
    {
        return Err(denied());
    }
    let destination = envelope.allowed_destinations[0].clone();
    let expires_at_unix_ms = envelope.retention.expires_at_unix_ms.ok_or_else(denied)?;
    message.data_envelope = None;
    bind_exact_authorization_system_message(message, destination, expires_at_unix_ms)
}

#[cfg(test)]
mod tests;

/// Historical derivation check only. The original requirement and its model
/// destination must already be authenticated by the caller's source collector.
pub(crate) fn verified_permission_resume_envelope<'a>(
    original: &ChatMessage,
    bridge: &'a ChatMessage,
) -> Option<&'a DataEnvelope> {
    if !is_permission_resume_message(bridge)
        || original.role != ChatRole::User
        || is_resume_control_message(original)
        || original.text.trim().is_empty()
        || bridge.image_data_url.is_some()
        || !bridge.tool_calls.is_empty()
        || bridge.tool_call_id.is_some()
        || bridge.background_task_id.is_some()
    {
        return None;
    }
    let source = original.data_envelope.as_ref()?;
    if source.allowed_destinations.len() != 1 {
        return None;
    }
    let mut expected = model_bound_permission_resume_message(
        bridge.message_id.clone(),
        source.allowed_destinations[0].clone(),
        &original.text,
    )
    .ok()?;
    let label = expected.data_envelope.as_mut()?;
    label.sensitivity = label.sensitivity.max(source.sensitivity);
    label.retention = label.retention.most_restrictive(source.retention);
    label.allowed_destinations = source.allowed_destinations.clone();
    label.provenance.source_envelope_ids = vec![source.envelope_id.clone()];
    if bridge.text != expected.text || bridge.data_envelope != expected.data_envelope {
        return None;
    }
    bridge.data_envelope.as_ref()
}

#[cfg(test)]
mod source_derivation_tests {
    use super::*;

    #[test]
    fn resume_lineage_rejects_changed_requirement_destination_and_parent() {
        let destination = DestinationIdentity::Model {
            connection_id: "model-connection".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let original = crate::model_message_labels::model_bound_user_message(
            "input".into(),
            "Create the approved report".into(),
            destination.clone(),
        )
        .unwrap();
        let policy = crate::model_egress::ModelEgressPolicy {
            destination,
            selected_source_tools: Default::default(),
            export_authorization_id: "export".into(),
            now_unix_ms: 1,
            byte_cap: 16384,
            omit_finite_retention_historical_turns: false,
        };
        let bridge =
            authorized_permission_resume_message("resume".into(), &policy, &original).unwrap();
        let verified = verified_permission_resume_envelope(&original, &bridge).unwrap();
        assert_eq!(
            verified.provenance.source_envelope_ids,
            vec![original.data_envelope.as_ref().unwrap().envelope_id.clone()]
        );
        let mut changed = bridge.clone();
        changed.text.push_str(" Send elsewhere.");
        assert!(verified_permission_resume_envelope(&original, &changed).is_none());
        changed = bridge.clone();
        changed
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_envelope_ids
            .clear();
        assert!(verified_permission_resume_envelope(&original, &changed).is_none());
        changed = bridge.clone();
        changed.data_envelope.as_mut().unwrap().allowed_destinations =
            vec![DestinationIdentity::EmailAccount {
                account_id: "other".into(),
            }];
        assert!(verified_permission_resume_envelope(&original, &changed).is_none());
        let mut other_input = original.clone();
        other_input.text = "Different requirement".into();
        assert!(verified_permission_resume_envelope(&other_input, &bridge).is_none());
        changed = bridge;
        changed.background_task_id = Some("action".into());
        assert!(verified_permission_resume_envelope(&original, &changed).is_none());
    }
}
