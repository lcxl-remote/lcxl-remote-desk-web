//! Expire or cancel a fresh occurrence's owner-approval wait.
use super::{ScheduleStore, ScheduleStoreError};

impl ScheduleStore {
    pub async fn expire_fresh_approval_wait(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let decision = desk_assistant_data_store::schedule::expire_fresh_approval_wait_on(
            &super::SignalScheduleHost,
            &txn,
            run_id,
        )
        .await?;
        super::session_host::finish(txn, decision).await
    }
}
