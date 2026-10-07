//! Drive only the pristine context returned by atomic fresh-task admission.
use super::*;
use crate::schedule::{
    contract::ValidatedTaskContract,
    fresh_session::{FreshSessionInput, initial_session},
};
use crate::subagent::creation::CreationEnvelope;

/// Frozen publication evidence proves the context's origin. Each physical seam
/// must still fence current publication, source state and its own planner lease.
fn validate_source_context(
    session: &PersistedAgentSession,
    contract: &ValidatedTaskContract,
    source: &CreationEnvelope,
    destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
) -> Result<(), AgentError> {
    source.validate()?;
    let published = source
        .scheduled_source
        .as_ref()
        .ok_or_else(|| crate::subagent::invalid("published source is missing"))?;
    if published.validate()?.canonical_json() != contract.canonical_json()
        || source.root_conversation_id != session.conversation_id
        || source.actor_id != session.actor_id
        || source.device_id != session.device_id
        || source.parent_input_revision != session.input_revision
        || source.parent_control_revision > session.control_revision
        || source.model_destination != *destination
        || session.delegation_group_id.as_deref()
            != Some(format!("dg-{}", source.source_key()?).as_str())
        || session.conversation.first() != Some(&source.owner_requirement)
    {
        return Err(crate::subagent::invalid("published source context changed"));
    }
    Ok(())
}

/// The runtime must bind both leases and enforce current task/model/tool
/// authority in its seams. This entry does not claim again, append another
/// requirement, or turn the task origin into a user approval.
pub async fn resume_claimed_fresh_task_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    contract: &ValidatedTaskContract,
    source: &CreationEnvelope,
    run_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let denied = || AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "Fresh task requires its original isolated claim and current model authority."
            .into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    };
    if session.version != 2 || deps.heartbeat.is_none() || session.conversation.len() != 1 {
        return Err(denied());
    }
    let policy = deps.model.model_egress_policy()?.ok_or_else(denied)?;
    validate_source_context(&session, contract, source, &policy.destination)?;
    let mut expected = initial_session(
        contract,
        FreshSessionInput {
            run_id,
            actor_id: &session.actor_id,
            prompt: &session.conversation[0].text,
            locale: session.response_locale.as_deref(),
            policy_revision: session.policy_revision,
            scope: session.scope_snapshot.clone(),
            now: &session.created_at,
        },
    )
    .map_err(|_| denied())?;
    expected.version = 2;
    expected.updated_at = session.updated_at.clone();
    expected.delegation_group_id = session.delegation_group_id.clone();
    expected.conversation[0] = source.owner_requirement.clone();
    if session != expected {
        return Err(denied());
    }
    let turn_id = session.current_turn_id.clone().ok_or_else(denied)?;
    drive_claimed(deps, session, turn_id, None, sink).await
}

/// Resume the existing task context after the central store atomically consumed
/// one owner decision and renewed both leases. This does not create user input,
/// copy rehearsal state, replenish budgets, or grant a tool permission.
pub async fn resume_claimed_fresh_task_permission_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    contract: &ValidatedTaskContract,
    source: &CreationEnvelope,
    run_id: &str,
    approval_reference: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    use crate::session::{ExecutionState, TriggerOrigin};
    use sha2::{Digest, Sha256};
    let denied = || AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "Task approval requires its original context and a current isolated claim.".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    };
    let now = chrono::DateTime::parse_from_rfc3339(&(deps.clock)())
        .ok()
        .and_then(|value| u64::try_from(value.timestamp_millis()).ok())
        .ok_or_else(denied)?;
    let original = session.conversation.first().ok_or_else(denied)?;
    if deps.heartbeat.is_none()
        || session.version <= 1
        || session.lease_token <= 1
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || !session.agent_role.is_main()
        || session.surface != AgentSessionSurface::AiAssistant
        || session.turn_state != TurnState::Running
        || session.execution_state != ExecutionState::None
        || session.conversation_id != run_id
        || session.current_request_id.as_deref() != Some(run_id)
        || session.device_id != contract.contract().target_device_id
        || session.input_revision != 1
        || session.latest_input_seq != 1
        || session.active_control_connection_id.is_some()
        || session.terminal_error.is_some()
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || original.role != ChatRole::User
        || original.message_id != format!("{run_id}:input")
        || format!("{:x}", Sha256::digest(original.text.as_bytes()))
            != contract.contract().prompt_sha256
        || session.current_turn_id.as_deref() != Some(format!("{run_id}-turn").as_str())
        || !crate::schedule::permission_wait::approved(&session, approval_reference, now)
    {
        return Err(denied());
    }
    let policy = deps.model.model_egress_policy()?.ok_or_else(denied)?;
    validate_source_context(&session, contract, source, &policy.destination)?;
    crate::schedule::published_input::validate_published_input(
        original,
        run_id,
        contract,
        &policy.destination,
    )?;
    let turn_id = session.current_turn_id.clone().ok_or_else(denied)?;
    let bridge_id = format!(
        "task-approval-{:x}",
        Sha256::digest(format!("{run_id}:{approval_reference}").as_bytes())
    );
    if session
        .conversation
        .iter()
        .any(|message| message.message_id == bridge_id)
    {
        return Err(denied());
    }
    let bridge = crate::permission_resume::authorized_permission_resume_message(
        bridge_id, &policy, original,
    )?;
    drive_claimed(deps, session, turn_id, Some(bridge), sink).await
}

/// Continue the same published occurrence after a durable child wait resolved.
/// This adds no user input, resets no counters and creates no task authorization.
pub async fn resume_claimed_fresh_task_children_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    contract: &ValidatedTaskContract,
    source: &CreationEnvelope,
    run_id: &str,
    wait_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let denied = || {
        crate::subagent::invalid("published child dependency requires its original paired claim")
    };
    let policy = deps.model.model_egress_policy()?.ok_or_else(denied)?;
    validate_source_context(&session, contract, source, &policy.destination)?;
    let wait = session.ready_subagent_wait.as_ref().ok_or_else(denied)?;
    wait.validate().map_err(crate::subagent::invalid)?;
    if deps.heartbeat.is_none()
        || deps.session_seam.subagents(&session).is_none()
        || !session.agent_role.is_main()
        || session.surface != AgentSessionSurface::AiAssistant
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.turn_state != TurnState::Running
        || session.main_stopped
        || session.lease_token <= 1
        || session.version <= 2
        || session.conversation_id != run_id
        || session.input_revision != 1
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(format!("{run_id}-turn").as_str())
        || wait.wait_id != wait_id
        || wait.parent_input_revision != session.input_revision
        || wait.parent_control_revision != session.control_revision
        || session.delegation_group_id.as_deref() != Some(wait.group_id.as_str())
        || session.subagent_wait.is_some()
        || session.terminal_error.is_some()
        || session.terminal_permission_request_id.is_some()
        || !session.unclosed_tool_call_ids().is_empty()
        || !session.execution_state.states().is_empty()
        || session
            .permission_requests
            .iter()
            .any(|request| !request.state.is_terminal())
    {
        return Err(denied());
    }
    let turn_id = session.current_turn_id.clone().ok_or_else(denied)?;
    drive_claimed(deps, session, turn_id, None, sink).await
}

/// Interpret a genuine child status event using the original published source.
/// The notification restricts the tool catalog and final dispatch to existing
/// dependency facts; no user input, clock, counter or task quota is restarted.
pub async fn resume_claimed_fresh_task_notification_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    contract: &ValidatedTaskContract,
    source: &CreationEnvelope,
    run_id: &str,
    message_id: &str,
    sink: &mut dyn TurnSink,
) -> Result<LoopOutcome, AgentError> {
    let denied = || {
        crate::subagent::invalid("published child notification requires its original paired claim")
    };
    let policy = deps.model.model_egress_policy()?.ok_or_else(denied)?;
    validate_source_context(&session, contract, source, &policy.destination)?;
    let notice = session
        .ready_subagent_notification
        .as_ref()
        .ok_or_else(denied)?;
    notice.validate().map_err(crate::subagent::invalid)?;
    if deps.heartbeat.is_none()
        || deps.session_seam.subagents(&session).is_none()
        || session.surface != AgentSessionSurface::AiAssistant
        || !session.is_subagent_result_turn()
        || session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.turn_state != TurnState::Running
        || session.main_stopped
        || session.lease_token <= 1
        || session.version <= 2
        || session.conversation_id != run_id
        || session.input_revision != 1
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(format!("{run_id}-turn").as_str())
        || notice.message_id != message_id
        || notice.parent_input_revision != session.input_revision
        || notice.parent_control_revision != session.control_revision
        || notice.accepted_response_message_id.is_some()
        || session.delegation_group_id.as_deref() != Some(notice.group_id.as_str())
        || session.subagent_wait.is_some()
        || session.ready_subagent_wait.is_some()
        || session.terminal_error.is_some()
        || session.terminal_permission_request_id.is_some()
        || !session.unclosed_tool_call_ids().is_empty()
        || !session.execution_state.states().is_empty()
        || session
            .permission_requests
            .iter()
            .any(|request| !request.state.is_terminal())
    {
        return Err(denied());
    }
    let turn_id = session.current_turn_id.clone().ok_or_else(denied)?;
    drive_claimed(deps, session, turn_id, None, sink).await
}
