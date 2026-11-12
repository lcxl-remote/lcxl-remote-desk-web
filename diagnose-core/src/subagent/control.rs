//! Adopt durable controls without granting a new owner input or resetting usage.

use super::{AgentRole, SubAgentState, creation::TaskCreationEnvelope, state::SubAgentRun};
use crate::session::{ChildRequestAdmission, PersistedAgentSession, TriggerOrigin, TurnState};
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
    next.close_awaiting_permission("not executed: the owner stopped generation before approval")
        .map_err(|_| "paused permission call is missing")?;
    close_unclosed_calls(
        &mut next,
        "tool result unavailable after the owner stopped generation; do not infer success, nonexecution, or permission to retry",
        "owner_stop_result_unavailable",
        "owner-stop",
    )?;
    next.main_stopped = true;
    next.main_coordination = None;
    next.coordinating_work = None;
    if next.has_admitted_personal_input() {
        for request in &mut next.permission_requests {
            if request.state.can_user_decide() {
                request.state = crate::dynamic_run::PermissionRequestState::Withdrawn;
            }
        }
    }
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

/// Answer every call the conversation left open with an unavailable result,
/// keyed by the session's control revision so a replay writes the same ids.
fn close_unclosed_calls(
    next: &mut PersistedAgentSession,
    content: &str,
    reason_code: &str,
    id_prefix: &str,
) -> Result<(), &'static str> {
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
            reason_code,
        )
        .map_err(|_| "invalid unavailable tool result label")?;
        let mut message = crate::chat::ChatMessage::tool_result(
            format!(
                "{id_prefix}-{:x}",
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
    Ok(())
}

/// The planning fence a session-level action fence names, in the bound
/// request's numbering. A Worker session counts revisions across requests.
pub fn request_fence(
    session: &PersistedAgentSession,
    fence: &crate::action_turn_fence::AssistantTurnFence,
) -> Option<super::state::PlanningFence> {
    Some(super::state::PlanningFence {
        input_revision: fence
            .input_revision
            .checked_sub(session.child_request_admission.input_offset)?,
        control_revision: fence
            .control_revision
            .checked_sub(session.child_request_admission.control_offset)?,
        source_epoch: fence.delegation.as_ref()?.source_epoch,
    })
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
        || held.targets != run.binding.targets
        || held.deadline_ms != run.binding.deadline_ms
        || held.input_revision > run.binding.input_revision
        || held.control_revision > run.binding.control_revision
        || held.source_epoch > run.binding.source_epoch
        || session.subagent_report_corrections_used > run.report_corrections_used
    {
        return Err("child control does not match its durable source");
    }
    let admission = session.child_request_admission;
    let input_revision = run
        .binding
        .input_revision
        .checked_add(admission.input_offset)
        .ok_or("child input revision exhausted")?;
    let control_revision = run
        .binding
        .control_revision
        .checked_add(admission.control_offset)
        .ok_or("child control revision exhausted")?;
    let input_changed = session.input_revision != input_revision;
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
        updated.begin_focus_epoch(input_revision, Vec::new())?;
        updated.latest_input_seq = updated
            .latest_input_seq
            .checked_add(1)
            .ok_or("child input sequence exhausted")?;
        updated.input_revision = input_revision;
        updated.bind_assistant_input_targets(&run.binding.targets)?;
    }
    updated.control_revision = control_revision;
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

/// Whether `run` is the request a Worker session currently works on. Other
/// requests of the same Worker are queued or already finished; their controls
/// change only their own request row.
pub fn is_bound(session: &PersistedAgentSession, run: &SubAgentRun) -> bool {
    session.conversation_id == run.child_conversation_id
        && session
            .agent_role
            .binding()
            .is_some_and(|binding| binding.task_id == run.binding.task_id)
}

/// A Worker session admits its next request the way a main conversation admits
/// a new owner input: once the previous request has finished and the session is
/// idle, the session takes a new input revision and focus epoch, and the new
/// request's source and instruction join the retained history. The host holds
/// the owner lock and writes the session with the same transaction.
pub fn admit_request(
    session: &mut PersistedAgentSession,
    previous: &SubAgentRun,
    next: &SubAgentRun,
    creation: &TaskCreationEnvelope,
    parent: &PersistedAgentSession,
    action_watermark: i64,
    now: &str,
) -> Result<(), &'static str> {
    next.validate()?;
    // The previous request's content may already have expired with its work;
    // only its identity and terminal state matter here.
    if !is_bound(session, previous)
        || session.actor_id != next.actor_id
        || session.device_id != next.device_id
        || !previous.state.is_terminal()
        || next.state != SubAgentState::Queued
        || next.worker_id != previous.worker_id
        || next.child_conversation_id != session.conversation_id
        || next.binding.root_conversation_id != parent.conversation_id
        || action_watermark < session.child_request_admission.action_watermark
    {
        return Err("worker request cannot be admitted to this session");
    }
    if !admission_ready(session) {
        return Err("worker session is still settling its previous request");
    }
    creation
        .validate_task(&next.binding)
        .map_err(|_| "worker request creation differs from its binding")?;
    let input_revision = session
        .input_revision
        .checked_add(1)
        .ok_or("child input revision exhausted")?
        .max(next.binding.input_revision);
    let control_revision = session
        .control_revision
        .checked_add(1)
        .ok_or("child control revision exhausted")?
        .max(next.binding.control_revision);
    let mut updated = session.clone();
    // The previous request's open calls never dispatched (the host found no
    // unresolved work); close them like a main turn superseded by new input.
    updated
        .close_awaiting_permission("not executed: the previous request ended before approval")
        .map_err(|_| "paused permission call is missing")?;
    // The finished request can no longer use its unanswered approvals.
    for request in &mut updated.permission_requests {
        if !request.state.is_terminal() {
            request.state = crate::dynamic_run::PermissionRequestState::Withdrawn;
        }
    }
    close_unclosed_calls(
        &mut updated,
        "not executed: the previous request ended before this call ran",
        "previous_request_ended",
        "request-ended",
    )?;
    updated.agent_role = AgentRole::SubAgent {
        binding: Box::new(next.binding.clone()),
    };
    updated.child_request_admission = ChildRequestAdmission {
        input_offset: input_revision - next.binding.input_revision,
        control_offset: control_revision - next.binding.control_revision,
        action_watermark,
    };
    updated.input_revision = input_revision;
    updated.control_revision = control_revision;
    updated.latest_input_seq = updated
        .latest_input_seq
        .checked_add(1)
        .ok_or("child input sequence exhausted")?;
    updated.delegation_group_id = Some(next.binding.group_id.clone());
    updated.delegated_owner_requirement = None;
    updated
        .bind_delegated_owner_requirement(&creation.source)
        .map_err(|_| "worker request source is invalid")?;
    updated.response_locale = creation.response_locale.clone();
    updated.inherit_direct_approval_origin(parent);
    updated.trigger_origin = TriggerOrigin::DelegatedTask;
    updated.chain_id = next.binding.task_id.clone();
    updated.automation_turns_used = 0;
    updated.subagent_report_corrections_used = next.report_corrections_used;
    updated.bind_assistant_input_targets(&next.binding.targets)?;
    updated.begin_focus_epoch(input_revision, Vec::new())?;
    updated.lease_token = updated
        .lease_token
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or("child lease exhausted")?;
    updated.conversation.push(
        creation
            .source
            .child_source_message(&next.binding.task_id)
            .map_err(|_| "worker request source is invalid")?,
    );
    updated.conversation.push(creation.instruction.clone());
    updated.finish_turn(TurnState::Idle, now);
    next.validate_session(&updated)?;
    *session = updated;
    Ok(())
}

/// The previous request left nothing running the next one could inherit: no
/// active turn, background action or pending result. As with a new owner input,
/// its unanswered approvals and open calls do not block: admission closes the
/// calls and withdraws the approvals, which belong to the finished request.
pub fn admission_ready(session: &PersistedAgentSession) -> bool {
    !session.turn_state.is_active()
        && session.execution_state == crate::session::ExecutionState::None
        && session.pending_auto_triggers.is_empty()
}
