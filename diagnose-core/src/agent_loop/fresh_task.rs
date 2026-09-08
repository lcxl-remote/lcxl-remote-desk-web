//! Drive only the pristine context returned by atomic fresh-task admission.
use super::*;
use crate::schedule::{
    contract::ValidatedTaskContract,
    fresh_session::{FreshSessionInput, initial_session},
};

/// The runtime must bind both leases and enforce current task/model/tool
/// authority in its seams. This entry does not claim again, append another
/// requirement, or turn the task origin into a user approval.
pub async fn resume_claimed_fresh_task_turn(
    deps: &LoopDeps<'_>,
    mut session: PersistedAgentSession,
    contract: &ValidatedTaskContract,
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
    if session.version != 1 || deps.heartbeat.is_none() || session.conversation.len() != 1 {
        return Err(denied());
    }
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
    expected.version = 1;
    if session != expected {
        return Err(denied());
    }
    let policy = deps.model.model_egress_policy()?.ok_or_else(denied)?;
    let turn_id = session.current_turn_id.clone().ok_or_else(denied)?;
    let original = &session.conversation[0];
    session.conversation[0] = crate::model_message_labels::model_bound_user_message(
        original.message_id.clone(),
        original.text.clone(),
        policy.destination,
    )?
    .with_turn_id(turn_id.clone());
    drive_claimed(deps, session, turn_id, None, sink).await
}

/// Resume the existing task context after the central store atomically consumed
/// one owner decision and renewed both leases. This does not create user input,
/// copy rehearsal state, replenish budgets, or grant a tool permission.
pub async fn resume_claimed_fresh_task_permission_turn(
    deps: &LoopDeps<'_>,
    session: PersistedAgentSession,
    contract: &ValidatedTaskContract,
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
        || session.surface != AgentSessionSurface::DeviceAssistant
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
