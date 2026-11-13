//! Apply the AI reviewer's verdict on a pending fresh-task proposal.
use super::ScheduleStoreError;
use desk_diagnose_core::schedule::review::ScheduleProposal;

/// Apply the reviewer's verdict to the exact reviewed revision in the
/// caller's review-settlement transaction.
pub(crate) async fn decide_by_ai_on(
    txn: &crate::config::connection::DatabaseTransaction,
    owner: &str,
    proposal: &ScheduleProposal,
    approve: bool,
    reason: &str,
) -> Result<(), ScheduleStoreError> {
    desk_assistant_data_store::schedule::decide_by_ai_on(
        &super::session_host::SignalScheduleHost,
        txn,
        owner,
        proposal,
        approve,
        reason,
    )
    .await
}
