//! Waiting-task permission inspection. This cannot claim a model or issue a task grant.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::{agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{
    ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter, QuerySelect};

/// Review-only context; no task execution authority can be obtained from it.
pub(crate) struct TaskApprovalReview {
    contract: desk_diagnose_core::schedule::contract::ValidatedTaskContract,
    provider_device_id: String,
    pub(super) valid_until: u64,
}
impl TaskApprovalReview {
    pub(crate) fn constrain(
        &self,
        session: &PersistedAgentSession,
        request: &desk_diagnose_core::dynamic_run::PermissionRequest,
        grants: &mut [desk_agent_protocol::capability_grant::CapabilityGrant],
    ) -> Result<(), DbErr> {
        desk_diagnose_core::schedule::contract::exception::constrain_approval(
            &self.contract,
            session,
            request,
            &self.provider_device_id,
            self.valid_until,
            grants,
        )
        .map_err(|_| invalid())
    }
}

/// Called before locking the permission/session row, after the owner's ordinary
/// authorization. An approval and task cancellation share the task-first fence.
/// Returns frozen review constraints for this decision, not execution authority.
pub(crate) async fn lock_fresh_approval_on(
    txn: &DatabaseTransaction,
    conversation: &str,
    request_id: &str,
) -> Result<Option<TaskApprovalReview>, DbErr> {
    let Some(peek) = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation))
        .one(txn)
        .await?
    else {
        return Ok(None);
    };
    let initial = PersistedAgentSession::decode_json(&peek.state_json).map_err(|_| invalid())?;
    if initial.trigger_origin != TriggerOrigin::ScheduledTask {
        return Ok(None);
    }
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(conversation))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let owner = initial.actor_id.parse::<i32>().map_err(|_| invalid())?;
    let node = work.lease_owner.as_deref().ok_or_else(invalid)?;
    let authority = ScheduleStore::lock_waiting_approval(
        txn,
        owner,
        &initial.device_id,
        conversation,
        node,
        work.lease_epoch,
    )
    .await
    .map_err(storage)?;
    if authority.contract.contract().exception_mode
        != desk_agent_protocol::schedule::contract::TaskExceptionMode::RequestApproval
    {
        return Err(invalid());
    }
    let row = agent_session::Entity::find_by_id(peek.id)
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if session != initial
        || session.version != row.version
        || session.actor_id != row.actor_id
        || session.device_id != row.device_id
        || session.conversation_id != conversation
        || session.input_revision != 1
        || session.current_request_id.as_deref() != Some(conversation)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.turn_state != TurnState::Idle
        || session.execution_state != ExecutionState::None
        || session.terminal_error.is_some()
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || session.handled_input_seq != session.latest_input_seq
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
        || desk_diagnose_core::schedule::permission_wait::reference(&session, request_id).as_deref()
            != work.result_ref.as_deref()
    {
        return Err(invalid());
    }
    // The session lock may have waited. Recheck the parent's expiry and revisions
    // using the database wall clock before the caller commits the owner decision.
    let current = ScheduleStore::lock_waiting_approval(
        txn,
        owner,
        &session.device_id,
        conversation,
        node,
        work.lease_epoch,
    )
    .await
    .map_err(storage)?;
    if current.verified_at < authority.verified_at {
        return Err(invalid());
    }
    let provider_device_id = session.device_id.clone();
    Ok(Some(TaskApprovalReview {
        contract: current.contract,
        provider_device_id,
        valid_until: u64::try_from(current.valid_until).map_err(|_| invalid())?,
    }))
}
fn invalid() -> DbErr {
    DbErr::Custom("scheduled approval is no longer current".into())
}
fn storage(error: ScheduleStoreError) -> DbErr {
    match error {
        ScheduleStoreError::Backend(error) => error,
        _ => invalid(),
    }
}
