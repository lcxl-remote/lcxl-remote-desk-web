//! Immutable edge-issued document targets captured with the original input.

use super::{ReadContextSelection, invalid};
use desk_agent_protocol::{
    AgentError, Capability,
    computer_use::{ComputerUseReadiness, ObjectKind, ObjectRef},
};
use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests;
