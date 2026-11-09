//! Atomic tool results for a host-committed complete forget batch.
//! Native owner intent, source admission and the transaction remain host-owned.

use super::*;
use crate::{
    chat::{ChatRole, ToolCall},
    native_ids::{self, Kind},
    session::{PersistedAgentSession, TurnState},
};
use desk_agent_protocol::ai_assistant::memory::{
    DataControlOutcome, DataControlReason, MemoryWriteOutcome, MemoryWriteResult,
};

/// Record a host-validated obsolete request without applying a control, changing
/// generations, or closing the turn. The results contain only platform metadata.
pub fn record_stale_memory_forget(
    session: &mut PersistedAgentSession,
    producer_id: &str,
    receipt: &AssistantDataControlReceipt,
) -> Result<(), &'static str> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or("missing assistant control state")?;
    let start = group_start(session, producer_id)?;
    let producer = &session.conversation[start];
    let calls = forget_calls(&session.ui_references, producer)?;
    if !super::super::data_access::eligible_origin(session)
        || session.trigger_origin != crate::session::TriggerOrigin::User
        || session.turn_state != TurnState::Running
        || session.current_turn_id.is_none()
        || producer.turn_id != session.current_turn_id
        || start + 1 != session.conversation.len()
        || receipt.control_generation < data.control_generation
        || receipt.memory_state_generation < data.memory_state_generation
        || receipt.operation_id != stale_operation_id(session, producer)?
    {
        return Err("stale memory control changed its original owner batch");
    }
    let text = stale_text(receipt)?;
    let mut results = Vec::with_capacity(calls.len());
    for call in calls {
        let id = stale_result_id(receipt, &call.id);
        if session.conversation.iter().any(|message| {
            message.tool_call_id.as_deref() == Some(call.id.as_str()) || message.message_id == id
        }) {
            return Err("stale memory control already has a result");
        }
        let mut result = ChatMessage::tool_result(id, &call.id, &text)
            .with_turn_id(session.current_turn_id.as_ref().unwrap());
        result.tool_ok = Some(false);
        result.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            producer.data_envelope.as_ref(),
            &call.id,
            &text,
            &call.name,
        )
        .map_err(|_| "invalid native stale-control result")?;
        results.push(result);
    }
    session.conversation.extend(results);
    Ok(())
}

fn stale_text(receipt: &AssistantDataControlReceipt) -> Result<String, &'static str> {
    if receipt.reason != DataControlReason::MemoryForget
        || receipt.outcome != DataControlOutcome::StaleTarget
        || receipt.affected_count != 0
        || receipt.cleanup_pending
        || receipt.control_generation == 0
        || receipt.memory_state_generation == 0
    {
        return Err("invalid obsolete memory control result");
    }
    serde_json::to_string(&MemoryWriteResult {
        outcome: MemoryWriteOutcome::Skipped,
        memory: None,
        reason: Some("stale_target".into()),
        control_receipt: Some(receipt.clone()),
    })
    .map_err(|_| "invalid obsolete memory control encoding")
}

fn stale_operation_id(
    session: &PersistedAgentSession,
    producer: &ChatMessage,
) -> Result<String, &'static str> {
    Ok(native_ids::stable(
        Kind::AssistantDataControl,
        &(
            &session.actor_id,
            &session.conversation_id,
            &producer.message_id,
            tool_group_sha256(std::slice::from_ref(producer))?,
        ),
    ))
}

fn stale_result_id(receipt: &AssistantDataControlReceipt, call_id: &str) -> String {
    native_ids::stable(
        Kind::Message,
        &(&receipt.operation_id, call_id, "control-stale"),
    )
}

/// Apply the host-committed result while preserving the original active turn.
pub fn commit_memory_forget_group(
    session: &mut PersistedAgentSession,
    producer_message_id: &str,
    call_message_sha256: &str,
    receipt: &AssistantDataControlReceipt,
    now: &str,
) -> Result<CommittedMemoryForgetGroup, &'static str> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or("missing assistant control state")?;
    let start = group_start(session, producer_message_id)?;
    let producer = &session.conversation[start];
    let calls = forget_calls(&session.ui_references, producer)?;
    if tool_group_sha256(std::slice::from_ref(producer))? != call_message_sha256 {
        return Err("memory control changed its original producer");
    }
    if session.conversation.iter().any(|message| {
        message
            .tool_call_id
            .as_ref()
            .is_some_and(|id| calls.iter().any(|call| call.id == *id))
    }) {
        let committed = read_memory_forget_group(session, producer_message_id)?;
        let original = &committed.receipt;
        if original.operation_id != receipt.operation_id
            || original.reason != receipt.reason
            || original.affected_count != receipt.affected_count
            || original.control_generation != receipt.control_generation
            || original.memory_state_generation != receipt.memory_state_generation
            || !matches!(
                receipt.outcome,
                DataControlOutcome::Applied | DataControlOutcome::AlreadyApplied
            )
        {
            return Err("memory control replay changed its original result");
        }
        return Ok(committed);
    }
    if receipt.reason != DataControlReason::MemoryForget
        || receipt.outcome != DataControlOutcome::Applied
        || !super::super::data_access::eligible_origin(session)
        || session.trigger_origin != crate::session::TriggerOrigin::User
        || session.turn_state != TurnState::Running
        || session.current_turn_id.is_none()
        || receipt.control_generation
            != data
                .control_generation
                .checked_add(1)
                .ok_or("control generation exhausted")?
        || receipt.memory_state_generation
            != data
                .memory_state_generation
                .checked_add(1)
                .ok_or("memory generation exhausted")?
        || start + 1 != session.conversation.len()
        || producer.turn_id != session.current_turn_id
        || calls.len() != receipt.affected_count as usize
    {
        return Err("memory control does not match the active original owner batch");
    }
    let text = serde_json::to_string(receipt).map_err(|_| "invalid memory control receipt")?;
    let mut results = Vec::with_capacity(calls.len());
    for call in &calls {
        let mut result =
            ChatMessage::tool_result(result_id(session, receipt, &call.id), &call.id, &text)
                .with_turn_id(session.current_turn_id.as_ref().unwrap());
        result.tool_ok = Some(true);
        result.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
            producer.data_envelope.as_ref(),
            &call.id,
            &text,
            "memory_forget_receipt",
        )
        .map_err(|_| "invalid original memory control result label")?;
        results.push(result);
    }
    let committed = CommittedMemoryForgetGroup {
        assistant_message_id: producer.message_id.clone(),
        receipt: receipt.clone(),
    };
    let version = session
        .version
        .checked_add(1)
        .ok_or("session version exhausted")?;
    let data = session.assistant_data.as_mut().unwrap();
    data.control_generation = receipt.control_generation;
    data.memory_state_generation = receipt.memory_state_generation;
    // Current recall is prepared again; accepted conversation snapshots remain.
    data.current_memories.clear();
    data.read_uses.clear();
    data.history_dependencies.clear();
    session.conversation.extend(results);
    session.version = version;
    session.updated_at = now.into();
    Ok(committed)
}

/// Reconstruct the exact committed batch from its ordinary transcript results.
pub fn read_memory_forget_group(
    session: &PersistedAgentSession,
    producer_message_id: &str,
) -> Result<CommittedMemoryForgetGroup, &'static str> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or("missing assistant control state")?;
    let start = group_start(session, producer_message_id)?;
    let producer = &session.conversation[start];
    let calls = forget_calls(&session.ui_references, producer)?;
    let end = start
        .checked_add(calls.len() + 1)
        .ok_or("invalid control group length")?;
    let messages = session
        .conversation
        .get(start..end)
        .ok_or("incomplete memory control group")?;
    let receipt: AssistantDataControlReceipt = serde_json::from_str(&messages[1].text)
        .map_err(|_| "invalid committed memory control receipt")?;
    if receipt.reason != DataControlReason::MemoryForget
        || receipt.outcome != DataControlOutcome::Applied
        || receipt.control_generation == 0
        || receipt.control_generation > data.control_generation
        || receipt.memory_state_generation == 0
        || receipt.memory_state_generation > data.memory_state_generation
        || calls.len() != receipt.affected_count as usize
    {
        return Err("invalid committed memory control receipt");
    }
    let text = serde_json::to_string(&receipt).map_err(|_| "invalid control receipt")?;
    for (call, message) in calls.iter().zip(&messages[1..]) {
        if message.role != ChatRole::Tool
            || message.tool_call_id.as_deref() != Some(call.id.as_str())
            || message.message_id != result_id(session, &receipt, &call.id)
            || message.text != text
            || message.tool_ok != Some(true)
            || message.turn_id != producer.turn_id
            || session
                .conversation
                .iter()
                .filter(|message| message.tool_call_id.as_deref() == Some(call.id.as_str()))
                .count()
                != 1
        {
            return Err("invalid exact memory control result");
        }
    }
    Ok(CommittedMemoryForgetGroup {
        assistant_message_id: producer.message_id.clone(),
        receipt,
    })
}

fn group_start(session: &PersistedAgentSession, id: &str) -> Result<usize, &'static str> {
    let mut found = session
        .conversation
        .iter()
        .enumerate()
        .filter(|(_, message)| message.message_id == id);
    let (index, _) = found
        .next()
        .ok_or("missing original memory control producer")?;
    if found.next().is_some() {
        return Err("duplicate original memory control producer");
    }
    Ok(index)
}

fn forget_calls(
    references: &crate::ui_references::UiReferenceState,
    message: &ChatMessage,
) -> Result<Vec<ToolCall>, &'static str> {
    if message.role != ChatRole::Assistant || message.tool_calls.is_empty() {
        return Err("invalid memory control producer");
    }
    let calls = message
        .tool_calls
        .iter()
        .map(|call| ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        })
        .collect::<Vec<_>>();
    let mut ids = std::collections::BTreeSet::new();
    for call in &calls {
        desk_agent_protocol::ai_assistant::source::validate_source_id(&call.id)?;
        if !ids.insert(&call.id) {
            return Err("duplicate tool call in memory control batch");
        }
    }
    if super::super::data_tools::forget_batch(&calls, references)
        .map_err(|_| "invalid complete forget batch")?
        .is_none()
    {
        return Err("control group is not a memory forget batch");
    }
    Ok(calls)
}

fn result_id(
    session: &PersistedAgentSession,
    receipt: &AssistantDataControlReceipt,
    call: &str,
) -> String {
    native_ids::stable(
        Kind::Message,
        &(
            &session.conversation_id,
            &receipt.operation_id,
            call,
            "memory-forget-result",
        ),
    )
}

#[cfg(test)]
mod tests;
