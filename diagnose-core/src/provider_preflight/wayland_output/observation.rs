//! Resolve only intact screenshot observations returned in this conversation.
use super::*;
use crate::chat::{ChatMessage, ChatRole};
use desk_agent_protocol::{
    ScreenCaptureOutput,
    computer_use::{OutputFrameBinding, RawInputScreenContext},
};
#[cfg(test)]
mod tests;

pub fn observed_output(
    history: &[ChatMessage],
    id: &str,
    now_unix_ms: u64,
) -> Result<ScreenCaptureOutput, AgentError> {
    if id.is_empty() || id.len() > 512 || now_unix_ms == 0 {
        return Err(unavailable());
    }
    let mut selected = None;
    for message in history.iter().rev() {
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let mut calls = history
            .iter()
            .filter(|m| m.role == ChatRole::Assistant)
            .flat_map(|m| &m.tool_calls)
            .filter(|call| call.id == call_id);
        let Some(call) = calls.next() else {
            continue;
        };
        if call.name != "read_current_screen" || calls.next().is_some() {
            continue;
        }
        let Some(envelope) = &message.data_envelope else {
            continue;
        };
        if envelope.validate().is_err()
            || crate::model_egress::envelope_expires_by(envelope, now_unix_ms)
            || envelope.provenance.source_tool_name != "read_current_screen"
            || envelope.provenance.source_provider_id
                != crate::ai_assistant::CURRENT_SCREEN_PROVIDER_ID
            || envelope.digest_sha256 != format!("{:x}", Sha256::digest(message.text.as_bytes()))
        {
            continue;
        }
        let Ok(value) = crate::image_input::structured_tool_result(&message.text) else {
            continue;
        };
        let Some(value) = value.pointer("/ReadContext/ScreenCaptureCurrent") else {
            continue;
        };
        let Ok(output) = serde_json::from_value::<ScreenCaptureOutput>(value.clone()) else {
            continue;
        };
        if output.truncated || output.window.is_some() {
            continue;
        }
        let Some(observation) = output.frame_observation.as_ref() else {
            continue;
        };
        let Some(reference) = observation.output_reference.as_ref() else {
            continue;
        };
        if reference.object_kind != ObjectKind::DesktopOutput || reference.token != id {
            continue;
        }
        if let Some(previous) = &selected {
            if previous != &output {
                return Err(unavailable());
            }
        } else {
            selected = Some(output);
        }
    }
    selected.ok_or_else(unavailable)
}

pub fn bind_observation(
    history: &[ChatMessage],
    id: &str,
    now_unix_ms: u64,
) -> Result<(ObjectRef, RawInputScreenContext, OutputFrameBinding), AgentError> {
    let output = observed_output(history, id, now_unix_ms)?;
    let observation = output.frame_observation.ok_or_else(unavailable)?;
    Ok((
        observation.output_reference.ok_or_else(unavailable)?,
        RawInputScreenContext {
            display: output.display,
            width: output.width,
            height: output.height,
            dpi_x: output.dpi_x,
            dpi_y: output.dpi_y,
        },
        OutputFrameBinding {
            stream_generation: observation.stream_generation,
            observation_id: observation.observation_id,
            received_at_unix_ms: observation.received_at_unix_ms,
            freshness: observation.freshness,
        },
    ))
}
