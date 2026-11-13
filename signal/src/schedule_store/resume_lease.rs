//! Renew a continuation's occurrence and session leases as one transaction.
use super::{ScheduleStore, ScheduleStoreError};
pub use desk_assistant_data_store::schedule::{ContinuationLease, FreshTaskLease};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};

impl ScheduleStore {
    /// Renew both leases only while the published parent authority remains current.
    /// This does not authorize model or tool calls; dispatch must check authority again.
    pub async fn renew_fresh_task(
        &self,
        lease: FreshTaskLease<'_>,
        lease_seconds: u32,
    ) -> Result<bool, ScheduleStoreError> {
        self.renew_paired_lease(
            ContinuationLease {
                owner: lease.owner,
                run_id: lease.run_id,
                node_id: lease.node_id,
                run_epoch: lease.run_epoch,
                session_token: lease.session_token,
            },
            lease_seconds,
            true,
        )
        .await
    }
    /// `false` stops the execution heartbeat. Never revive either expired lease.
    /// No session JSON or version is changed, so normal step saves can keep using
    /// their held CAS version. A user pause affects future runs, not this live run.
    pub async fn renew_conversation_resume(
        &self,
        lease: ContinuationLease<'_>,
        lease_seconds: u32,
    ) -> Result<bool, ScheduleStoreError> {
        self.renew_paired_lease(lease, lease_seconds, false).await
    }

    pub(super) async fn renew_paired_lease(
        &self,
        lease: ContinuationLease<'_>,
        lease_seconds: u32,
        fresh: bool,
    ) -> Result<bool, ScheduleStoreError> {
        if lease.owner <= 0
            || lease.run_epoch <= 0
            || lease.session_token == 0
            || lease.session_token > i64::MAX as u64
            || lease.node_id.is_empty()
            || lease.node_id.len() > 256
            || !(30..=300).contains(&lease_seconds)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        // Acquire SQLite's write reservation before establishing a read snapshot.
        // Concurrent timer and pre-call renewals must wait for each other rather
        // than fail when a deferred read transaction upgrades to a writer in WAL.
        let reserved = super::entity::Entity::update_many()
            .col_expr(
                super::entity::Column::Revision,
                Expr::col(super::entity::Column::Revision),
            )
            .filter(super::entity::Column::OwnerUserId.eq(lease.owner))
            .filter(super::entity::Column::ActiveRunId.eq(lease.run_id))
            .exec(&txn)
            .await?;
        if reserved.rows_affected != 1 {
            return Ok(false);
        }
        let renewed = desk_assistant_data_store::schedule::renew_paired_lease_on(
            &super::session_host::SignalScheduleHost,
            &txn,
            lease,
            lease_seconds,
            fresh,
        )
        .await?;
        if renewed {
            txn.commit().await?;
        }
        Ok(renewed)
    }
}

#[cfg(test)]
mod tests;
