//! Versioned task edits and tombstones preserve immutable running snapshots.
use super::{ScheduleStore, ScheduleStoreError, entity};
use desk_agent_protocol::schedule::ScheduleSpec;

impl ScheduleStore {
    /// Change failure policy only between runs. This never clears pauses or
    /// changes the recovery epoch, and it cannot revive a completed occurrence.
    pub async fn set_failure_threshold(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        threshold: u32,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = desk_assistant_data_store::schedule::set_failure_threshold_on(
            &txn, owner, id, expected, threshold,
        )
        .await?;
        txn.commit().await?;
        Ok(result)
    }

    pub async fn rename(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        title: &str,
    ) -> Result<entity::Model, ScheduleStoreError> {
        desk_assistant_data_store::schedule::rename(&self.db, owner, id, expected, title).await
    }

    pub async fn change_time(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        spec: &ScheduleSpec,
    ) -> Result<entity::Model, ScheduleStoreError> {
        self.edit(owner, id, expected, Some(spec), None, false)
            .await
    }

    /// A changed requirement takes effect from the next run.
    pub async fn change_prompt(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        prompt: &str,
    ) -> Result<entity::Model, ScheduleStoreError> {
        if prompt.trim().is_empty()
            || prompt.len() > desk_diagnose_core::schedule::MAX_SCHEDULE_PROMPT_CHARS
        {
            return Err(ScheduleStoreError::Invalid);
        }
        self.edit(owner, id, expected, None, Some(prompt), false)
            .await
    }

    pub async fn delete(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
    ) -> Result<entity::Model, ScheduleStoreError> {
        self.edit(owner, id, expected, None, None, true).await
    }

    async fn edit(
        &self,
        owner: i32,
        id: &str,
        expected: i64,
        new_spec: Option<&ScheduleSpec>,
        prompt: Option<&str>,
        delete: bool,
    ) -> Result<entity::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let result = desk_assistant_data_store::schedule::edit_on(
            &super::session_host::SignalScheduleHost,
            &txn,
            owner,
            id,
            expected,
            new_spec,
            prompt,
            delete,
        )
        .await?;
        txn.commit().await?;
        Ok(result)
    }
}
