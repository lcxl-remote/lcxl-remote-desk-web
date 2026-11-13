//! Waiting-task permission inspection. This cannot claim a model or issue a task grant.
pub(crate) use desk_assistant_data_store::schedule::TaskApprovalReview;
use sea_orm::DbErr;

/// Called before locking the permission/session row, after the owner's ordinary
/// authorization. Returns frozen review constraints, not execution authority.
pub(crate) async fn lock_fresh_approval_on(
    txn: &crate::config::connection::DatabaseTransaction,
    conversation: &str,
    request_id: &str,
) -> Result<Option<TaskApprovalReview>, DbErr> {
    desk_assistant_data_store::schedule::lock_fresh_approval_on(
        &super::SignalScheduleHost,
        txn,
        conversation,
        request_id,
    )
    .await
}
