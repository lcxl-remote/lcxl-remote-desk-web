//! Settle an unstarted occurrence rejected by current server limits.
use super::settlement::{Settlement, settle};
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::schedule::ScheduledRunStatus;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

impl ScheduleStore {
    /// No model, grant or work is created. Race with claim/cancel using the same
    /// task fence and retain ordinary failure counting and recurrence semantics.
    pub async fn reject_unstarted_budget_policy(
        &self,
        owner: i32,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let work = run::Entity::find()
            .filter(run::Column::OwnerUserId.eq(owner))
            .filter(run::Column::RunId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.failure_accounted
            || work.started_at.is_some()
            || work.attempt != 0
            || work.lease_owner.is_some()
            || work.lease_deadline.is_some()
            || work.lease_epoch != 0
            || work.cancel_requested_at.is_some()
            || !matches!(work.status.as_str(), "queued" | "waiting_device")
        {
            return Ok(false);
        }
        let task = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::ScheduleId.eq(&work.schedule_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if task.kind != "fresh_task"
            || task.active_run_id.as_deref() != Some(run_id)
            || !matches!(task.status.as_str(), "active" | "triggered")
        {
            return Ok(false);
        }
        let locked = entity::Entity::update_many()
            .col_expr(
                entity::Column::Revision,
                Expr::col(entity::Column::Revision),
            )
            .filter(entity::Column::Id.eq(task.id))
            .filter(entity::Column::Revision.eq(task.revision))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            return Err(ScheduleStoreError::Conflict);
        }
        let current = run::Entity::find_by_id(work.id)
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if current != work {
            return Err(ScheduleStoreError::Conflict);
        }
        let (_, contract) = super::publication::load_contract(
            &txn,
            owner,
            &task.schedule_id,
            task.contract_revision.ok_or(ScheduleStoreError::Conflict)?,
        )
        .await?;
        let policy = crate::schedule_budget_policy::read(&txn).await?;
        if desk_diagnose_core::schedule::policy::permits(&policy, &contract.contract().budget) {
            return Ok(false);
        }
        let now = super::queue::database_now(&txn).await?;
        settle(Settlement {
            txn,
            work,
            now,
            outcome: ScheduledRunStatus::Failed,
            offline_timeout: false,
            error_kind: Some("budget_policy_exceeded".into()),
            result_ref: None,
        })
        .await?;
        Ok(true)
    }
}
