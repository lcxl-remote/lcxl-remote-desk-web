//! Shared construction of owner-selected object metadata, never observation bytes.

use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{ObjectKind, ObjectRef},
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
    device_assistant::{DeviceAssistantObjectContextOperation, DeviceAssistantObjectContextUpdate},
};
use sha2::{Digest, Sha256};

use crate::{
    context_attachment::*,
    device_assistant::*,
    session::{AgentSessionSurface, PersistedAgentSession},
};

#[derive(Debug, Clone)]
pub enum ObjectContextMutation {
    Attach(ContextAttachment),
    Detach {
        attachment_id: String,
    },
    Refresh {
        stale_attachment_id: String,
        replacement: ContextAttachment,
    },
}

/// Generated identifiers and model destination are supplied by the central host.
/// This builds metadata only: the device must still validate the opaque ref on read.
pub struct ObjectContextBuild<'a> {
    pub actor_id: &'a str,
    pub device_id: &'a str,
    pub destination: &'a DestinationIdentity,
    pub now_unix_ms: u64,
    pub attachment_id: &'a str,
    pub observation_id: &'a str,
}

pub fn build_object_context_mutation(
    update: &DeviceAssistantObjectContextUpdate,
    context: ObjectContextBuild<'_>,
) -> Result<ObjectContextMutation, AgentError> {
    update.validate().map_err(|_| invalid())?;
    use DeviceAssistantObjectContextOperation::*;
    match &update.operation {
        Detach { attachment_id } => Ok(ObjectContextMutation::Detach {
            attachment_id: attachment_id.clone(),
        }),
        AttachFile {
            object_ref,
            display_summary,
        }
        | AttachTerminalOutput {
            object_ref,
            display_summary,
        } => Ok(ObjectContextMutation::Attach(build_attachment(
            update,
            object_ref,
            display_summary,
            context,
        )?)),
        RefreshFile {
            stale_attachment_id,
            object_ref,
            display_summary,
        } => Ok(ObjectContextMutation::Refresh {
            stale_attachment_id: stale_attachment_id.clone(),
            replacement: build_attachment(update, object_ref, display_summary, context)?,
        }),
    }
}

fn build_attachment(
    update: &DeviceAssistantObjectContextUpdate,
    object_ref: &ObjectRef,
    display_summary: &str,
    context: ObjectContextBuild<'_>,
) -> Result<ContextAttachment, AgentError> {
    let expiry = chrono::DateTime::parse_from_rfc3339(&object_ref.expires_at)
        .map_err(|_| invalid())?
        .timestamp_millis();
    let expires_at_unix_ms = u64::try_from(expiry).map_err(|_| invalid())?;
    if expires_at_unix_ms <= context.now_unix_ms
        || !matches!(context.destination, DestinationIdentity::Model { .. })
    {
        return Err(invalid());
    }
    let (kind, provider, capability, tool, max_bytes, max_objects) = match object_ref.object_kind {
        ObjectKind::File => (
            ContextAttachmentKind::File,
            FILE_WORKSPACE_PROVIDER_ID,
            FILE_METADATA_CAPABILITY_ID,
            "inspect_selected_file_metadata",
            64 * 1024,
            32,
        ),
        ObjectKind::Directory => (
            ContextAttachmentKind::DirectorySelection,
            FILE_WORKSPACE_PROVIDER_ID,
            FILE_METADATA_CAPABILITY_ID,
            "inspect_selected_file_metadata",
            64 * 1024,
            32,
        ),
        ObjectKind::TerminalOutput => (
            ContextAttachmentKind::TerminalSessionRef,
            TERMINAL_OUTPUT_PROVIDER_ID,
            TERMINAL_OUTPUT_CAPABILITY_ID,
            "inspect_selected_terminal_output",
            32 * 1024,
            8,
        ),
        _ => return Err(invalid()),
    };
    let metadata = serde_json::to_vec(&serde_json::json!({ "provider_id": provider, "capability_id": capability, "object_ref": object_ref, "display_summary": display_summary })).map_err(|_| invalid())?;
    let attachment = ContextAttachment {
        schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
        attachment_id: context.attachment_id.into(),
        client_request_id: update.client_request_id.clone(),
        actor_id: context.actor_id.into(),
        device_id: context.device_id.into(),
        surface: AgentSessionSurface::DeviceAssistant,
        kind,
        object_ref: AttachmentObjectRef {
            opaque_token: serde_json::to_string(object_ref).map_err(|_| invalid())?,
            object_incarnation: format!("{}:{}", object_ref.snapshot_id, object_ref.token),
            source_provider_id: provider.into(),
            source_capability_id: capability.into(),
        },
        bounds: AttachmentBounds {
            max_bytes,
            max_objects,
        },
        display_summary: display_summary.into(),
        created_at_unix_ms: context.now_unix_ms,
        expires_at_unix_ms,
        envelope: DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: format!("envelope-{}", context.attachment_id),
            content: ContentRef::EphemeralObservation {
                observation_id: context.observation_id.into(),
                size_bytes: metadata.len() as u64,
                expires_at_unix_ms,
            },
            provenance: DataProvenance {
                source_provider_id: provider.into(),
                source_tool_name: tool.into(),
                source_object_id: Some(object_ref.token.clone()),
                source_envelope_ids: vec![],
            },
            digest_sha256: format!("{:x}", Sha256::digest(metadata)),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![context.destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(expires_at_unix_ms),
                delete_with_run: false,
            },
        },
        state: AttachmentState::Active,
    };
    attachment.validate().map_err(|_| invalid())?;
    Ok(attachment)
}

pub fn apply_object_mutation(
    session: &mut PersistedAgentSession,
    mutation: &ObjectContextMutation,
) -> Result<bool, AgentError> {
    match mutation {
        ObjectContextMutation::Attach(attachment) => {
            validate_attachment_subject(
                attachment,
                &session.actor_id,
                &session.device_id,
                session.surface,
            )
            .map_err(|_| invalid())?;
            // Same-request replay must be exact; a duplicate object is not proof
            // that a contradictory request was accepted.
            if let Some(old) = session
                .context_attachments
                .iter()
                .find(|old| old.client_request_id == attachment.client_request_id)
            {
                return if same_selection(old, attachment) {
                    Ok(false)
                } else {
                    Err(invalid())
                };
            }
            if session.context_attachments.iter().any(|existing| {
                matches!(existing.state, AttachmentState::Active)
                    && existing.object_ref == attachment.object_ref
            }) {
                return Ok(false);
            }
            session
                .attach_context(attachment.clone())
                .map_err(|_| invalid())
        }
        ObjectContextMutation::Detach { attachment_id } => {
            if !session
                .context_attachments
                .iter()
                .any(|attachment| attachment.attachment_id == *attachment_id)
            {
                return Err(invalid());
            }
            Ok(session.detach_context(attachment_id))
        }
        ObjectContextMutation::Refresh {
            stale_attachment_id,
            replacement,
        } => {
            validate_attachment_subject(
                replacement,
                &session.actor_id,
                &session.device_id,
                session.surface,
            )
            .map_err(|_| invalid())?;
            let replacement = if let Some(old) = session
                .context_attachments
                .iter()
                .find(|old| old.client_request_id == replacement.client_request_id)
            {
                if !same_selection(old, replacement) {
                    return Err(invalid());
                }
                old.clone()
            } else {
                replacement.clone()
            };
            session
                .refresh_context(
                    stale_attachment_id,
                    AttachmentStaleReason::ObjectChanged,
                    replacement,
                )
                .map_err(|_| invalid())
        }
    }
}

/// Ignore only freshly minted observation identity and creation time. Original
/// object expiry, destination, metadata and request identity must remain exact.
fn same_selection(original: &ContextAttachment, candidate: &ContextAttachment) -> bool {
    let mut normalized = candidate.clone();
    normalized.attachment_id = original.attachment_id.clone();
    normalized.created_at_unix_ms = original.created_at_unix_ms;
    normalized.state = original.state.clone();
    normalized.envelope.envelope_id = original.envelope.envelope_id.clone();
    if let (
        ContentRef::EphemeralObservation { observation_id, .. },
        ContentRef::EphemeralObservation {
            observation_id: old,
            ..
        },
    ) = (&mut normalized.envelope.content, &original.envelope.content)
    {
        *observation_id = old.clone();
    }
    normalized == *original
}

fn invalid() -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: "Object context metadata is invalid, unavailable or conflicting".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

#[cfg(test)]
mod tests;
