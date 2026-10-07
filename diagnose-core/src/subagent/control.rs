//! Adopt durable controls without granting a new owner input or resetting usage.

use super::{AgentRole, SubAgentState, state::SubAgentRun};
use crate::session::{PersistedAgentSession, TurnState};
use sha2::{Digest, Sha256};

/// Fence main generation while preserving execution identities for immutable
/// receipts. Closing a missing result says only that it is unavailable.
pub fn stop_main_session(
    session: &mut PersistedAgentSession,
    now: &str,
) -> Result<(), &'static str> {
    if !session.agent_role.is_main()
        || session.surface != crate::session::AgentSessionSurface::AiAssistant
    {
        return Err("main stop requires an owner assistant session");
    }
    let mut next = session.clone();
    next.control_revision = next
        .control_revision
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or("main control revision exhausted")?;
    next.lease_token = next
        .lease_token
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or("main lease exhausted")?;
    let content = "tool result unavailable after the owner stopped generation; do not infer success, nonexecution, or permission to retry";
    for call_id in next.unclosed_tool_call_ids() {
        let label = next
            .conversation
            .iter()
            .find(|message| {
                message.role == crate::chat::ChatRole::Assistant
                    && message.tool_calls.iter().any(|call| call.id == call_id)
            })
            .and_then(|message| message.data_envelope.as_ref());
        let envelope = crate::model_message_labels::internal_tool_result_envelope(
            label,
            &call_id,
            content,
            "owner_stop_result_unavailable",
        )
        .map_err(|_| "invalid stopped tool result label")?;
        let mut message = crate::chat::ChatMessage::tool_result(
            format!(
                "owner-stop-{:x}",
                Sha256::digest(
                    format!(
                        "{}:{}:{call_id}",
                        next.conversation_id, next.control_revision
                    )
                    .as_bytes()
                )
            ),
            &call_id,
            content,
        );
        message.turn_id = next.current_turn_id.clone();
        message.data_envelope = envelope;
        next.conversation.push(message);
    }
    next.main_stopped = true;
    next.subagent_wait = None;
    next.ready_subagent_wait = None;
    next.ready_subagent_notification = None;
    next.pending_auto_triggers.clear();
    next.observed_subagent_results.clear();
    next.accepted_subagent_observations.clear();
    next.interpreted_subagent_results.clear();
    next.finish_turn(TurnState::Cancelled, now);
    *session = next;
    Ok(())
}

/// The host holds source/task/session control and writes the task and session in
/// one transaction. Unclosed calls and execution identities remain available to
/// the existing receipt reconciliation before the next planning claim.
pub fn synchronize_session(
    session: &mut PersistedAgentSession,
    run: &SubAgentRun,
    now: &str,
) -> Result<(), &'static str> {
    run.validate()?;
    let held = session
        .agent_role
        .binding()
        .ok_or("task control requires a child session")?;
    if session.conversation_id != run.child_conversation_id
        || session.actor_id != run.actor_id
        || session.device_id != run.device_id
        || held.root_conversation_id != run.binding.root_conversation_id
        || held.group_id != run.binding.group_id
        || held.task_id != run.binding.task_id
        || held.source != run.binding.source
        || held.deadline_ms != run.binding.deadline_ms
        || held.input_revision > run.binding.input_revision
        || held.control_revision > run.binding.control_revision
        || held.source_epoch > run.binding.source_epoch
        || session.subagent_report_corrections_used > run.report_corrections_used
    {
        return Err("child control does not match its durable source");
    }
    let input_changed = session.input_revision != run.binding.input_revision;
    let lease = session
        .lease_token
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or("child lease exhausted")?;
    let mut updated = session.clone();
    updated.agent_role = AgentRole::SubAgent {
        binding: Box::new(run.binding.clone()),
    };
    if input_changed {
        updated.begin_focus_epoch(run.binding.input_revision, Vec::new())?;
        updated.latest_input_seq = updated
            .latest_input_seq
            .checked_add(1)
            .ok_or("child input sequence exhausted")?;
        updated.input_revision = run.binding.input_revision;
    }
    updated.control_revision = run.binding.control_revision;
    updated.subagent_report_corrections_used = run.report_corrections_used;
    updated.lease_token = lease;
    updated.finish_turn(
        match run.state {
            SubAgentState::Cancelled | SubAgentState::Cancelling => TurnState::Cancelled,
            SubAgentState::Failed => TurnState::Failed,
            _ => TurnState::Idle,
        },
        now,
    );
    if run.state.is_terminal() || run.state == SubAgentState::Cancelling {
        updated.pending_auto_triggers.clear();
    }
    *session = updated;
    Ok(())
}
