//! Recover action-free expired executions without replaying their model context.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_schedule_run as run;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

impl ScheduleStore {
    pub(super) async fn expired_fresh_candidates(
        &self,
        after: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        let now = self.database_time().await?;
        Ok(run::Entity::find()
            .filter(run::Column::Id.gt(after))
            .filter(run::Column::Status.eq("running"))
            .filter(run::Column::LeaseDeadline.lte(now))
            .filter(run::Column::FailureAccounted.eq(false))
            .filter(run::Column::FinishedAt.is_null())
            .order_by_asc(run::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    pub fn recover_action_free_fresh_task<'a>(
        &'a self,
        run_id: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<bool, ScheduleStoreError>> + Send + 'a>,
    > {
        desk_diagnose_core::future::send_phase(|| async move {
            let txn =
                crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
            let decision = desk_diagnose_core::future::send_phase(|| {
                desk_assistant_data_store::schedule::recover_action_free_fresh_task_on(
                    &super::SignalScheduleHost,
                    &txn,
                    run_id,
                )
            })
            .await?;
            super::session_host::finish(txn, decision).await
        })
    }
}
