//! Shared live-selection metadata. These references never authorize device execution.
use crate::{
    capability_availability::CapabilityAvailability,
    context_attachment::{
        AttachmentBounds, AttachmentObjectRef, AttachmentRuntimeBinding, AttachmentStaleReason,
        AttachmentState, CONTEXT_ATTACHMENT_SCHEMA_VERSION, ContextAttachment,
        ContextAttachmentKind, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_OBJECTS,
    },
    provider_registry::ProviderRegistry,
    session::{AgentSessionSurface, PersistedAgentSession},
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::ComputerUseReadiness,
    data_lineage::{
        ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataProvenance,
        DestinationIdentity, RetentionBoundary, Sensitivity,
    },
};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSelectionClaim {
    pub selected_capability_ids: Vec<String>,
    pub runtime_bindings: Vec<AttachmentRuntimeBinding>,
    pub candidates: Vec<ContextAttachment>,
    pub now_unix_ms: u64,
}

pub struct LiveContextBuild<'a> {
    pub registry: &'a ProviderRegistry,
    pub inventory: &'a [CapabilityAvailability],
    pub readiness: Option<&'a ComputerUseReadiness>,
    pub selected_capability_ids: &'a [String],
    pub request_id: &'a str,
    pub actor_id: &'a str,
    pub device_id: &'a str,
    pub destination: &'a DestinationIdentity,
    pub now_unix_ms: u64,
}

fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

pub fn validate_durable_update(
    update: &desk_agent_protocol::device_assistant::DeviceAssistantContextUpdate,
) -> Result<(), AgentError> {
    update.validate().map_err(invalid)?;
    if !crate::conversation_key::is_valid_client_conversation_id(&update.conversation_id) {
        return Err(invalid("Invalid live context conversation id"));
    }
    crate::device_assistant::selected_context_capabilities(&update.selected_capability_ids)
        .map_err(invalid)?;
    if update
        .selected_capability_ids
        .iter()
        .any(|id| id == crate::device_assistant::CURRENT_SCREEN_CAPABILITY_ID)
    {
        return Err(invalid(
            "CurrentScreen is a one-turn selection and cannot be saved as durable context",
        ));
    }
    Ok(())
}

pub fn selection_request_id(request_id: &str, capability_id: &str) -> String {
    format!(
        "select-{:x}",
        Sha256::digest(format!("{request_id}:{capability_id}").as_bytes())
    )
}

/// Identity entropy is supplied by the runtime; the core has no random or I/O dependency.
pub fn build_live_context(
    params: LiveContextBuild<'_>,
    mut fresh_id: impl FnMut() -> String,
) -> Result<ContextSelectionClaim, AgentError> {
    let LiveContextBuild {
        registry,
        inventory,
        readiness,
        selected_capability_ids,
        request_id,
        actor_id,
        device_id,
        destination,
        now_unix_ms,
    } = params;
    crate::device_assistant::selected_context_capabilities(selected_capability_ids)
        .map_err(invalid)?;
    let selected = selected_capability_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    for capability_id in &selected {
        if !inventory
            .iter()
            .any(|item| item.capability_id == *capability_id && item.ready)
        {
            return Err(invalid(format!(
                "selected context capability is no longer ready: {capability_id}"
            )));
        }
    }

    let (incarnation, expires_at_unix_ms) = match readiness {
        Some(readiness) => {
            let expires_at = chrono::DateTime::parse_from_rfc3339(&readiness.expires_at)
                .map_err(|_| invalid("invalid context readiness expiry"))?
                .timestamp_millis();
            let expires_at_unix_ms = u64::try_from(expires_at)
                .map_err(|_| invalid("context readiness expiry predates Unix epoch"))?;
            if expires_at_unix_ms <= now_unix_ms {
                return Err(invalid("selected context readiness expired"));
            }
            (
                readiness.interactive_session_incarnation.clone(),
                expires_at_unix_ms,
            )
        }
        None if selected.is_empty() => ("unavailable".to_string(), now_unix_ms.saturating_add(1)),
        None => return Err(invalid("selected context Provider is unavailable")),
    };

    let runtime_bindings = inventory
        .iter()
        .filter(|item| item.ready)
        .map(|item| AttachmentRuntimeBinding {
            source_provider_id: item.provider_id.clone(),
            source_capability_id: item.capability_id.clone(),
            object_incarnation: incarnation.clone(),
        })
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    for capability_id in selected_capability_ids {
        // CurrentScreen is a sensitive one-turn grant. It controls tool
        // exposure for this turn, but must never become durable session
        // attachment metadata.
        if capability_id == crate::device_assistant::CURRENT_SCREEN_CAPABILITY_ID {
            continue;
        }
        let capability = registry.capability(capability_id).ok_or_else(|| {
            invalid(format!(
                "unknown selected context capability: {capability_id}"
            ))
        })?;
        let provider = registry
            .provider_for_capability(capability_id)
            .ok_or_else(|| invalid("selected context Provider is missing"))?;
        let attachment_id = format!("context-{}", fresh_id());
        let opaque_token = fresh_id();
        let observation_id = format!("selection-{}", fresh_id());
        let metadata = serde_json::to_vec(&serde_json::json!({
            "provider_id": provider.wire.provider_id,
            "capability_id": capability_id,
            "interactive_session_incarnation": incarnation,
        }))
        .map_err(|error| invalid(format!("encode context metadata: {error}")))?;
        let digest = format!("{:x}", Sha256::digest(&metadata));
        let client_request_id = selection_request_id(request_id, capability_id);
        candidates.push(ContextAttachment {
            schema_version: CONTEXT_ATTACHMENT_SCHEMA_VERSION,
            attachment_id: attachment_id.clone(),
            client_request_id,
            actor_id: actor_id.to_string(),
            device_id: device_id.to_string(),
            surface: AgentSessionSurface::DeviceAssistant,
            // Today's selector binds the capability to the exact worker
            // incarnation. More specific Office/file/range selectors replace
            // this with their own immutable object kinds and incarnations.
            kind: ContextAttachmentKind::InteractiveSession,
            object_ref: AttachmentObjectRef {
                opaque_token: opaque_token.clone(),
                object_incarnation: incarnation.clone(),
                source_provider_id: provider.wire.provider_id.clone(),
                source_capability_id: capability_id.clone(),
            },
            bounds: AttachmentBounds {
                max_bytes: capability
                    .wire
                    .limits
                    .max_output_bytes
                    .min(MAX_ATTACHMENT_BYTES),
                max_objects: capability
                    .wire
                    .limits
                    .max_objects
                    .min(MAX_ATTACHMENT_OBJECTS),
            },
            display_summary: format!("{capability_id} on the current interactive session"),
            created_at_unix_ms: now_unix_ms,
            expires_at_unix_ms,
            envelope: DataEnvelope {
                schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
                envelope_id: format!("envelope-{attachment_id}"),
                content: ContentRef::EphemeralObservation {
                    observation_id,
                    size_bytes: u64::try_from(metadata.len()).unwrap_or(u64::MAX),
                    expires_at_unix_ms,
                },
                provenance: DataProvenance {
                    source_provider_id: provider.wire.provider_id.clone(),
                    source_tool_name: capability.wire.tool_name.clone(),
                    source_object_id: Some(opaque_token),
                    source_envelope_ids: Vec::new(),
                },
                digest_sha256: digest,
                sensitivity: Sensitivity::UserContent,
                allowed_destinations: vec![destination.clone()],
                retention: RetentionBoundary {
                    expires_at_unix_ms: Some(expires_at_unix_ms),
                    delete_with_run: false,
                },
            },
            state: AttachmentState::Active,
        });
    }
    for candidate in &candidates {
        candidate
            .validate()
            .map_err(|_| invalid("Invalid live context metadata"))?;
    }
    Ok(ContextSelectionClaim {
        selected_capability_ids: selected_capability_ids.to_vec(),
        runtime_bindings,
        candidates,
        now_unix_ms,
    })
}

/// Validate every candidate before touching the session, including duplicates.
pub fn reconcile_live_context(
    session: &mut PersistedAgentSession,
    selection: &ContextSelectionClaim,
) -> Result<bool, AgentError> {
    let mut next = session.clone();
    let changed = reconcile(&mut next, selection)?;
    *session = next;
    Ok(changed)
}

fn reconcile(
    session: &mut PersistedAgentSession,
    selection: &ContextSelectionClaim,
) -> Result<bool, AgentError> {
    for candidate in &selection.candidates {
        candidate
            .validate()
            .map_err(|_| invalid("Invalid live context candidate"))?;
        if candidate.actor_id != session.actor_id
            || candidate.device_id != session.device_id
            || candidate.surface != session.surface
            || candidate.kind != ContextAttachmentKind::InteractiveSession
            || !candidate.is_active_at(selection.now_unix_ms)
            || !selection
                .selected_capability_ids
                .contains(&candidate.object_ref.source_capability_id)
        {
            return Err(invalid(
                "Live context candidate does not match the selected subject",
            ));
        }
    }
    let mut changed = false;
    // CurrentScreen is deliberately ephemeral. Older builds briefly wrote
    // attachment metadata for it; remove that metadata on the next claim as
    // well as preventing new entries in the orchestrator.
    let attachment_count = session.context_attachments.len();
    session
        .context_attachments
        .retain(|attachment| attachment.kind != ContextAttachmentKind::CurrentScreen);
    changed |= session.context_attachments.len() != attachment_count;
    for attachment in session.context_attachments.iter_mut().filter(|attachment| {
        attachment.kind == ContextAttachmentKind::InteractiveSession
            && matches!(attachment.state, AttachmentState::Active)
            && selection
                .selected_capability_ids
                .contains(&attachment.object_ref.source_capability_id)
    }) {
        let reason = attachment
            .stale_reason_against(selection.now_unix_ms, &selection.runtime_bindings)
            .or_else(|| {
                selection
                    .candidates
                    .iter()
                    .find(|candidate| {
                        candidate.object_ref.source_provider_id
                            == attachment.object_ref.source_provider_id
                            && candidate.object_ref.source_capability_id
                                == attachment.object_ref.source_capability_id
                    })
                    .filter(|candidate| {
                        candidate.envelope.allowed_destinations
                            != attachment.envelope.allowed_destinations
                    })
                    .map(|_| AttachmentStaleReason::PolicyNarrowed)
            });
        if let Some(reason) = reason {
            attachment.mark_stale(reason);
            changed = true;
        }
    }

    let selected = selection
        .selected_capability_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let detach = session
        .context_attachments
        .iter()
        .filter(|attachment| {
            matches!(attachment.state, AttachmentState::Active)
                && attachment.kind == ContextAttachmentKind::InteractiveSession
                && !selected.contains(attachment.object_ref.source_capability_id.as_str())
        })
        .map(|attachment| attachment.attachment_id.clone())
        .collect::<Vec<_>>();
    for attachment_id in detach {
        changed |= session.detach_context(&attachment_id);
    }

    for candidate in &selection.candidates {
        let already_active = session.context_attachments.iter().any(|attachment| {
            matches!(attachment.state, AttachmentState::Active)
                && attachment.object_ref.source_provider_id
                    == candidate.object_ref.source_provider_id
                && attachment.object_ref.source_capability_id
                    == candidate.object_ref.source_capability_id
                && attachment.object_ref.object_incarnation
                    == candidate.object_ref.object_incarnation
        });
        if !already_active {
            changed |= session
                .attach_context(candidate.clone())
                .map_err(|error| invalid(format!("reconcile Device Assistant context: {error}")))?;
        }
    }

    Ok(changed)
}

#[cfg(test)]
mod tests;
