//! Host session rows and fences reached from shared schedule rules.
//!
//! Both hosts keep their own `agent_session` table. These traits expose only
//! the columns, locks and host stores the schedule rules use. OSS serializes
//! writers with one write transaction, so its lock methods are no-ops; Manager
//! keeps its conversation and subject lock order across instances.
use super::ScheduleStoreError;
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::{ConnectionTrait, DbErr};

/// Session columns schedule rules read. `raw` goes back to host hooks and
/// keeps whole-row equality checks exact.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow<R> {
    pub raw: R,
    pub id: i64,
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub state_json: String,
    pub version: i64,
    pub lease_token: i64,
    pub lease_deadline_ms: Option<i64>,
    /// Manager `org_id IS NULL`; always true on OSS.
    pub personal: bool,
    /// Manager `turn_state`; `None` on OSS.
    pub turn_state: Option<String>,
}

/// How a lookup or guard treats organization sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrgMode {
    /// Manager adds `org_id IS NULL`.
    PersonalOnly,
    /// No organization condition; callers that must see organization sessions,
    /// or check `personal` after reading, use this.
    Any,
}

#[derive(Debug, Clone, Copy)]
pub enum SessionKey<'a> {
    Conversation {
        conversation: &'a str,
        actor: Option<&'a str>,
        device: Option<&'a str>,
    },
    Id(i64),
}

/// Columns a write changes; `None` leaves the column untouched.
#[derive(Debug, Clone, Copy, Default)]
pub struct SessionPatch<'a> {
    pub state_json: Option<&'a str>,
    pub version: Option<i64>,
    pub lease_token: Option<i64>,
    /// `Some(None)` writes NULL.
    pub lease_deadline_ms: Option<Option<i64>>,
    pub updated_at_ms: Option<i64>,
    /// Written by Manager only, and only when set.
    pub turn_state: Option<&'a str>,
}

/// Row conditions of a CAS or touch lock. Unset fields add no condition.
#[derive(Debug, Clone, Copy)]
pub struct SessionGuard<'a> {
    pub id: Option<i64>,
    pub conversation: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub device: Option<&'a str>,
    pub version: Option<i64>,
    pub lease_token: Option<i64>,
    pub state_json: Option<&'a str>,
    pub org: OrgMode,
}

impl SessionGuard<'_> {
    /// No conditions beyond the organization mode.
    pub fn any() -> Self {
        Self {
            id: None,
            conversation: None,
            actor: None,
            device: None,
            version: None,
            lease_token: None,
            state_json: None,
            org: OrgMode::Any,
        }
    }
}

/// The column a touch lock assigns to itself to take the row write lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchColumn {
    Version,
    LeaseToken,
}

/// A host lock or store refusal, mapped by each call site as before.
#[derive(Debug)]
pub enum LockError {
    Backend(DbErr),
    Denied,
}

#[async_trait::async_trait]
pub trait ScheduleLocks<C: ConnectionTrait>: Sync {
    /// Manager conversation advisory lock; no-op on OSS.
    async fn lock_conversation(&self, txn: &C, conversation: &str) -> Result<(), DbErr>;
}

#[async_trait::async_trait]
pub trait SessionRows<C: ConnectionTrait>: Sync {
    type Row: Clone + PartialEq + Send + Sync;
    async fn find_session(
        &self,
        txn: &C,
        key: SessionKey<'_>,
        org: OrgMode,
        exclusive: bool,
    ) -> Result<Option<SessionRow<Self::Row>>, DbErr>;
    /// Self-assigning UPDATE that takes the row write lock; returns affected rows.
    async fn touch_session(
        &self,
        txn: &C,
        column: TouchColumn,
        guard: SessionGuard<'_>,
    ) -> Result<u64, DbErr>;
    async fn write_session(
        &self,
        txn: &C,
        guard: SessionGuard<'_>,
        patch: SessionPatch<'_>,
    ) -> Result<u64, DbErr>;
}

/// Host stores and definition helpers reached from shared schedule rules.
#[async_trait::async_trait]
pub trait ScheduleHostStores<C: ConnectionTrait>: SessionRows<C> + ScheduleLocks<C> {
    /// Reads the directory receipt from the host run-event table and compares
    /// every column that table has. Missing row: NotFound; undecodable:
    /// Invalid; mismatch: Conflict.
    async fn directory_receipt(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        request_id: &str,
    ) -> Result<desk_diagnose_core::file_scope::transaction::FileScopeReceipt, ScheduleStoreError>;
    /// A conversation timer must run before its source session can be reclaimed.
    async fn check_resume_retention(
        &self,
        txn: &C,
        spec: &desk_agent_protocol::schedule::ScheduleSpec,
        now_ms: i64,
    ) -> Result<(), ScheduleStoreError>;
    /// Locks a delegated child's scheduled source before its session fence.
    async fn lock_child_source(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
    ) -> Result<(), DbErr>;
    /// Manager: the device row's owner is `owner` (an unparsable id is not).
    /// OSS has a single account and always returns true.
    async fn owns_device(&self, txn: &C, owner: i32, device: &str) -> Result<bool, DbErr>;
    /// Manager permission owner lock for the run's subject; no-op on OSS.
    async fn lock_permission_owner(
        &self,
        txn: &C,
        run_id: &str,
        actor: &str,
        device: &str,
    ) -> Result<(), LockError>;
    /// Consumes the owner's recorded decision for a continuation permission
    /// resume. OSS reports every refusal, including storage errors, as `Denied`.
    async fn consume_scheduled_decision(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        request_id: &str,
        grants: &[desk_agent_protocol::capability_grant::CapabilityGrant],
        now_ms: i64,
    ) -> Result<(), LockError>;
    /// Starts the continuation's sub-agent group in the claimed session.
    async fn initialize_continuation_group(
        &self,
        txn: &C,
        session: &mut PersistedAgentSession,
        now_ms: i64,
    ) -> Result<(), DbErr>;
    /// Current assistant-data limits for a first continuation claim.
    async fn assistant_data_limits(
        &self,
        txn: &C,
    ) -> Result<Box<desk_agent_protocol::ai_assistant::limits::AssistantDataLimits>, DbErr>;
    /// An unfinished long-term goal plans this conversation (OSS by `actor_id`,
    /// Manager by `owner_user_id`).
    async fn active_goal_conflict(
        &self,
        txn: &C,
        conversation: &str,
        owner: i32,
        device: &str,
    ) -> Result<bool, DbErr>;
    /// Inserts a task definition from an owner request or an AI proposal.
    async fn create_draft(
        &self,
        txn: &C,
        owner: i32,
        request: &desk_agent_protocol::schedule::ScheduleDraft,
        now_ms: i64,
        original: Option<&PersistedAgentSession>,
    ) -> Result<crate::entity::agent_schedule::Model, ScheduleStoreError>;
    /// Freezes the proposing turn's prompt retention on a proposed task.
    async fn record_prompt_retention(
        &self,
        txn: &C,
        task: &mut crate::entity::agent_schedule::Model,
        session: &PersistedAgentSession,
        parent: &desk_diagnose_core::chat::ChatMessage,
    ) -> Result<(), ScheduleStoreError>;
    /// Checks a model-control call against its recorded model egress.
    async fn validate_model_control(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        call: &desk_diagnose_core::chat::ToolCall,
    ) -> Result<(), DbErr>;
    /// Opens the approval review of a fresh-task proposal; `true` when it
    /// wrote an audit event that advanced the session row.
    async fn open_for_schedule_proposal(
        &self,
        txn: &C,
        row: &Self::Row,
        session: &mut PersistedAgentSession,
        now_ms: u64,
    ) -> Result<bool, DbErr>;
    /// Unfinished device actions on the conversation, locked for the caller.
    async fn has_open_actions(&self, txn: &C, conversation: &str) -> Result<bool, DbErr>;
    /// Closes the owner's pending permission decision for a cancelled wait.
    /// Every refusal, including a backend error, is a Conflict.
    async fn close_wait_decision(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        request_id: &str,
        now_ms: i64,
    ) -> Result<(), ScheduleStoreError>;
    /// The owner's prompt retention record for a fresh task's requirement.
    async fn owner_prompt_retention(
        &self,
        txn: &C,
        actor: &str,
        device: &str,
    ) -> Result<String, ScheduleStoreError>;
}

/// Milliseconds since the Unix epoch as a UTC timestamp.
pub fn timestamp(ms: i64) -> Result<chrono::DateTime<chrono::Utc>, ScheduleStoreError> {
    chrono::DateTime::from_timestamp_millis(ms).ok_or(ScheduleStoreError::Invalid)
}
