//! Fence owner decisions and cancellation of the original conversation pause.
use super::{ScheduleStore, ScheduleStoreError, entity};

impl ScheduleStore {
    pub(super) async fn cancel_continuation_wait(
        &self,
        run_id: &str,
    ) -> Result<bool, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, entity::Entity).await?;
        let Some((work, now)) = desk_assistant_data_store::schedule::cancel_continuation_wait_on(
            &super::SignalScheduleHost,
            &txn,
            run_id,
        )
        .await?
        else {
            return Ok(false);
        };
        let result_ref = work.result_ref.clone();
        super::settlement::settle(super::settlement::Settlement {
            txn,
            work,
            now,
            outcome: desk_agent_protocol::schedule::ScheduledRunStatus::Cancelled,
            offline_timeout: false,
            error_kind: Some("cancelled".into()),
            result_ref,
        })
        .await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
