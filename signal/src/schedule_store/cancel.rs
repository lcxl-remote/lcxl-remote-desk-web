//! Owner cancellation persists intent without claiming to undo external effects.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

impl ScheduleStore {
    /// The executor observes cancel_requested_at and cancels the matching run lease.
    pub async fn cancel_run(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        let (task, result) =
            desk_diagnose_core::future::send_phase(|| self.record_run_cancellation(owner, run_id))
                .await?;
        let Some(task) = task else {
            return Ok(*result);
        };

        if task.kind == "conversation_resume"
            && result.status == "awaiting_permission"
            && matches!(
                desk_diagnose_core::future::send_phase(|| self.cancel_continuation_wait(run_id))
                    .await,
                Ok(true)
            )
        {
            return run::Entity::find_by_id(result.id)
                .one(&self.db)
                .await?
                .ok_or(ScheduleStoreError::NotFound);
        }
        if task.kind == "fresh_task"
            && result.status == "awaiting_permission"
            && matches!(
                desk_diagnose_core::future::send_phase(|| self.expire_fresh_approval_wait(run_id))
                    .await,
                Ok(true)
            )
        {
            return run::Entity::find_by_id(result.id)
                .one(&self.db)
                .await?
                .ok_or(ScheduleStoreError::NotFound);
        }
        if task.kind == "fresh_task" {
            let _ =
                desk_diagnose_core::future::send_phase(|| self.propagate_task_cancellation(run_id))
                    .await;
        }
        // Cancellation intent is durable even if immediate cleanup contends.
        // The background wait scan will retry without redispatching the run.
        Ok(*result)
    }

    // Commit the owner's intent before attempting any original native cleanup.
    async fn record_run_cancellation(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<(Option<Box<entity::Model>>, Box<run::Model>), ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result =
            desk_assistant_data_store::schedule::record_run_cancellation_on(&txn, owner, run_id)
                .await?;
        txn.commit().await?;
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
