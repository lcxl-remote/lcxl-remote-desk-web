//! Expire or cancel a fresh occurrence's owner-approval wait.
use super::{
    ScheduleStoreError,
    authority::authority_now,
    session::{
        ActionClass, ActionFilter, Finish, OrgMode, ScheduleHostStores, SessionGuard, SessionKey,
        SessionPatch,
    },
    settlement::{Settlement, settle_on},
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

/// Occurrences waiting on an owner decision, in id order.
pub async fn wait_expiry_candidates<C: ConnectionTrait>(
    db: &C,
    after: i64,
    limit: u64,
) -> Result<Vec<run::Model>, ScheduleStoreError> {
    Ok(run::Entity::find()
        .filter(run::Column::Id.gt(after))
        .filter(run::Column::Status.is_in(["awaiting_permission"]))
        .filter(run::Column::FailureAccounted.eq(false))
        .filter(run::Column::FinishedAt.is_null())
        .order_by_asc(run::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}

/// Ends a fresh occurrence's approval wait that was denied, cancelled or
/// timed out, and settles it.
pub async fn expire_fresh_approval_wait_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
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
    if task.kind != "fresh_task" || task.active_run_id.as_deref() != Some(run_id) {
        return Ok(Finish::Rollback(false));
    }
    // Same task write fence as execution; cleanup intentionally does not
    // require current device availability or a still-valid execution grant.
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
    if work.status != "awaiting_permission"
        || work.failure_accounted
        || work.finished_at.is_some()
        || work.lease_deadline.is_some()
        || work.conversation_id != work.run_id
    {
        return Ok(Finish::Rollback(false));
    }
    let snapshot: entity::Model =
        serde_json::from_str(&work.task_snapshot_json).map_err(|_| ScheduleStoreError::Invalid)?;
    if snapshot.kind != "fresh_task"
        || snapshot.schedule_id != task.schedule_id
        || snapshot.owner_user_id != work.owner_user_id
        || snapshot.target_device_id != task.target_device_id
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let policy: desk_agent_protocol::schedule::policy::ScheduleBudgetPolicy = serde_json::from_str(
        work.budget_policy_json
            .as_deref()
            .ok_or(ScheduleStoreError::Invalid)?,
    )
    .map_err(|_| ScheduleStoreError::Invalid)?;
    let deadline = work
        .started_at
        .ok_or(ScheduleStoreError::Invalid)?
        .checked_add(i64::from(policy.maximum.max_runtime_seconds) * 1000)
        .ok_or(ScheduleStoreError::Invalid)?;
    let now = authority_now(txn).await?;
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
    let mut session =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
    if row.actor_id != work.owner_user_id.to_string()
        || row.device_id != snapshot.target_device_id
        || session.actor_id != row.actor_id
        || session.device_id != row.device_id
        || session.conversation_id != work.run_id
        || session.version != row.version
        || i64::try_from(session.lease_token).ok() != Some(row.lease_token)
        || session.input_revision != 1
        || !row.personal
        || !row.stored_turn_state_is(desk_diagnose_core::session::TurnState::Idle)
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let denied = desk_diagnose_core::schedule::permission_wait::rejected(
        &session,
        work.result_ref
            .as_deref()
            .ok_or(ScheduleStoreError::Invalid)?,
    );

    let cancelled = work.cancel_requested_at.is_some();
    if !cancelled && !denied && now < deadline {
        return Ok(Finish::Rollback(false));
    }
    // Approval entry required every original action to be complete. Recheck
    // under the task fence: never classify an outstanding effect as timeout.
    if host
        .find_action(
            txn,
            run_id,
            ActionFilter {
                exclude: Some(ActionClass::Succeeded),
                ..ActionFilter::default()
            },
            true,
        )
        .await?
        .is_some()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let timestamp =
        chrono::DateTime::from_timestamp_millis(now).ok_or(ScheduleStoreError::Invalid)?;
    let reference = work
        .result_ref
        .as_deref()
        .ok_or(ScheduleStoreError::Invalid)?;
    if let Some(request_id) = reference.strip_prefix("permission:") {
        host.close_fresh_wait_decision(txn, &session, request_id, now)
            .await
            .map_err(|_| ScheduleStoreError::Conflict)?;
    }
    if cancelled {
        desk_diagnose_core::schedule::permission_wait::cancel(
            &mut session,
            reference,
            &timestamp.to_rfc3339(),
        )
    } else {
        desk_diagnose_core::schedule::permission_wait::expire(
            &mut session,
            reference,
            &timestamp.to_rfc3339(),
        )
    }
    .ok_or(ScheduleStoreError::Conflict)?;
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
                lease_deadline_ms: Some(None),
                updated_at_ms: Some(now),
                turn_state: Some(if cancelled { "cancelled" } else { "failed" }),
                ..Default::default()
            },
        )
        .await?;
    if changed != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let result_ref = work.result_ref.clone();
    settle_on(
        host,
        txn,
        Settlement {
            work,
            now,
            outcome: if cancelled {
                desk_agent_protocol::schedule::ScheduledRunStatus::Cancelled
            } else {
                desk_agent_protocol::schedule::ScheduledRunStatus::Failed
            },
            offline_timeout: false,
            error_kind: Some(
                if cancelled {
                    "cancelled"
                } else if denied {
                    "approval_denied"
                } else {
                    "approval_timeout"
                }
                .into(),
            ),
            result_ref,
        },
    )
    .await?;
    Ok(Finish::Commit(true))
}
