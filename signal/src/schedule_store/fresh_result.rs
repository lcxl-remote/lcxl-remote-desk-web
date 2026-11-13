//! Settle fresh runs from persisted terminal state and original effect evidence.
use super::{FreshTaskLease, ScheduleStore, ScheduleStoreError};
use crate::entity::agent_schedule_run as run;
use desk_assistant_data_store::schedule::FreshResult;

impl ScheduleStore {
    pub async fn finish_answered_fresh_task(
        &self,
        lease: FreshTaskLease<'_>,
        answer: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Answer(answer))
            .await
    }

    pub async fn finish_failed_fresh_task(
        &self,
        lease: FreshTaskLease<'_>,
        error: &desk_agent_protocol::AgentError,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Failure(error))
            .await
    }

    /// Preserve the active occurrence while the owner considers one durable request.
    pub async fn await_fresh_task_permission(
        &self,
        lease: FreshTaskLease<'_>,
        request_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Permission(request_id))
            .await
    }

    /// Waiting releases only the parent's planning holder. Its original
    /// occurrence, source deadline, authorization and shared quota remain fixed.
    pub async fn await_fresh_task_children(
        &self,
        lease: FreshTaskLease<'_>,
        wait_id: &str,
    ) -> Result<run::Model, ScheduleStoreError> {
        self.finish_fresh_result(lease, FreshResult::Children(wait_id))
            .await
    }

    async fn finish_fresh_result(
        &self,
        lease: FreshTaskLease<'_>,
        result: FreshResult<'_>,
    ) -> Result<run::Model, ScheduleStoreError> {
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let decision = desk_assistant_data_store::schedule::finish_fresh_result_on(
            &super::SignalScheduleHost,
            &txn,
            lease,
            result,
        )
        .await?;
        super::session_host::finish(txn, decision).await
    }
}
