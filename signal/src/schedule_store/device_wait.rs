//! Device absence releases executor resources while retaining the task's active slot.
use super::queue::database_now;
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use desk_diagnose_core::schedule::lifecycle::FailureState;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait};

impl ScheduleStore {
    /// Called by trusted device resolution before any model or action dispatch.
    pub async fn wait_for_device(&self, run_id: &str) -> Result<run::Model, ScheduleStoreError> {
        let txn = self.db.begin().await?;
        let now = database_now(&txn).await?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if !matches!(work.status.as_str(), "queued" | "waiting_device")
            || work.failure_accounted
            || now >= work.start_deadline
            || work.started_at.is_some()
            || work.lease_owner.is_some()
            || work.lease_deadline.is_some()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        let failures: FailureState = serde_json::from_str(&task.failure_state_json)
            .map_err(|_| ScheduleStoreError::Invalid)?;
        if !matches!(task.status.as_str(), "active" | "triggered")
            || task.active_run_id.as_deref() != Some(run_id)
            || !failures.pause_reasons.is_empty()
            || i64::try_from(failures.recovery_epoch).ok() != Some(work.recovery_epoch)
        {
            return Err(ScheduleStoreError::Conflict);
        }
        if work.status == "waiting_device" {
            return Ok(work);
        }
        let touched = entity::Entity::update_many()
            .set(entity::ActiveModel {
                revision: Set(task
                    .revision
                    .checked_add(1)
                    .ok_or(ScheduleStoreError::Invalid)?),
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
                status: Set("waiting_device".into()),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::Id.eq(work.id))
            .filter(run::Column::Status.eq("queued"))
            .filter(run::Column::LeaseEpoch.eq(work.lease_epoch))
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
        Ok(result)
    }
}
