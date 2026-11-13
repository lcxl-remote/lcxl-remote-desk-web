//! The owner's review of an occurrence whose device effects are unknown.
use super::{ScheduleStore, ScheduleStoreError};

impl ScheduleStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn acknowledge_run_outcome(
        &self,
        owner: i32,
        schedule_id: &str,
        run_id: &str,
        expected_revision: i64,
        key: &str,
        note: &str,
    ) -> Result<(), ScheduleStoreError> {
        self.review_run_outcome(
            owner,
            schedule_id,
            run_id,
            expected_revision,
            key,
            note,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn dispose_run_outcome(
        &self,
        owner: i32,
        schedule_id: &str,
        run_id: &str,
        expected_revision: i64,
        key: &str,
        note: &str,
        work_id: i64,
        execution_id: &str,
    ) -> Result<(), ScheduleStoreError> {
        if work_id <= 0 || execution_id.is_empty() || execution_id.len() > 256 {
            return Err(ScheduleStoreError::Invalid);
        }
        self.review_run_outcome(
            owner,
            schedule_id,
            run_id,
            expected_revision,
            key,
            note,
            Some((work_id, execution_id)),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn review_run_outcome(
        &self,
        owner: i32,
        schedule_id: &str,
        run_id: &str,
        expected_revision: i64,
        key: &str,
        note: &str,
        disposition: Option<(i64, &str)>,
    ) -> Result<(), ScheduleStoreError> {
        if owner <= 0
            || key.is_empty()
            || key.len() > 128
            || key.chars().any(char::is_control)
            || note.trim().is_empty()
            || note.len() > 2048
            || note.chars().any(|c| c.is_control() && c != '\n')
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        desk_assistant_data_store::schedule::review_run_outcome_on(
            &super::SignalScheduleHost,
            &txn,
            owner,
            schedule_id,
            run_id,
            expected_revision,
            key,
            note,
            disposition,
        )
        .await?;
        txn.commit().await?;
        Ok(())
    }
}
