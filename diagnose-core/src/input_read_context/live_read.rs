//! Immutable edge-issued document targets captured with the original input.

use super::{ReadContextSelection, invalid};
use desk_agent_protocol::{
    AgentError, Capability,
    computer_use::{ComputerUseReadiness, ObjectKind, ObjectRef},
};
use serde::{Deserialize, Serialize};

use crate::{
    context_attachment::{AttachmentState, ContextAttachmentKind},
    provider_registry::ProviderRegistry,
    session::PersistedAgentSession,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveReadTarget {
    pub tool_name: String,
    pub object_ref: ObjectRef,
    pub interactive_session_incarnation: String,
    /// The original readiness deadline; refreshes cannot extend this input.
    pub readiness_expires_at_unix_ms: u64,
}

pub fn target_kind(name: &str) -> Option<(Capability, ObjectKind)> {
    Some(match name {
        "inspect_office_selection" => (
            Capability::OfficeDocumentInspect,
            ObjectKind::OfficeDocument,
        ),
        "inspect_live_spreadsheet" => (Capability::SpreadsheetLiveInspect, ObjectKind::Range),
        "inspect_live_document" => (Capability::DocumentLiveInspect, ObjectKind::Document),
        "inspect_live_presentation" => (Capability::PresentationLiveInspect, ObjectKind::Slide),
        _ => return None,
    })
}

fn millis(value: &str) -> Result<u64, AgentError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
        .ok_or_else(|| invalid("invalid original live target deadline"))
}

fn bounded(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 4096
}

pub fn validate_targets(selection: &ReadContextSelection) -> Result<(), AgentError> {
    if selection.live_targets.len() > 4
        || selection
            .live_targets
            .windows(2)
            .any(|pair| pair[0].tool_name >= pair[1].tool_name)
    {
        return Err(invalid("original live targets are not canonical"));
    }
    for target in &selection.live_targets {
        let (_, kind) =
            target_kind(&target.tool_name).ok_or_else(|| invalid("invalid original live tool"))?;
        if !selection.tool_names.contains(&target.tool_name)
            || target.object_ref.object_kind != kind
            || !bounded(&target.object_ref.token)
            || !bounded(&target.object_ref.snapshot_id)
            || !bounded(&target.interactive_session_incarnation)
            || target.readiness_expires_at_unix_ms == 0
        {
            return Err(invalid("invalid original live target binding"));
        }
        millis(&target.object_ref.expires_at)?;
    }
    Ok(())
}

/// Capture only final exposed read tools, never all objects reported by the edge.
/// The caller must obtain readiness from its authenticated target connection.
pub fn capture(
    selection: &ReadContextSelection,
    readiness: Option<&ComputerUseReadiness>,
    now: u64,
) -> Result<Vec<LiveReadTarget>, AgentError> {
    selection.validate()?;
    if !selection.live_targets.is_empty() {
        return Err(invalid("original live targets cannot be recaptured"));
    }
    let mut targets = Vec::new();
    for name in &selection.tool_names {
        let Some((capability, _)) = target_kind(name) else {
            continue;
        };
        let readiness = readiness.ok_or_else(|| invalid("live target readiness unavailable"))?;
        readiness
            .validate()
            .map_err(|_| invalid("invalid live target readiness"))?;
        let reference = readiness
            .context_references
            .iter()
            .find(|reference| reference.capability == capability)
            .ok_or_else(|| invalid("selected live target is unavailable"))?;
        targets.push(LiveReadTarget {
            tool_name: name.clone(),
            object_ref: reference.object_ref.clone(),
            interactive_session_incarnation: readiness.interactive_session_incarnation.clone(),
            readiness_expires_at_unix_ms: millis(&readiness.expires_at)?,
        });
    }
    let mut captured = selection.clone();
    captured.live_targets = targets;
    validate_current(&captured, readiness, now)?;
    Ok(captured.live_targets)
}

pub fn target<'a>(
    selection: &'a ReadContextSelection,
    name: &str,
    now: u64,
) -> Result<&'a LiveReadTarget, AgentError> {
    validate_targets(selection)?;
    let target = selection
        .live_targets
        .iter()
        .find(|target| target.tool_name == name)
        .ok_or_else(|| invalid("original live target is missing; submit a new input"))?;
    if expiry(selection, target)? <= now {
        return Err(invalid("original live target expired"));
    }
    Ok(target)
}

pub fn expiry(
    selection: &ReadContextSelection,
    target: &LiveReadTarget,
) -> Result<u64, AgentError> {
    let mut expiry =
        millis(&target.object_ref.expires_at)?.min(target.readiness_expires_at_unix_ms);
    if let Some(scope) = &selection.expires_at {
        expiry = expiry.min(millis(scope)?);
    }
    Ok(expiry)
}

/// A current report may prove availability, but never replaces the old target.
/// Empty legacy targets remain decodable; attempting a live read still requires
/// `target`, which refuses to reconstruct a missing original reference.
pub fn validate_current(
    selection: &ReadContextSelection,
    readiness: Option<&ComputerUseReadiness>,
    now: u64,
) -> Result<(), AgentError> {
    validate_targets(selection)?;
    if selection.live_targets.is_empty() {
        return Ok(());
    }
    let readiness = readiness.ok_or_else(|| invalid("live target readiness unavailable"))?;
    readiness
        .validate()
        .map_err(|_| invalid("invalid live target readiness"))?;
    // Both authenticated readiness registries permit thirty seconds of clock
    // skew. This check must not reject a report they accepted within that bound.
    let observed = millis(&readiness.observed_at)?;
    let current_expiry = millis(&readiness.expires_at)?;
    if current_expiry <= now || observed > now.saturating_add(30_000) || observed >= current_expiry
    {
        return Err(invalid("live target readiness is not current"));
    }
    for original in &selection.live_targets {
        target(selection, &original.tool_name, now)?;
        let (capability, _) = target_kind(&original.tool_name).expect("validated live tool");
        if original.interactive_session_incarnation != readiness.interactive_session_incarnation
            || !readiness
                .capabilities
                .iter()
                .any(|entry| entry.capability == capability && entry.supported && entry.ready)
            || !readiness.context_references.iter().any(|reference| {
                reference.capability == capability && reference.object_ref == original.object_ref
            })
        {
            return Err(invalid("original live target changed or is unavailable"));
        }
    }
    Ok(())
}

/// The durable store checks identity and input revision separately. Model
/// destination comes from the original input envelope, not from readiness.
pub fn validate_input(
    selection: &ReadContextSelection,
    message: &crate::chat::ChatMessage,
    destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
    now: u64,
) -> Result<(), AgentError> {
    validate_targets(selection)?;
    if selection.live_targets.is_empty() {
        return Ok(());
    }
    let envelope = message
        .data_envelope
        .as_ref()
        .ok_or_else(|| invalid("original live input envelope missing"))?;
    if !matches!(
        destination,
        desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
    ) || envelope.allowed_destinations.as_slice() != [destination.clone()]
    {
        return Err(invalid("original live input model destination changed"));
    }
    for original in &selection.live_targets {
        target(selection, &original.tool_name, now)?;
    }
    Ok(())
}

/// Proves that every frozen live target still belongs to the unique durable
/// selection that was active when the original input was accepted.
///
/// A later re-selection cannot revive an older input: attachments created after
/// `input_created_at_unix_ms` are ignored, while a withdrawn historical
/// attachment remains stale and therefore fails closed.
pub fn validate_durable_selection(
    selection: &ReadContextSelection,
    session: &PersistedAgentSession,
    registry: &ProviderRegistry,
    destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
    input_created_at_unix_ms: u64,
    now_unix_ms: u64,
) -> Result<(), AgentError> {
    validate_targets(selection)?;
    for target in &selection.live_targets {
        let capability = registry
            .capability_for_tool(&target.tool_name)
            .ok_or_else(|| invalid("original live capability is unavailable"))?;
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .ok_or_else(|| invalid("original live Provider is unavailable"))?;
        let mut candidates = session
            .context_attachments
            .iter()
            .filter(|attachment| {
                attachment.kind == ContextAttachmentKind::InteractiveSession
                    && attachment.created_at_unix_ms <= input_created_at_unix_ms
                    && attachment.object_ref.source_provider_id == provider.wire.provider_id
                    && attachment.object_ref.source_capability_id == capability.wire.capability_id
                    && attachment.object_ref.object_incarnation
                        == target.interactive_session_incarnation
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|attachment| attachment.created_at_unix_ms);
        let Some(latest) = candidates.last().copied() else {
            return Err(invalid("original live selection is unavailable"));
        };
        if candidates
            .iter()
            .rev()
            .skip(1)
            .any(|candidate| candidate.created_at_unix_ms == latest.created_at_unix_ms)
            || !matches!(latest.state, AttachmentState::Active)
            || !latest.is_active_at(now_unix_ms)
            || latest.envelope.allowed_destinations.as_slice() != [destination.clone()]
        {
            return Err(invalid("original live selection changed or was withdrawn"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

pub fn bind(
    selection: &ReadContextSelection,
    call: &crate::chat::ToolCall,
    input: &mut desk_agent_protocol::OperationInput,
    now: u64,
) -> Result<(), AgentError> {
    use desk_agent_protocol::{ContextKind, OperationInput, ReadContextInput};
    let original = target(selection, &call.name, now)?;
    let OperationInput::ReadContext(ReadContextInput { kind }) = input else {
        return Err(invalid("live read operation mismatch"));
    };
    match (call.name.as_str(), kind) {
        ("inspect_office_selection", ContextKind::OfficeDocumentInspect(params)) => {
            params.document = Some(original.object_ref.clone());
            params.selection_only = true;
            params.max_objects = params.max_objects.min(16);
            params.max_bytes = params.max_bytes.min(256 * 1024);
        }
        ("inspect_live_spreadsheet", ContextKind::SpreadsheetLiveInspect(params))
        | ("inspect_live_document", ContextKind::DocumentLiveInspect(params))
        | ("inspect_live_presentation", ContextKind::PresentationLiveInspect(params)) => {
            params.target = Some(original.object_ref.clone());
            params.batch_file = None;
            params.max_bytes = params.max_bytes.min(256 * 1024);
        }
        _ => return Err(invalid("live read operation mismatch")),
    }
    Ok(())
}

pub(super) fn label(
    binding: &super::object_read::ObjectReadBinding<'_>,
    call: &crate::chat::ToolCall,
    output: &crate::seam::ToolRunOutput,
    mut envelope: desk_agent_protocol::data_lineage::DataEnvelope,
) -> Result<desk_agent_protocol::data_lineage::DataEnvelope, AgentError> {
    use desk_agent_protocol::{
        ContextKind, OperationInput, ReadContextInput,
        data_lineage::{ContentRef, RetentionBoundary},
    };
    use sha2::{Digest, Sha256};
    let original = target(binding.original, &call.name, binding.now_unix_ms)?;
    let (_, mut input) = crate::read_tools::build_read_operation(call)?;
    bind(binding.original, call, &mut input, binding.now_unix_ms)?;
    let bytes = match input {
        OperationInput::ReadContext(ReadContextInput {
            kind: ContextKind::OfficeDocumentInspect(p),
        }) => p.max_bytes,
        OperationInput::ReadContext(ReadContextInput {
            kind:
                ContextKind::SpreadsheetLiveInspect(p)
                | ContextKind::DocumentLiveInspect(p)
                | ContextKind::PresentationLiveInspect(p),
        }) => p.max_bytes,
        _ => return Err(invalid("live read operation mismatch")),
    };
    if output.image_data_url.is_some() || output.content.len() > bytes as usize {
        return Err(invalid("live read output exceeds original bounds"));
    }
    // Identify the exact original target without exposing its executable token.
    let digest = Sha256::digest(
        serde_json::to_vec(original).map_err(|_| invalid("invalid live result source"))?,
    );
    envelope.provenance.source_object_id = Some(format!("live-target:sha256:{digest:x}"));
    let deadline = expiry(binding.original, original)?;
    envelope.allowed_destinations = vec![binding.destination.clone()];
    envelope.retention = envelope.retention.most_restrictive(RetentionBoundary {
        expires_at_unix_ms: Some(deadline),
        delete_with_run: false,
    });
    if let ContentRef::EphemeralObservation {
        expires_at_unix_ms, ..
    } = &mut envelope.content
    {
        *expires_at_unix_ms = (*expires_at_unix_ms).min(deadline);
    }
    envelope
        .validate()
        .map_err(|_| invalid("invalid live result envelope"))?;
    Ok(envelope)
}
