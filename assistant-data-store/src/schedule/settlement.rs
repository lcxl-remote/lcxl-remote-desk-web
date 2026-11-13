//! Fenced finalization atomically settles the active slot and failure policy.
use super::{
    ScheduleStoreError, json,
    queue::database_now,
    session::{Finish, ScheduleHostStores},
};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use desk_agent_protocol::schedule::ScheduledRunStatus;
use desk_diagnose_core::schedule::lifecycle::FailureState;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, Set};

/// Unstarted work can expire safely; leased work must first be reconciled.
pub async fn expire_pending_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    run_id: &str,
) -> Result<Finish<run::Model>, ScheduleStoreError> {
    let now = database_now(txn).await?;
    let work = run::Entity::find()
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if work.failure_accounted && work.status == "missed" {
        return Ok(Finish::Rollback(work));
    }
    if !matches!(work.status.as_str(), "queued" | "waiting_device")
        || work.failure_accounted
        || work.start_deadline > now
        || work.started_at.is_some()
        || work.lease_owner.is_some()
        || work.lease_deadline.is_some()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    let goal_conflict = host
        .active_goal_conflict(txn, &work.conversation_id, work.owner_user_id, None)
        .await?;
    let offline = work.status == "waiting_device" && !goal_conflict;
    let error = if goal_conflict {
        "active_goal_conflict"
    } else if offline {
        "device_offline_timeout"
    } else {
        "queue_timeout"
    };
    settle_on(
        host,
        txn,
        Settlement {
            work,
            now,
            outcome: ScheduledRunStatus::Missed,
            offline_timeout: offline,
            error_kind: Some(error.into()),
            result_ref: None,
        },
    )
    .await
    .map(Finish::Commit)
}

/// The terminal accounting of one occurrence.
pub struct Settlement {
    pub work: run::Model,
    pub now: i64,
    pub outcome: ScheduledRunStatus,
    pub offline_timeout: bool,
    pub error_kind: Option<String>,
    pub result_ref: Option<String>,
}

/// Settles the occurrence and its task in the caller's transaction; the
/// caller commits on success. Native history is captured last, after every
/// original mutation.
pub async fn settle_on<C: ConnectionTrait, H: ScheduleHostStores<C>>(
    host: &H,
    txn: &C,
    args: Settlement,
) -> Result<run::Model, ScheduleStoreError> {
    let Settlement {
        work,
        now,
        outcome,
        offline_timeout,
        error_kind,
        result_ref,
    } = args;
    let run_id = work.run_id.as_str();
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    let actor = work.owner_user_id.to_string();
    let device = task.target_device_id.to_string();
    let history = desk_diagnose_core::future::boxed(|| {
        host.open_native_scope(txn, &actor, &device, &work.conversation_id)
    })
    .await?;
    if task.active_run_id.as_deref() != Some(run_id) {
        return Err(ScheduleStoreError::Conflict);
    }
    let mut failures: FailureState =
        serde_json::from_str(&task.failure_state_json).map_err(|_| ScheduleStoreError::Invalid)?;
    let applied = failures
        .settle(
            u64::try_from(work.recovery_epoch).map_err(|_| ScheduleStoreError::Invalid)?,
            outcome,
            offline_timeout,
        )
        .map_err(|_| ScheduleStoreError::Invalid)?;
    if !applied {
        return Err(ScheduleStoreError::Conflict);
    }
    let paused = !failures.pause_reasons.is_empty();
    let task_status = desk_diagnose_core::schedule::lifecycle::settled_task_status(
        &task.status,
        paused,
        task.next_run_at.is_some(),
    );
    let changed = entity::Entity::update_many()
        .set(entity::ActiveModel {
            status: Set(task_status.into()),
            failure_state_json: Set(json(&failures)?),
            active_run_id: Set(None),
            next_run_at: Set(if paused { None } else { task.next_run_at }),
            revision: Set(task
                .revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(task.revision))
        .filter(entity::Column::ActiveRunId.eq(run_id))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            status: Set(json(&outcome)?.trim_matches('"').into()),
            failure_accounted: Set(true),
            error_kind: Set(error_kind),
            result_ref: Set(result_ref),
            lease_deadline: Set(None),
            finished_at: Set(Some(now)),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(run::Column::Id.eq(work.id))
        .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
        .filter(run::Column::Status.eq(&work.status))
        .filter(run::Column::FailureAccounted.eq(false))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let result = run::Entity::find_by_id(work.id)
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if let Some(history) = &history {
        desk_diagnose_core::future::boxed(|| host.capture_native_scope(txn, history)).await?;
    }
    Ok(result)
}
