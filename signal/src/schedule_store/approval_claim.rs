//! Reclaim a fresh occurrence after its owner approved the waiting request.
use super::{FreshTaskClaim, ScheduleStore, ScheduleStoreError};
use desk_diagnose_core::session::PersistedAgentSession;

impl ScheduleStore {
    pub(super) async fn claim_fresh_approval_on(
        txn: &crate::config::connection::DatabaseTransaction,
        input: FreshTaskClaim<'_>,
        request_id: &str,
    ) -> Result<PersistedAgentSession, ScheduleStoreError> {
        desk_assistant_data_store::schedule::claim_fresh_approval_on(
            &super::SignalScheduleHost,
            txn,
            input,
            request_id,
        )
        .await
    }
}
