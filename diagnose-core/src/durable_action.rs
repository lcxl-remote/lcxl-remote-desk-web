//! Runtime-neutral durable lifecycle for side-effecting agent actions.
//!
//! Manager implements this over its shared database and lease-fenced outbox;
//! open-source Signal implements it over single-node SQLite. The contract keeps
//! the safety facts identical without forcing manager clustering machinery into
//! the OSS runtime.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::session::WorkKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedAction {
    pub work_id: i64,
    pub claim_token: String,
    pub attempt: i32,
    pub kind: WorkKind,
    pub action_request_id: String,
    /// Rolling-compatibility correlation for `ResolveExec`; absent for every
    /// non-exec action.
    pub exec_request_id: Option<String>,
    pub approval_id: Option<String>,
    pub payload_json: String,
    pub payload_schema_version: u16,
    pub is_side_effecting: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredAction {
    pub work_id: i64,
    pub kind: WorkKind,
    pub action_request_id: String,
    pub exec_request_id: Option<String>,
    pub conversation_id: String,
    pub tool_call_id: String,
    pub execution_id: Option<String>,
    pub new_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritebackOutcome {
    Applied,
    AlreadyResolved,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelRequestOutcome {
    ConvergedNotExecuted,
    Requested { execution_id: Option<String> },
    AlreadyTerminal,
    NotFound,
}

/// Common durable fact semantics. `E` remains runtime-specific: manager returns
/// `DbErr`, while Signal maps SQLite failures into its local `AgentError` surface.
#[async_trait]
pub trait DurableActionLifecycle<E>: Send + Sync {
    async fn claim_by_id(
        &self,
        work_id: i64,
        node_id: &str,
        lease: Duration,
    ) -> Result<Option<ClaimedAction>, E>;

    async fn renew(&self, claim_token: &str, lease: Duration) -> Result<bool, E>;

    async fn mark_dispatched(&self, claim_token: &str, execution_id: &str) -> Result<bool, E>;

    async fn rollback_unsent(&self, claim_token: &str, attempt: i32) -> Result<bool, E>;

    async fn writeback(
        &self,
        claim_token: &str,
        attempt: i32,
        result: Value,
    ) -> Result<WritebackOutcome, E>;

    async fn writeback_execution_result(
        &self,
        work_id: i64,
        execution_id: &str,
        attempt: i32,
        result: Value,
    ) -> Result<WritebackOutcome, E>;

    async fn recover_expired(&self, kind: WorkKind) -> Result<Vec<RecoveredAction>, E>;

    async fn manual_resolve(&self, work_id: i64) -> Result<WritebackOutcome, E>;

    async fn request_cancel(
        &self,
        work_id: i64,
        requested_by: &str,
        cancelled_result_json: &str,
    ) -> Result<CancelRequestOutcome, E>;
}
