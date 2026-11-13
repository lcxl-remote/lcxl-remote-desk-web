//! Recover a committed task permission pause without restarting the model.
use super::super::{
    ScheduleStoreError,
    session::{
        ActionClass, ActionFilter, Finish, ScheduleHostStores, SessionGuard, SessionPatch,
        SessionRow,
    },
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_diagnose_core::session::{ExecutionState, PersistedAgentSession, TurnState};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

/// Caller has locked the original task and session and checked subject, run and
/// expired execution lease. Ownership of the transaction prevents a partial wait.
pub(in crate::schedule) async fn recover<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    work: run::Model,
    row: SessionRow<H::Row>,
    mut session: PersistedAgentSession,
    now: i64,
) -> Result<Finish<bool>, ScheduleStoreError> {
    let unfinished = desk_diagnose_core::schedule::permission_wait::unfinished_pause(&session);
    if (session.turn_state != TurnState::Idle && unfinished.is_none())
        || session.execution_state != ExecutionState::None
        || session.terminal_error.is_some()
        || (unfinished.is_none() && session.handled_input_seq != session.latest_input_seq)
        || !session.pending_auto_triggers.is_empty()
        || !session.unclosed_tool_call_ids().is_empty()
        || row.lease_deadline_ms.is_some_and(|deadline| deadline > now)
    {
        return Ok(Finish::Rollback(false));
    }
    if unfinished.is_some() && row.lease_deadline_ms.is_none() {
        return Ok(Finish::Rollback(false));
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
    if host
        .find_action(
            txn,
            &work.run_id,
            ActionFilter {
                exclude: Some(ActionClass::Succeeded),
                ..ActionFilter::default()
            },
            true,
        )
        .await?
        .is_some()
    {
        return Ok(Finish::Rollback(false));
    }
    if reference.starts_with("permission:") {
        host.verify_fresh_wait_request(txn, &session, &request_id)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
    } else {
        super::super::verify_directory_receipt_on(host, txn, &session, &request_id).await?;
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
        host.settle_fresh_decision(txn, &session, previous, now)
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
    let state_json = session
        .encode_json_for_storage()
        .map_err(|_| ScheduleStoreError::Invalid)?;
    let changed = host
        .write_session(
            txn,
            SessionGuard {
                id: Some(row.id),
                version: Some(row.version),
                lease_token: Some(row.lease_token),
                ..SessionGuard::any()
            },
            SessionPatch {
                state_json: Some(&state_json),
                version: Some(session.version),
                lease_token: Some(session.lease_token as i64),
                lease_deadline_ms: Some(None),
                updated_at_ms: Some(now),
                turn_state: Some(TurnState::Idle.as_str()),
            },
        )
        .await?;
    if changed != 1 {
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
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    // Keep started_at and cancellation intent. The wait scanner applies an
    // already elapsed deadline, rejection or cancellation without dispatch.
    Ok(Finish::Commit(true))
}
