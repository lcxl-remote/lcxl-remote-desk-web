//! Fenced finalization atomically settles the active slot and failure policy.
use super::queue::database_now;
use super::{ScheduleStore, ScheduleStoreError, entity, json};
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::schedule::ScheduledRunStatus;
use desk_diagnose_core::schedule::lifecycle::FailureState;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set};

impl ScheduleStore {
    /// A result acknowledges the exact live lease. Unknown effects pause all future runs.
    pub async fn finish_run(
        &self,
        run_id: &str,
        node: &str,
        epoch: i64,
        outcome: ScheduledRunStatus,
        error_kind: Option<String>,
        result_ref: Option<String>,
    ) -> Result<run::Model, ScheduleStoreError> {
        if !matches!(
            outcome,
            ScheduledRunStatus::Succeeded
                | ScheduledRunStatus::Failed
                | ScheduledRunStatus::Cancelled
                | ScheduledRunStatus::OutcomeUnknown
        ) || error_kind.as_ref().is_some_and(|s| s.len() > 128)
            || result_ref.as_ref().is_some_and(|s| s.len() > 512)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let now = database_now(&txn).await?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.lease_epoch != epoch || work.lease_owner.as_deref() != Some(node) {
            return Err(ScheduleStoreError::Conflict);
        }
        if work.failure_accounted {
            return Ok(work);
        }
        if work.status != "running" || work.lease_deadline.is_none_or(|at| at <= now) {
            return Err(ScheduleStoreError::Conflict);
        }
        settle(Settlement {
            txn,
            work,
            now,
            outcome,
            offline_timeout: false,
            error_kind,
            result_ref,
        })
        .await
    }

    /// Unstarted work can expire safely; leased work must first be reconciled.
    pub async fn expire_pending(&self, run_id: &str) -> Result<run::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let now = database_now(&txn).await?;
        let work = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&txn)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if work.failure_accounted && work.status == "missed" {
            return Ok(work);
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
        let offline = work.status == "waiting_device";
        let error = if offline {
            "device_offline_timeout"
        } else {
            "queue_timeout"
        };
        settle(Settlement {
            txn,
            work,
            now,
            outcome: ScheduledRunStatus::Missed,
            offline_timeout: offline,
            error_kind: Some(error.into()),
            result_ref: None,
        })
        .await
    }

    pub async fn expired_pending(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        if after_id < 0 || limit == 0 || limit > 100 {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = database_now(&self.db).await?;
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after_id))
            .filter(run::Column::Status.is_in(["queued", "waiting_device"]))
            .filter(run::Column::FailureAccounted.eq(false))
            .filter(run::Column::StartDeadline.lte(now))
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }
}

pub(super) struct Settlement {
    pub(super) txn: sea_orm::DatabaseTransaction,
    pub(super) work: run::Model,
    pub(super) now: i64,
    pub(super) outcome: ScheduledRunStatus,
    pub(super) offline_timeout: bool,
    pub(super) error_kind: Option<String>,
    pub(super) result_ref: Option<String>,
}

pub(super) async fn settle(args: Settlement) -> Result<run::Model, ScheduleStoreError> {
    let Settlement {
        txn,
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
        .one(&txn)
        .await?
        .ok_or(ScheduleStoreError::NotFound)?;
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
    let task_status = if task.status == "deleted" {
        "deleted"
    } else if matches!(
        task.status.as_str(),
        "draft" | "rehearsing" | "awaiting_authorization"
    ) {
        &task.status
    } else if paused {
        "paused"
    } else if task.next_run_at.is_some() {
        "active"
    } else {
        "completed"
    };
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
        .exec(&txn)
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
