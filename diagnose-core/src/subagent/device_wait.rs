//! A worker yields with a durable tool receipt and observes the result on resume.

use crate::{
    chat::{ChatMessage, ChatRole, ToolCall},
    session::PersistedAgentSession,
};
use desk_agent_protocol::AgentError;
use sha2::{Digest, Sha256};

pub const NATIVE_WORK_PENDING: &str = "delegated_device_wait_has_unsettled_native_work";

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceWaitStatus {
    Waiting,
    Ready,
    Unavailable,
}

fn payload(target_ref: &str, status: DeviceWaitStatus) -> serde_json::Value {
    serde_json::json!({"target_ref": target_ref, "status": status})
}

fn invalid_receipt() -> AgentError {
    super::invalid("the device wait receipt is unavailable")
}

pub fn receipt(
    session: &PersistedAgentSession,
    call: &ToolCall,
    message_id: &str,
    target_ref: &str,
    status: DeviceWaitStatus,
) -> Result<ChatMessage, AgentError> {
    if !super::valid_id(message_id)
        || session.unclosed_tool_call_ids().as_slice() != [call.id.as_str()]
        || session
            .conversation
            .iter()
            .any(|message| message.message_id == message_id)
    {
        return Err(invalid_receipt());
    }
    let label = session.conversation.iter()
        .find(|message| {
            message.role == ChatRole::Assistant
                && message.turn_id == session.current_turn_id
                && matches!(message.tool_calls.as_slice(), [reference] if
                    reference.id == call.id && reference.name == super::tools::WAIT_DEVICE
                    && super::tools::same_arguments(&session.ui_references, &call.name, &reference.arguments_json, &call.arguments_json))
        })
        .and_then(|message| message.data_envelope.as_ref())
        .ok_or_else(invalid_receipt)?;
    let text = payload(target_ref, status).to_string();
    let mut message = ChatMessage::tool_result(message_id, &call.id, &text);
    message.tool_ok = Some(true);
    message.data_envelope = Some(super::projection::envelope(
        message_id,
        &text,
        super::tools::WAIT_DEVICE,
        std::slice::from_ref(label),
    )?);
    Ok(message)
}

/// Replace the initial pending result before the request re-enters the model.
pub fn finish_receipt(
    session: &mut PersistedAgentSession,
    message_id: &str,
    target_ref: &str,
    status: DeviceWaitStatus,
) -> Result<(), AgentError> {
    let message = session
        .conversation
        .iter_mut()
        .find(|message| message.message_id == message_id && message.role == ChatRole::Tool)
        .ok_or_else(invalid_receipt)?;
    let label = message
        .data_envelope
        .as_ref()
        .filter(|label| label.provenance.source_tool_name == super::tools::WAIT_DEVICE)
        .ok_or_else(invalid_receipt)?;
    let expected = payload(target_ref, DeviceWaitStatus::Waiting).to_string();
    if message.text != expected {
        return Err(invalid_receipt());
    }
    let text = payload(target_ref, status).to_string();
    let resolved_id = format!("device-wait-{:x}", Sha256::digest(message_id.as_bytes()));
    message.data_envelope = Some(super::projection::envelope(
        &resolved_id,
        &text,
        super::tools::WAIT_DEVICE,
        std::slice::from_ref(label),
    )?);
    message.text = text;
    Ok(())
}
