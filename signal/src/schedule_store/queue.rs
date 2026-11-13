//! Calendar materialization and lease claims in host write transactions.
use super::{ScheduleStore, ScheduleStoreError};
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_schedule_run as run;
use desk_agent_protocol::schedule::policy::ScheduleBudgetPolicy;
use desk_assistant_data_store::schedule as shared;
#[cfg(test)]
use desk_assistant_data_store::schedule::database_now;
#[cfg(test)]
use desk_diagnose_core::schedule::due::start_deadline;
use sea_orm::DbErr;
#[cfg(test)]
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, Set};

#[cfg(test)]
mod tests;

/// Claims freeze the policy from the claim transaction's `config.toml` snapshot.
struct ConfigBudgetPolicy;

#[async_trait::async_trait]
impl shared::BudgetPolicySource<DatabaseTransaction> for ConfigBudgetPolicy {
    async fn read(&self, txn: &DatabaseTransaction) -> Result<ScheduleBudgetPolicy, DbErr> {
        crate::schedule_budget_policy::read(txn).await
    }
}

impl ScheduleStore {
    #[cfg(test)]
    pub async fn enqueue_manual(
        &self,
        owner: i32,
        schedule_id: &str,
        client_key: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.enqueue_manual_checked(owner, schedule_id, client_key, None)
            .await
    }

    pub async fn enqueue_manual_at_revision(
        &self,
        owner: i32,
        schedule_id: &str,
        client_key: &str,
        expected_revision: i64,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.enqueue_manual_checked(owner, schedule_id, client_key, Some(expected_revision))
            .await
    }

    async fn enqueue_manual_checked(
        &self,
        owner: i32,
        schedule_id: &str,
        client_key: &str,
        expected_revision: Option<i64>,
    ) -> Result<run::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result =
            shared::enqueue_manual_on(&txn, owner, schedule_id, client_key, expected_revision)
                .await?;
        txn.commit().await?;
        Ok(result)
    }

    pub async fn due_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<super::entity::Model>, ScheduleStoreError> {
        shared::due_candidates(&self.db, after_id, limit).await
    }

    pub async fn materialize_due(
        &self,
        schedule_id: &str,
        expected_revision: i64,
    ) -> Result<Option<run::Model>, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = shared::materialize_due_on(&txn, schedule_id, expected_revision).await?;
        txn.commit().await?;
        Ok(result)
    }

    #[cfg(test)]
    pub async fn claim_queued(
        &self,
        run_id: &str,
        node_id: &str,
        lease_seconds: u32,
    ) -> Result<run::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = Self::claim_queued_on(&txn, run_id, node_id, lease_seconds).await?;
        txn.commit().await?;
        Ok(result)
    }

    /// The caller must roll back on any error and commit only after pairing the
    /// claimed occurrence with its fresh session and current authorization.
    pub async fn claim_queued_on(
        txn: &DatabaseTransaction,
        run_id: &str,
        node_id: &str,
        lease_seconds: u32,
    ) -> Result<run::Model, ScheduleStoreError> {
        shared::claim_queued_on(txn, run_id, node_id, lease_seconds, &ConfigBudgetPolicy).await
    }

    #[cfg(test)]
    pub async fn renew_run(
        &self,
        run_id: &str,
        node: &str,
        epoch: i64,
        lease_seconds: u32,
    ) -> Result<bool, ScheduleStoreError> {
        if !(30..=300).contains(&lease_seconds) {
            return Err(ScheduleStoreError::Invalid);
        }
        let now = database_now(&self.db).await?;
        let changed = run::Entity::update_many()
            .set(run::ActiveModel {
                lease_deadline: Set(Some(
                    start_deadline(now, lease_seconds).map_err(|_| ScheduleStoreError::Invalid)?,
                )),
                updated_at: Set(now),
                ..Default::default()
            })
            .filter(run::Column::RunId.eq(run_id))
            .filter(run::Column::LeaseOwner.eq(node))
            .filter(run::Column::LeaseEpoch.eq(epoch))
            .filter(run::Column::Status.eq("running"))
            .filter(run::Column::LeaseDeadline.gt(now))
            .exec(&self.db)
            .await?;
        Ok(changed.rows_affected == 1)
    }
}
