//! Scheduled-task storage rules shared by Manager and standalone Signal.
//! Callers own transactions, locks taken before a rule, and policy sources.
use crate::entity::{agent_schedule, agent_task_budget_reservation as ledger};
#[cfg(test)]
use crate::entity_table::EntityTableExt;
use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
use sea_orm::{ConnectionTrait, DbErr, sea_query::Index};
use sha2::{Digest, Sha256};

mod approval_admission;
mod approval_claim;
mod approval_expiry;
mod approval_request;
mod authority;
mod budget;
mod cancellation;
mod candidates;
mod continuation_wait;
mod create;
mod device_wait;
mod directory_receipt;
mod edit;
mod fresh_children;
mod fresh_children_settlement;
mod fresh_recovery;
mod fresh_result;
mod history;
mod late_receipts;
mod model_management;
mod outcome_review;
mod proposal;
mod proposal_review;
mod queue;
mod receipt_context;
mod resume_activation;
mod resume_admission;
mod resume_claim;
mod resume_lease;
mod resume_result;
mod review_wait;
mod scan;
mod search;
pub mod session;
mod settlement;
mod task;
#[cfg(test)]
mod test_host;
pub use approval_admission::{TaskApprovalReview, lock_fresh_approval_on};
pub use approval_claim::claim_fresh_approval_on;
pub use approval_expiry::{expire_fresh_approval_wait_on, wait_expiry_candidates};
pub use approval_request::{validate_task_directory_on, validate_task_permission_on};
pub use authority::{
    CurrentDelegationSourceAuthority, CurrentTaskAuthority, PendingTaskApproval, authority_now,
    lock_delegation_source_authority, lock_run_authority, lock_waiting_approval,
};
pub use budget::{
    TaskBudgetKind, TaskBudgetRequest, authority_of, reserve_delegation_model_budget_on,
    reserve_run_budget, reserve_task_budget, settle_task_model_budget,
};
pub use cancellation::{expired_pending, record_run_cancellation_on};
pub use candidates::{
    continuation_candidates, expired_continuation_candidates, fresh_task_candidates,
};
pub use continuation_wait::{cancel_continuation_wait_on, lock_approval_on};
pub use create::create_task_on;
pub use device_wait::wait_for_device_on;
pub use directory_receipt::verify_directory_receipt_on;
pub use edit::{edit_on, rename, set_failure_threshold_on, stop_pending};
pub use fresh_children::claim_fresh_children_on;
pub use fresh_children_settlement::{
    ChildrenWaitClosed, reconcile_fresh_children_receipts_on, settle_fresh_children_wait_on,
};
pub use fresh_recovery::recover_action_free_fresh_task_on;
pub use fresh_result::{FreshResult, finish_fresh_result_on};
pub use history::{RunHistoryPage, resolve_run_session, run_history};
pub use late_receipts::{late_receipt_candidates, reconcile_late_task_receipts_on};
pub use model_management::{
    cancel as cancel_managed_task, list as list_managed_tasks, lock_target,
    view as managed_task_view,
};
pub use outcome_review::{review_run_outcome_on, reviewed_at as outcome_reviewed_at};
pub use proposal::manage_on;
pub use proposal_review::{decide_by_ai_on, review_proposal_on};
pub use queue::{
    BudgetPolicySource, claim_queued_on, database_now, due_candidates, enqueue_manual_on,
    materialize_due_on,
};
pub use receipt_context::TaskReceiptContext;
pub use resume_activation::{
    ProposalDecider, activate_on, decide_proposal_on, find_task, lock_original_requirement,
    record_decision_on,
};
pub use resume_admission::{
    assistant_retrieval_deadline_on, fresh_action_authority_on, lock_action_session,
    same_retrieval_definition, scheduled_context_on,
};
pub use resume_claim::{
    ClaimedContinuation, ContinuationClaim, ContinuationPermissionClaim, FreshTaskClaim,
    PermissionReceiptClaim, claim_continuation_on,
};
pub use resume_lease::{ContinuationLease, FreshTaskLease, renew_paired_lease_on};
pub use resume_result::{
    CommittedResult, SettlementLease, committed_result, record_committed_continuation_on,
};
pub use review_wait::{ReviewPoll, poll_review_decision_on};
pub use scan::{
    ScanCursors, ScanWarning, ScheduleScanOps, ScheduleScanReport, scan_action_cancellations_once,
    scan_calendar_once, scan_children_wait_settlement_once, scan_committed_recovery_once,
    scan_fresh_recovery_once, scan_late_receipts_once, scan_pending_expiry_once, scan_tick,
    scan_wait_expiry_once,
};
pub use search::search;
pub use settlement::{Settlement, expire_pending_on, settle_on};
pub use task::{list, pause_on, read};

#[derive(Debug)]
pub enum ScheduleStoreError {
    Invalid,
    ToolInput(&'static str),
    NotFound,
    Conflict,
    BudgetExceeded,
    /// A conversation timer would run after its source session expires.
    ExceedsSessionRetention {
        max_delay_seconds: u64,
    },
    Backend(DbErr),
}
impl From<DbErr> for ScheduleStoreError {
    fn from(error: DbErr) -> Self {
        Self::Backend(error)
    }
}

/// A bounded, trimmed identifier without control characters.
pub fn key(value: &str) -> Result<(), ScheduleStoreError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(ScheduleStoreError::Invalid);
    }
    Ok(())
}

pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn json<T: serde::Serialize>(value: &T) -> Result<String, ScheduleStoreError> {
    serde_json::to_string(value).map_err(|_| ScheduleStoreError::Invalid)
}

pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// The task's frozen retrieval consent, validated against its definition.
pub fn retrieval_consent(
    task: &agent_schedule::Model,
) -> Result<Option<TaskRetrievalConsent>, ScheduleStoreError> {
    let Some(raw) = task.retrieval_consent_json.as_deref() else {
        return Ok(None);
    };
    let consent = TaskRetrievalConsent::decode(raw).map_err(|_| ScheduleStoreError::Invalid)?;
    consent
        .validate_definition(
            &task.schedule_id,
            task.task_revision
                .try_into()
                .map_err(|_| ScheduleStoreError::Invalid)?,
            &task.owner_user_id.to_string(),
            &task.target_device_id,
            consent.source_conversation_id.as_deref(),
        )
        .map_err(|_| ScheduleStoreError::Conflict)?;
    if task.kind == "conversation_resume"
        && task.source_conversation_id.as_deref() != consent.source_conversation_id.as_deref()
    {
        return Err(ScheduleStoreError::Conflict);
    }
    Ok(Some(consent))
}

/// The source session device a continuation was proposed from.
pub fn continuation_subject(task: &agent_schedule::Model) -> Result<String, ScheduleStoreError> {
    retrieval_consent(task)?
        .map(|consent| consent.source_session_device_id)
        .ok_or(ScheduleStoreError::Invalid)
}

/// Due scans and budget windows use bounded index ranges.
pub async fn ensure_indexes<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    let due = Index::create()
        .if_not_exists()
        .name("idx_agent_schedule_due")
        .table(agent_schedule::Entity)
        .col(agent_schedule::Column::Status)
        .col(agent_schedule::Column::NextRunAt)
        .col(agent_schedule::Column::Id)
        .to_owned();
    db.execute(&due).await?;
    let budget_window = Index::create()
        .if_not_exists()
        .name("idx_task_budget_schedule_day")
        .table(ledger::Entity)
        .col(ledger::Column::ScheduleId)
        .col(ledger::Column::Kind)
        .col(ledger::Column::UtcDay)
        .to_owned();
    db.execute(&budget_window).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_bounded_trimmed_and_printable() {
        assert!(key("task-1").is_ok());
        for value in ["", " task", "task ", "a\nb"] {
            assert!(matches!(key(value), Err(ScheduleStoreError::Invalid)));
        }
        assert!(key(&"k".repeat(256)).is_ok());
        assert!(key(&"k".repeat(257)).is_err());
    }

    #[test]
    fn digests_are_lowercase_sha256_hex() {
        let value = digest("schedule");
        assert!(valid_digest(&value));
        assert!(!valid_digest(&value.to_uppercase()));
        assert!(!valid_digest(&value[1..]));
    }

    #[tokio::test]
    async fn indexes_are_idempotent_on_existing_tables() {
        use sea_orm::{ConnectionTrait, Database, Schema};
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.entity_table(agent_schedule::Entity))
            .await
            .unwrap();
        db.execute(&schema.entity_table(ledger::Entity))
            .await
            .unwrap();
        ensure_indexes(&db).await.unwrap();
        ensure_indexes(&db).await.unwrap();
    }
}
