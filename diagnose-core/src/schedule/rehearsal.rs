//! Verify interactive rehearsal session evidence; this does not verify permission coverage.
use crate::{
    chat::{ChatMessage, ChatRole},
    dynamic_run::PermissionRequestState,
    file_scope::DirectoryConsentState,
    session::{
        AgentSessionSurface, ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
    },
};

pub struct RehearsalSource<'a> {
    pub actor: &'a str,
    pub device: &'a str,
    pub conversation: &'a str,
    pub client_conversation: &'a str,
    pub input_message_id: &'a str,
    pub prompt: &'a str,
}

pub fn answered_message<'a>(
    session: &'a PersistedAgentSession,
    source: &RehearsalSource<'_>,
    answer: &str,
) -> Option<&'a ChatMessage> {
    if session.actor_id != source.actor
        || session.device_id != source.device
        || session.conversation_id != source.conversation
        || session.client_conversation_id.as_deref() != Some(source.client_conversation)
        || session.surface != AgentSessionSurface::DeviceAssistant
        || !matches!(
            session.trigger_origin,
            TriggerOrigin::User | TriggerOrigin::PermissionDecision
        )
        || session.turn_state != TurnState::Idle
        || session.terminal_error.is_some()
        || session.terminal_permission_request_id.is_some()
        || session.execution_state != ExecutionState::None
        || session.input_revision != 1
        || session.latest_input_seq != 1
        || session.handled_input_seq != 1
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || session
            .permission_requests
            .iter()
            .any(|request| request.state == PermissionRequestState::Pending)
        || session
            .file_scope
            .records()
            .iter()
            .any(|record| record.state == DirectoryConsentState::Pending)
        || answer.trim().is_empty()
    {
        return None;
    }
    let inputs: Vec<_> = session
        .conversation
        .iter()
        .filter(|message| {
            message.role == ChatRole::User
                && !crate::permission_resume::is_resume_control_message(message)
        })
        .collect();
    if inputs.len() != 1
        || inputs[0].message_id != source.input_message_id
        || inputs[0].text != source.prompt
        || inputs[0].turn_id.is_none()
    {
        return None;
    }
    let turn = session.current_turn_id.as_deref()?;
    session
        .conversation
        .iter()
        .rev()
        .find(|message| message.role == ChatRole::Assistant)
        .filter(|message| {
            message.turn_id.as_deref() == Some(turn)
                && message.tool_calls.is_empty()
                && message.text == answer
        })
}

use crate::provider_preflight::ObservedCapabilityAuthority;
use desk_agent_protocol::capability_grant::CapabilityGrantIssuer;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ObservedRehearsalRead {
    pub call_id: String,
    pub tool_call_id: String,
    pub grant_id: String,
    pub issued_by: CapabilityGrantIssuer,
    pub authority: ObservedCapabilityAuthority,
    pub completed_at: i64,
    pub output_sha256: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RehearsalReadReport {
    pub rehearsal_id: String,
    pub session_sha256: String,
    pub reads: Vec<ObservedRehearsalRead>,
    /// Consumption without completion is not an observed successful read.
    pub unconfirmed_read_call_ids: Vec<String>,
    /// Other providers and control tools still require their own evidence adapters.
    pub other_tool_call_ids: Vec<String>,
}

/// Classify a settled first-input failure without any tool history. Callers must
/// additionally lock and verify the durable session row and absence of action
/// records; this session projection alone does not authorize a retry.
pub fn terminal_without_tools(
    session: &PersistedAgentSession,
    source: &RehearsalSource<'_>,
) -> Option<TurnState> {
    if !matches!(session.turn_state, TurnState::Failed | TurnState::Cancelled)
        || session.actor_id != source.actor
        || session.device_id != source.device
        || session.conversation_id != source.conversation
        || session.client_conversation_id.as_deref() != Some(source.client_conversation)
        || session.surface != AgentSessionSurface::DeviceAssistant
        || session.trigger_origin != TriggerOrigin::User
        || session.current_turn_id.as_deref().is_none_or(str::is_empty)
        || session.execution_state != ExecutionState::None
        || session.terminal_permission_request_id.is_some()
        || session.input_revision != 1
        || session.latest_input_seq != 1
        // Failed turns do not advance the handled watermark in the shared loop.
        || session.handled_input_seq > 1
        || !session.pending_auto_triggers.is_empty()
        || !session.permission_requests.is_empty()
        || session
            .file_scope
            .records()
            .iter()
            .any(|record| record.state == DirectoryConsentState::Pending)
        || session.conversation.iter().any(|message| {
            message.role == ChatRole::Tool
                || !message.tool_calls.is_empty()
                || message.tool_call_id.is_some()
                || message.background_task_id.is_some()
                || crate::permission_resume::is_resume_control_message(message)
        })
    {
        return None;
    }
    let mut inputs = session
        .conversation
        .iter()
        .filter(|message| message.role == ChatRole::User);
    let input = inputs.next()?;
    if inputs.next().is_some()
        || input.message_id != source.input_message_id
        || input.text != source.prompt
        || input.image_data_url.is_some()
        || input.turn_id != session.current_turn_id
    {
        return None;
    }
    Some(session.turn_state)
}

#[cfg(test)]
mod terminal_tests;

pub mod read_sources;

pub mod model_source;

pub mod fixed_input;

pub mod sources;

pub mod permission_controls;

mod paused_controls;
