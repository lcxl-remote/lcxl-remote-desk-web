//! Fenced finalization atomically settles the active slot and failure policy.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::schedule::ScheduledRunStatus;
#[cfg(test)]
use desk_assistant_data_store::schedule::database_now;
#[cfg(test)]
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

impl ScheduleStore {
    #[cfg(test)]
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
        let decision = desk_assistant_data_store::schedule::expire_pending_on(
            &super::SignalScheduleHost,
            &txn,
            run_id,
        )
        .await?;
        super::session_host::finish(txn, decision).await
    }

    pub async fn expired_pending(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        desk_assistant_data_store::schedule::expired_pending(&self.db, after_id, limit).await
    }
}

/// Settlement inside a host transaction that this call commits.
pub(super) struct Settlement {
    pub(super) txn: crate::config::connection::DatabaseTransaction,
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
    let result = desk_assistant_data_store::schedule::settle_on(
        &super::SignalScheduleHost,
        &txn,
        desk_assistant_data_store::schedule::Settlement {
            work,
            now,
            outcome,
            offline_timeout,
            error_kind,
            result_ref,
        },
    )
    .await?;
    txn.commit().await?;
    Ok(result)
}
