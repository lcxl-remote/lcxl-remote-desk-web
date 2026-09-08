//! Owner cancellation persists intent without claiming to undo external effects.
use super::queue::database_now;
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};

impl ScheduleStore {
    /// The executor observes cancel_requested_at and cancels the matching run lease.
    pub async fn cancel_run(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let now = database_now(&txn).await?;
        let work = run::Entity::find()
            .filter(run::Column::OwnerUserId.eq(owner))
            .filter(run::Column::RunId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.failure_accounted {
            return Ok(work);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .filter(entity::Column::OwnerUserId.eq(owner))
            .one(&txn)
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
            .exec(&txn)
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
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let result = run::Entity::find_by_id(work.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        txn.commit().await?;
        if task.kind == "fresh_task"
            && result.status == "awaiting_permission"
            && matches!(self.expire_fresh_approval_wait(run_id).await, Ok(true))
        {
            return run::Entity::find_by_id(result.id)
                .one(&self.db)
                .await?
                .ok_or(ScheduleStoreError::NotFound);
        }
        if task.kind == "fresh_task" {
            let _ = self.propagate_task_cancellation(run_id).await;
        }
        // Cancellation intent is durable even if immediate cleanup contends.
        // The background wait scan will retry without redispatching the run.
        Ok(result)
    }

    pub async fn cancellation_requested(
        &self,
        run_id: &str,
        node: &str,
        epoch: i64,
    ) -> Result<bool, ScheduleStoreError> {
        let row = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .filter(run::Column::LeaseOwner.eq(node))
            .filter(run::Column::LeaseEpoch.eq(epoch))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::Conflict)?;
        Ok(row.cancel_requested_at.is_some())
    }
}
