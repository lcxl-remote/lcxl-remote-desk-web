//! Owner cancellation intent and the expiry scan of unstarted occurrences.
use super::{ScheduleStoreError, queue::database_now};
use crate::entity::{agent_schedule as entity, agent_schedule_run as run};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};

/// Records the owner's cancellation intent. Unstarted work is cancelled at once;
/// started work keeps its status for the executor to observe. Returns `None` for
/// the task when the occurrence was already accounted for.
pub async fn record_run_cancellation_on<C: ConnectionTrait>(
    txn: &C,
    owner: i32,
    run_id: &str,
) -> Result<(Option<Box<entity::Model>>, Box<run::Model>), ScheduleStoreError> {
    let now = database_now(txn).await?;
    let work = run::Entity::find()
        .filter(run::Column::OwnerUserId.eq(owner))
        .filter(run::Column::RunId.eq(run_id))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if work.failure_accounted {
        return Ok((None, Box::new(work)));
    }
    let task = entity::Entity::find()
        .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
        .filter(entity::Column::OwnerUserId.eq(owner))
        .one(txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
    if task.active_run_id.as_deref() != Some(run_id) {
        return Err(ScheduleStoreError::Conflict);
    }
    let unstarted = matches!(work.status.as_str(), "queued" | "waiting_device");
    let next_status = if unstarted
        && task.next_run_at.is_none()
        && matches!(task.status.as_str(), "triggered" | "active")
    {
        "completed"
    } else {
        &task.status
    };
    let touched = entity::Entity::update_many()
        .set(entity::ActiveModel {
            status: Set(next_status.into()),
            revision: Set(task
                .revision
                .checked_add(1)
                .ok_or(ScheduleStoreError::Invalid)?),
            active_run_id: Set(if unstarted {
                None
            } else {
                task.active_run_id.clone()
            }),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(entity::Column::Id.eq(task.id))
        .filter(entity::Column::Revision.eq(task.revision))
        .exec(txn)
        .await?;
    if touched.rows_affected != 1 {
        return Err(ScheduleStoreError::Conflict);
    }
    let changed = run::Entity::update_many()
        .set(run::ActiveModel {
            cancel_requested_at: Set(work.cancel_requested_at.or(Some(now))),
            status: Set(if unstarted {
                "cancelled".into()
            } else {
                work.status.clone()
            }),
            failure_accounted: Set(unstarted),
            finished_at: Set(if unstarted {
                Some(now)
            } else {
                work.finished_at
            }),
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
    Ok((Some(Box::new(task)), Box::new(result)))
}

/// Unstarted occurrences past their start deadline, in id order.
pub async fn expired_pending<C: ConnectionTrait>(
    db: &C,
    after_id: i64,
    limit: u64,
) -> Result<Vec<run::Model>, ScheduleStoreError> {
    if after_id < 0 || limit == 0 || limit > 100 {
        return Err(ScheduleStoreError::Invalid);
    }
    let now = database_now(db).await?;
    Ok(run::Entity::find()
        .filter(run::Column::Id.gt(after_id))
        .filter(run::Column::Status.is_in(["queued", "waiting_device"]))
        .filter(run::Column::FailureAccounted.eq(false))
        .filter(run::Column::StartDeadline.lte(now))
        .order_by_asc(run::Column::Id)
        .limit(limit)
        .all(db)
        .await?)
}
