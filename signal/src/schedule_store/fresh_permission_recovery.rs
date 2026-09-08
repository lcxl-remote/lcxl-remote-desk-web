//! Recover a committed task permission pause without restarting the model.
use super::{ScheduleStoreError, entity};
use crate::entity::{agent_action_item as work_item, agent_schedule_run as run, agent_session};
use desk_diagnose_core::session::{ExecutionState, PersistedAgentSession, TurnState};
use sea_orm::{ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Set};

/// Caller has locked the original task and session and checked subject, run and
/// expired execution lease. Ownership of the transaction prevents a partial wait.
pub(super) async fn recover(
    txn: DatabaseTransaction,
    work: run::Model,
    row: agent_session::Model,
    mut session: PersistedAgentSession,
    now: i64,
) -> Result<bool, ScheduleStoreError> {
    let unfinished = desk_diagnose_core::schedule::permission_wait::unfinished_pause(&session);
    if (session.turn_state != TurnState::Idle && unfinished.is_none())
        || session.execution_state != ExecutionState::None
        || session.terminal_error.is_some()
        || (unfinished.is_none() && session.handled_input_seq != session.latest_input_seq)
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || row
            .lease_deadline
            .is_some_and(|deadline| deadline.timestamp_millis() > now)
    {
        return Ok(false);
    }
    if unfinished.is_some() && row.lease_deadline.is_none() {
        return Ok(false);
    }
    let request_id = session
        .terminal_permission_request_id
        .clone()
        .or_else(|| unfinished.clone())
        .ok_or(ScheduleStoreError::Conflict)?;
    let reference = desk_diagnose_core::schedule::permission_wait::reference(&session, &request_id)
        .ok_or(ScheduleStoreError::Conflict)?;
    let snapshot: entity::Model =
        serde_json::from_str(&work.task_snapshot_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if snapshot.kind != "fresh_task"
        || snapshot.schedule_id != work.schedule_id
        || snapshot.owner_user_id != work.owner_user_id
        || snapshot.target_device_id != session.device_id
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let (_, contract) = super::publication::load_contract(
        &txn,
        work.owner_user_id,
        &work.schedule_id,
        snapshot
            .contract_revision
            .ok_or(ScheduleStoreError::Invalid)?,
    )
    .await?;
    if contract.contract().exception_mode
        != desk_agent_protocol::schedule::contract::TaskExceptionMode::RequestApproval
        || i64::try_from(contract.contract().task_revision).ok() != Some(snapshot.task_revision)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    if work_item::Entity::find()
        .filter(work_item::Column::ConversationId.eq(&work.run_id))
        .filter(
            work_item::Column::Status.ne(crate::capability_grant_store::CAPABILITY_WORK_SUCCEEDED),
        )
        .lock_exclusive()
        .one(&txn)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    if reference.starts_with("permission:") {
        crate::agent_session_store::permission_resume::verify_fresh_wait_request_on(
            &txn,
            &session,
            &request_id,
        )
        .await
        .map_err(|_| ScheduleStoreError::Conflict)?;
    } else {
        super::directory_receipt::verify(&txn, &session, &request_id).await?;
    }
    // The same decision must not be recovered as a second pause segment.
    if work.result_ref.as_deref() == Some(reference.as_str()) {
        return Err(ScheduleStoreError::Conflict);
    }
    if let Some(previous) = work
        .result_ref
        .as_deref()
        .and_then(|reference| reference.strip_prefix("permission:"))
    {
        if previous == request_id {
            return Err(ScheduleStoreError::Conflict);
        }
        let timestamp =
            chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
        crate::agent_session_store::permission_resume::settle_fresh_decision_on(
            &txn, &session, previous, timestamp,
        )
        .await
        .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    session.version = session
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.lease_token = session
        .lease_token
        .checked_add(1)
        .filter(|token| *token <= i64::MAX as u64)
        .ok_or(ScheduleStoreError::Invalid)?;
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if unfinished.is_some() {
        session.finish_turn(TurnState::Idle, timestamp.to_rfc3339());
        session.handled_input_seq = session.latest_input_seq;
        session.terminal_permission_request_id = Some(request_id);
    }
    let changed = agent_session::Entity::update_many()
        .set(agent_session::ActiveModel {
            state_json: Set(session
                .encode_json_for_storage()
                .map_err(|_| ScheduleStoreError::Invalid)?),
            version: Set(session.version),
            lease_token: Set(session.lease_token as i64),
            lease_deadline: Set(None),
            updated_at: Set(timestamp),
            ..Default::default()
        })
        .filter(agent_session::Column::Id.eq(row.id))
        .filter(agent_session::Column::Version.eq(row.version))
        .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set("awaiting_permission".into()),
            result_ref: Set(Some(reference)),
            lease_deadline: Set(None),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::Status.eq("running"))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .filter(run::Column::FailureAccounted.eq(false))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    // Keep started_at and cancellation intent. The wait scanner applies an
    // already elapsed deadline, rejection or cancellation without dispatch.
    txn.commit().await?;
    Ok(true)
}
