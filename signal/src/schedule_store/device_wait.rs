//! Device absence releases executor resources while retaining the task's active slot.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_schedule_run as run;

impl ScheduleStore {
    /// Called by trusted device resolution before any model or action dispatch.
    pub async fn wait_for_device(&self, run_id: &str) -> Result<run::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = desk_assistant_data_store::schedule::wait_for_device_on(&txn, run_id).await?;
        txn.commit().await?;
        Ok(result)
    }
}
