//! Reconcile receipts that arrived after a fresh occurrence settled as unknown.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    session::{
        ActionClass, ActionFilter, Finish, OrgMode, ScheduleHostStores, SessionGuard, SessionKey,
        SessionPatch,
    },
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_diagnose_core::session::{
    ExecutionState, PersistedAgentSession, TriggerOrigin, TurnState,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};

/// Outcome-unknown occurrences whose late receipts are not reconciled yet.
pub async fn late_receipt_candidates<C: ConnectionTrait>(
    db: &C,
    after: i64,
    limit: u64,
) -> Result<Vec<run::Model>, ScheduleStoreError> {
    Ok(run::Entity::find()
        .filter(run::Column::Id.gt(after))
        .filter(run::Column::Status.eq("outcome_unknown"))
        .filter(run::Column::FailureAccounted.eq(true))
        .filter(run::Column::FinishedAt.is_not_null())
        .filter(run::Column::ReceiptsReconciledAt.is_null())
        .order_by_asc(run::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// Reconciles late receipts in the caller's transaction after the host has
/// handled child waits.
pub async fn reconcile_late_task_receipts_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    run_id: &str,
) -> Result<Finish<bool>, ScheduleStoreError> {
    host.lock_conversation(txn, run_id).await?;

    let initial = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&initial.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(initial.owner_user_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if task.kind != "fresh_task" {
        return Ok(Finish::Rollback(false));
    }
    let locked = entity::Entity::update_many()
        .col_expr(
            entity::Column::Revision,
            sea_orm::sea_query::Expr::col(entity::Column::Revision),
        )
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(task.revision))
        .exec(txn)
        .await?;
    if locked.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let work = run::Entity::find_by_id(initial.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if work.status != "outcome_unknown"
        || !work.failure_accounted
        || work.finished_at.is_none()
        || work.receipts_reconciled_at.is_some()
        || work.lease_deadline.is_some()
        || work.conversation_id != work.run_id
    {
        return Ok(Finish::Rollback(false));
    }
    let row = host
        .find_session(
            txn,
            SessionKey::Conversation {
                conversation: run_id,
                actor: None,
                device: None,
            },
            OrgMode::Any,
            true,
        )
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json)
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if session.trigger_origin != TriggerOrigin::ScheduledTask
        || session.turn_state != TurnState::Failed
        || session.actor_id != work.owner_user_id.to_string()
        || session.actor_id != row.actor_id
        || session.device_id != task.target_device_id
        || session.device_id != row.device_id
        || session.conversation_id != work.run_id
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
        || row.lease_deadline_ms.is_some()
        || !row.personal
        || session.current_request_id.as_deref() != Some(run_id)
        || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
        || session.input_revision != 1
        || !session.pending_auto_triggers.is_empty()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    if session.execution_state.unknown().is_none() {
        return Ok(Finish::Rollback(false));
    }
    let actions = host
        .find_actions(txn, run_id, ActionFilter::default(), true)
        .await?;
    if actions.is_empty()
        || actions
            .iter()
            .any(|action| !matches!(action.class, ActionClass::Succeeded | ActionClass::Failed))
    {
        return Ok(Finish::Rollback(false));
    }
    let now = authority_now(txn).await?;
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    if !(host
        .restore_completed_calls(txn, &work, &mut session, now)
        .await?)
        || session.execution_state != ExecutionState::None
        || !session.unclosed_tool_call_ids().is_empty()
    {
        return Ok(Finish::Rollback(false));
    }
    session.version = session
        .version
        .checked_add(1)
        .ok_or(ScheduleStoreError::Invalid)?;
    session.updated_at = timestamp.to_rfc3339();
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
                updated_at_ms: Some(now),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            receipts_reconciled_at: Set(Some(now)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::Status.eq("outcome_unknown"))
        .filter(run::Column::ReceiptsReconciledAt.is_null())
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(Finish::Commit(true))
}
