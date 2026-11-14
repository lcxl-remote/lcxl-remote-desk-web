//! Atomic occurrence/session claim for an explicitly confirmed continuation.
use super::{ScheduleStore, ScheduleStoreError};
use desk_assistant_data_store::schedule::PermissionReceiptClaim;
pub use desk_assistant_data_store::schedule::{
    ClaimedContinuation, ContinuationClaim, ContinuationPermissionClaim,
};

impl ScheduleStore {
    pub fn claim_conversation_resume<'a>(
        &'a self,
        input: ContinuationClaim<'a>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ClaimedContinuation, ScheduleStoreError>>
                + Send
                + 'a,
        >,
    > {
        desk_diagnose_core::future::send_phase(|| self.claim_continuation_on(input, None))
    }

    pub fn claim_continuation_permission<'a>(
        &'a self,
        input: ContinuationPermissionClaim<'a>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<ClaimedContinuation, ScheduleStoreError>>
                + Send
                + 'a,
        >,
    > {
        let ContinuationPermissionClaim {
            continuation,
            request_id,
            expected_session_version,
            expected_run_epoch,
            grants,
        } = input;
        desk_diagnose_core::future::send_phase(|| {
            self.claim_continuation_on(
                continuation,
                Some(PermissionReceiptClaim {
                    request_id,
                    version: expected_session_version,
                    epoch: expected_run_epoch,
                    grants,
                }),
            )
        })
    }

    async fn claim_continuation_on(
        &self,
        input: ContinuationClaim<'_>,
        permission: Option<PermissionReceiptClaim<'_>>,
    ) -> Result<ClaimedContinuation, ScheduleStoreError> {
        if input.owner <= 0
            || input.node_id.is_empty()
            || input.node_id.len() > 256
            || !(30..=300).contains(&input.lease_seconds)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let txn = crate::db::begin_write(&self.db, crate::entity::agent_schedule::Entity).await?;
        let claimed = desk_diagnose_core::future::send_phase(|| {
            desk_assistant_data_store::schedule::claim_continuation_on(
                &super::SignalScheduleHost,
                &txn,
                input,
                permission,
            )
        })
        .await?;
        txn.commit().await?;
        Ok(claimed)
    }
}

#[cfg(test)]
pub(super) mod tests;
