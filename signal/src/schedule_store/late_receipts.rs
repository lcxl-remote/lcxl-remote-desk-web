//! Reconcile receipts that arrived after a fresh occurrence settled as unknown.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_schedule_run as run;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

impl ScheduleStore {
    pub async fn reconcile_late_task_receipts(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let waiting_children = run::Entity::find()
            .filter(run::Column::RunId.eq(run_id))
            .one(&self.db)
            .await?
            .is_some_and(|work| {
                work.result_ref.as_deref().is_some_and(|reference| {
                    reference.starts_with("children:")
                        || reference.starts_with("stopped-children:")
                        || reference.starts_with("answer-children:")
                        || reference.starts_with("delegated-effects:")
                })
            });
        if waiting_children {
            return self.reconcile_fresh_children_receipts(run_id).await;
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let decision = desk_assistant_data_store::schedule::reconcile_late_task_receipts_on(
            &super::SignalScheduleHost,
            &txn,
            run_id,
        )
        .await?;
        super::session_host::finish(txn, decision).await
    }
}
