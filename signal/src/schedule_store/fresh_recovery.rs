//! Recover action-free expired executions without replaying their model context.
use super::{ScheduleStore, ScheduleStoreError};

impl ScheduleStore {
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
