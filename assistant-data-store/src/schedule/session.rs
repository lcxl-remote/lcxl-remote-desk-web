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

impl<R> SessionRow<R> {
    /// Whether Manager's stored `turn_state` names `state`; OSS stores none.
    pub fn stored_turn_state_is(&self, state: desk_diagnose_core::session::TurnState) -> bool {
        self.turn_state
            .as_deref()
            .is_none_or(|stored| stored == state.as_str())
    }
}

/// Native delegated effects of a scheduled sub-agent group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeDisposition {
    Settled,
    Pending,
    Unknown,
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
    /// A proposal source: the target device or an unbound conversation.
    Source {
        conversation: &'a str,
        actor: &'a str,
        device: &'a str,
    },
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
    /// Adds `lease_deadline IS NULL`.
    pub lease_deadline_null: bool,
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
            lease_deadline_null: false,
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
    /// The class of a host action status.
    fn action_class(&self, status: &str) -> ActionClass;
    /// Host action row type.
    type Action: Clone + Send + Sync;
    /// Action rows of a conversation; `exclusive` locks every matching row.
    async fn find_actions(
        &self,
        txn: &C,
        conversation: &str,
        filter: ActionFilter<'_>,
        exclusive: bool,
    ) -> Result<Vec<ActionView<Self::Action>>, DbErr>;
    /// The first matching action row; `exclusive` locks only that row.
    async fn find_action(
        &self,
        txn: &C,
        conversation: &str,
        filter: ActionFilter<'_>,
        exclusive: bool,
    ) -> Result<Option<ActionView<Self::Action>>, DbErr>;
    /// OSS checks the dispatch outbox row and payload of a manual disposition;
    /// Manager has nothing to check. Mismatch: Conflict.
    async fn verify_manual_outbox(
        &self,
        txn: &C,
        action: &ActionView<Self::Action>,
        execution_id: &str,
        requested: bool,
    ) -> Result<(), ScheduleStoreError>;
    /// Marks an outcome-unknown action manually resolved (CAS on the raw
    /// unknown status); Manager also records the outbox event. `false` when
    /// the CAS matched no row.
    async fn resolve_unknown_action(
        &self,
        txn: &C,
        action: &ActionView<Self::Action>,
        at_ms: i64,
    ) -> Result<bool, DbErr>;
    /// Current task grants of the session's subject.
    async fn list_task_grants(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
    ) -> Result<Vec<desk_agent_protocol::capability_grant::CapabilityGrant>, DbErr>;
    /// Closes the owner's pending decision for an expiring fresh wait.
    async fn close_fresh_wait_decision(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        request_id: &str,
        now_ms: i64,
    ) -> Result<(), LockError>;
    /// Restores calls completed by the device into the run's session. OSS
    /// writes completion records here; the caller rolls back on `false`.
    async fn restore_completed_calls(
        &self,
        txn: &C,
        work: &crate::entity::agent_schedule_run::Model,
        session: &mut PersistedAgentSession,
        now_ms: i64,
    ) -> Result<bool, ScheduleStoreError>;
    /// Host native-history scope captured after a settlement's writes.
    type NativeScope: Send + Sync;
    async fn open_native_scope(
        &self,
        txn: &C,
        actor: &str,
        device: &str,
        root: &str,
    ) -> Result<Option<Box<Self::NativeScope>>, DbErr>;
    /// Runs after every original mutation; a history outage cannot discard them.
    async fn capture_native_scope(&self, txn: &C, scope: &Self::NativeScope) -> Result<(), DbErr>;
    /// Whether an action row is the one a session execution identity names.
    fn action_matches_identity(
        &self,
        action: &ActionView<Self::Action>,
        identity: &desk_diagnose_core::session::ActionIdentity,
    ) -> bool;
    /// OSS `agent_exec_task` rows that are not done and not the session's own
    /// awaited execution when `allow_result`; Manager has no such table.
    async fn has_open_exec_tasks(
        &self,
        txn: &C,
        conversation: &str,
        session: &PersistedAgentSession,
        allow_result: bool,
    ) -> Result<bool, DbErr>;
    /// Reconciles an interrupted continuation's actions into its session.
    async fn reconcile_scheduled_actions(
        &self,
        txn: &C,
        session: &mut PersistedAgentSession,
        now_ms: i64,
    ) -> Result<(), LockError>;
    /// Settles the continuation's scheduled permission decision.
    async fn settle_scheduled_decision(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        now_ms: i64,
    ) -> Result<(), LockError>;
    /// Settles a fresh occurrence's recorded owner decision.
    async fn settle_fresh_decision(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        request_id: &str,
        now_ms: i64,
    ) -> Result<(), LockError>;
    /// Manager task subject lock with its device policy check; no-op on OSS.
    async fn lock_task_subject(
        &self,
        txn: &C,
        owner: i32,
        device: &str,
        run_id: &str,
    ) -> Result<(), ScheduleStoreError>;
    /// Manager lock of the root's child runs; no-op on OSS.
    async fn lock_root_children(
        &self,
        txn: &C,
        root: &str,
        actor: &str,
        device: &str,
    ) -> Result<(), DbErr>;
    /// Native delegated effects of a scheduled sub-agent group.
    async fn scheduled_native(
        &self,
        txn: &C,
        group: &crate::entity::agent_delegation_group::Model,
    ) -> Result<NativeDisposition, DbErr>;
    /// Whether every required child of the session's group finished.
    async fn required_children_complete(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
    ) -> Result<bool, DbErr>;
    /// Confirms the fresh occurrence's pending owner permission request.
    async fn verify_fresh_wait_request(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
        request_id: &str,
    ) -> Result<(), LockError>;
    /// Decodes a delegation group's frozen creation envelope.
    fn decode_creation(
        &self,
        row: &crate::entity::agent_delegation_group::Model,
    ) -> Result<desk_diagnose_core::subagent::creation::CreationEnvelope, DbErr>;
    /// Prepares the root's ready child notification in its session.
    async fn prepare_notification(
        &self,
        txn: &C,
        session: &mut PersistedAgentSession,
    ) -> Result<bool, DbErr>;
    /// The group and source of the root's ready child notification.
    async fn ready_notification_source(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
    ) -> Result<
        Option<(
            desk_diagnose_core::subagent::group::DelegationGroup,
            desk_diagnose_core::subagent::creation::CreationEnvelope,
        )>,
        DbErr,
    >;
    /// The resolved wait, group and source of the root's child wait.
    async fn ready_wait_source(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
    ) -> Result<
        Option<(
            desk_diagnose_core::subagent::wait::ParentWait,
            desk_diagnose_core::subagent::group::DelegationGroup,
            desk_diagnose_core::subagent::creation::CreationEnvelope,
        )>,
        DbErr,
    >;
    /// Records the root's notification delivery attempt.
    async fn mark_notification_attempt(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
    ) -> Result<(), DbErr>;
    /// Closes a scheduled group's planning and native stop intents.
    async fn close_scheduled_group(
        &self,
        txn: &C,
        row: &crate::entity::agent_delegation_group::Model,
        now_ms: i64,
    ) -> Result<
        (
            usize,
            desk_diagnose_core::model_observability::permission::PendingEnds,
        ),
        DbErr,
    >;
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
    /// Manager by `owner_user_id`), on `device` when given.
    async fn active_goal_conflict(
        &self,
        txn: &C,
        conversation: &str,
        owner: i32,
        device: Option<&str>,
    ) -> Result<bool, DbErr>;
    /// Whether `owner` is an account of this host; OSS has a single account.
    fn account_owner(&self, owner: i32) -> bool;
    /// The read selection of the source conversation's accepted input.
    async fn original_read_context(
        &self,
        txn: &C,
        session: &PersistedAgentSession,
    ) -> Result<
        Option<desk_diagnose_core::input_read_context::ReadContextSelection>,
        ScheduleStoreError,
    >;
    /// The owner's current assistant instructions for a definition consent.
    async fn definition_instructions(
        &self,
        txn: &C,
        owner: i32,
    ) -> Result<
        desk_agent_protocol::ai_assistant::user_instructions::UserInstructions,
        crate::AssistantStoreError,
    >;
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

/// Whether the host commits or rolls back the caller's transaction after a
/// shared rule returns. Rules that hold a transaction never commit it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish<T> {
    Commit(T),
    Rollback(T),
}

impl<T> Finish<T> {
    pub fn into_inner(self) -> T {
        match self {
            Self::Commit(value) | Self::Rollback(value) => value,
        }
    }
}

/// Host action status reduced to the classes schedule rules decide on.
/// Each call site states which classes it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionClass {
    Succeeded,
    Failed,
    /// Superseded or revoked before intent (OSS); rejected, expired or
    /// cancelled (Manager).
    Withdrawn,
    /// OSS rows from the earlier action vocabulary: done, rejected, expired,
    /// cancelled. Finished for continuation results only.
    LegacyClosed,
    OutcomeUnknown,
    Dispatching,
    Open,
}

/// Host action row reduced to the fields schedule rules read. `raw` goes back
/// to host hooks.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionView<R> {
    pub raw: R,
    pub id: i64,
    pub kind: String,
    pub class: ActionClass,
    pub conversation_id: String,
    pub turn_id: String,
    pub actor_id: String,
    pub target_device_id: String,
    pub action_request_id: Option<String>,
    pub execution_id: Option<String>,
    pub tool_call_id: String,
    pub has_result: bool,
    pub is_side_effecting: bool,
    pub dispatch_intent_at_ms: Option<i64>,
    pub manual_resolved_at_ms: Option<i64>,
}

/// Conditions of an action lookup; the host translates them into SQL before
/// any row is read or locked.
#[derive(Debug, Clone, Copy, Default)]
pub struct ActionFilter<'a> {
    pub kind: Option<&'a str>,
    /// Leaves out rows of this class.
    pub exclude: Option<ActionClass>,
    /// Only rows without a recorded cancellation request.
    pub not_cancel_requested: bool,
}

#[cfg(test)]
mod stored_turn_state_tests {
    use super::*;
    use desk_diagnose_core::session::TurnState;

    fn row(turn_state: Option<&str>) -> SessionRow<()> {
        SessionRow {
            raw: (),
            id: 1,
            conversation_id: String::new(),
            actor_id: String::new(),
            device_id: String::new(),
            state_json: String::new(),
            version: 1,
            lease_token: 1,
            lease_deadline_ms: None,
            personal: true,
            turn_state: turn_state.map(str::to_owned),
        }
    }

    #[test]
    fn stored_turn_state_must_name_the_expected_state_when_present() {
        assert!(row(None).stored_turn_state_is(TurnState::Failed));
        assert!(row(Some("idle")).stored_turn_state_is(TurnState::Idle));
        assert!(!row(Some("running")).stored_turn_state_is(TurnState::Idle));
        assert!(!row(Some("idle")).stored_turn_state_is(TurnState::Failed));
    }
}
