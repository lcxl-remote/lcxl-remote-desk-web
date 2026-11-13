//! Advisory dispatcher candidates; only a paired claim can authorize execution.
use super::{ScheduleStore, ScheduleStoreError};
use crate::entity::agent_schedule_run as run;
use desk_assistant_data_store::schedule as shared;

impl ScheduleStore {
    pub async fn expired_continuation_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        shared::expired_continuation_candidates(&self.db, after_id, limit).await
    }

    pub async fn fresh_task_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        shared::fresh_task_candidates(&self.db, after_id, limit).await
    }

    pub async fn continuation_candidates(
        &self,
        after_id: i64,
        limit: u64,
    ) -> Result<Vec<run::Model>, ScheduleStoreError> {
        shared::continuation_candidates(&self.db, after_id, limit).await
    }
}

#[cfg(test)]
mod tests;
