//! A recording host for contract tests of the session reads, locks and writes
//! the shared rules ask a host for.
use super::{
    ScheduleStoreError,
    session::{
        OrgMode, ScheduleHostStores, ScheduleLocks, SessionGuard, SessionKey, SessionPatch,
        SessionRow, SessionRows, TouchColumn,
    },
};
use crate::entity::agent_schedule as entity;
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::{DatabaseConnection, DbErr};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Call {
    Lock(String),
    Find(String, Option<String>, Option<String>, OrgMode),
    FindSource(String, String, String, OrgMode),
    FindId(i64, OrgMode),
    Touch(TouchColumn, Guard),
    Write(Guard, Patch),
}

/// Owned copies of the guard and patch a rule passed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Guard {
    pub(crate) id: Option<i64>,
    pub(crate) conversation: Option<String>,
    pub(crate) actor: Option<String>,
    pub(crate) device: Option<String>,
    pub(crate) version: Option<i64>,
    pub(crate) lease_token: Option<i64>,
    pub(crate) state_json: bool,
    pub(crate) org: OrgMode,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Patch {
    pub(crate) state_json: bool,
    pub(crate) version: Option<i64>,
    pub(crate) lease_token: Option<i64>,
    pub(crate) lease_deadline_ms: Option<Option<i64>>,
    pub(crate) updated_at: bool,
    pub(crate) turn_state: Option<String>,
}

pub(crate) struct Recorder {
    pub(crate) session: PersistedAgentSession,
    pub(crate) calls: Mutex<Vec<Call>>,
}

impl Recorder {
    pub(crate) fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

fn guard(guard: &SessionGuard<'_>) -> Guard {
    Guard {
        id: guard.id,
        conversation: guard.conversation.map(str::to_owned),
        actor: guard.actor.map(str::to_owned),
        device: guard.device.map(str::to_owned),
        version: guard.version,
        lease_token: guard.lease_token,
        state_json: guard.state_json.is_some(),
        org: guard.org,
    }
}

#[async_trait::async_trait]
impl ScheduleLocks<DatabaseConnection> for Recorder {
    async fn lock_conversation(
        &self,
        _: &DatabaseConnection,
        conversation: &str,
    ) -> Result<(), DbErr> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Lock(conversation.into()));
        Ok(())
    }
}

#[async_trait::async_trait]
impl SessionRows<DatabaseConnection> for Recorder {
    type Row = ();
    async fn find_session(
        &self,
        _: &DatabaseConnection,
        key: SessionKey<'_>,
        org: OrgMode,
        _: bool,
    ) -> Result<Option<SessionRow<()>>, DbErr> {
        let (call, conversation) = match key {
            SessionKey::Conversation {
                conversation,
                actor,
                device,
            } => (
                Call::Find(
                    conversation.into(),
                    actor.map(str::to_owned),
                    device.map(str::to_owned),
                    org,
                ),
                conversation.to_owned(),
            ),
            SessionKey::Source {
                conversation,
                actor,
                device,
            } => (
                Call::FindSource(conversation.into(), actor.into(), device.into(), org),
                conversation.to_owned(),
            ),
            SessionKey::Id(id) => (Call::FindId(id, org), self.session.conversation_id.clone()),
        };
        self.calls.lock().unwrap().push(call);
        Ok(Some(SessionRow {
            raw: (),
            id: 11,
            conversation_id: conversation,
            actor_id: "1".into(),
            device_id: "device".into(),
            state_json: self.session.encode_json_for_storage().unwrap(),
            version: self.session.version,
            lease_token: 0,
            lease_deadline_ms: None,
            personal: true,
            turn_state: None,
        }))
    }
    async fn touch_session(
        &self,
        _: &DatabaseConnection,
        column: TouchColumn,
        g: SessionGuard<'_>,
    ) -> Result<u64, DbErr> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Touch(column, guard(&g)));
        Ok(1)
    }
    async fn write_session(
        &self,
        _: &DatabaseConnection,
        g: SessionGuard<'_>,
        patch: SessionPatch<'_>,
    ) -> Result<u64, DbErr> {
        self.calls.lock().unwrap().push(Call::Write(
            guard(&g),
            Patch {
                state_json: patch.state_json.is_some(),
                version: patch.version,
                lease_token: patch.lease_token,
                lease_deadline_ms: patch.lease_deadline_ms,
                updated_at: patch.updated_at_ms.is_some(),
                turn_state: patch.turn_state.map(str::to_owned),
            },
        ));
        Ok(1)
    }
}

#[async_trait::async_trait]
impl ScheduleHostStores<DatabaseConnection> for Recorder {
    fn action_class(&self, _: &str) -> crate::schedule::session::ActionClass {
        unreachable!()
    }
    type Action = ();
    type NativeScope = ();
    async fn find_actions(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: crate::schedule::session::ActionFilter<'_>,
        _: bool,
    ) -> Result<Vec<crate::schedule::session::ActionView<()>>, DbErr> {
        unreachable!()
    }
    async fn find_action(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: crate::schedule::session::ActionFilter<'_>,
        _: bool,
    ) -> Result<Option<crate::schedule::session::ActionView<()>>, DbErr> {
        unreachable!()
    }
    async fn verify_manual_outbox(
        &self,
        _: &DatabaseConnection,
        _: &crate::schedule::session::ActionView<()>,
        _: &str,
        _: bool,
    ) -> Result<(), ScheduleStoreError> {
        unreachable!()
    }
    async fn resolve_unknown_action(
        &self,
        _: &DatabaseConnection,
        _: &crate::schedule::session::ActionView<()>,
        _: i64,
    ) -> Result<bool, DbErr> {
        unreachable!()
    }
    async fn list_task_grants(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
    ) -> Result<Vec<desk_agent_protocol::capability_grant::CapabilityGrant>, DbErr> {
        unreachable!()
    }
    async fn close_fresh_wait_decision(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
        _: i64,
    ) -> Result<(), crate::schedule::session::LockError> {
        unreachable!()
    }
    async fn restore_completed_calls(
        &self,
        _: &DatabaseConnection,
        _: &crate::entity::agent_schedule_run::Model,
        _: &mut PersistedAgentSession,
        _: i64,
    ) -> Result<bool, ScheduleStoreError> {
        unreachable!()
    }
    async fn open_native_scope(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<Box<()>>, DbErr> {
        unreachable!()
    }
    async fn capture_native_scope(&self, _: &DatabaseConnection, _: &()) -> Result<(), DbErr> {
        unreachable!()
    }
    fn action_matches_identity(
        &self,
        _: &crate::schedule::session::ActionView<()>,
        _: &desk_diagnose_core::session::ActionIdentity,
    ) -> bool {
        unreachable!()
    }
    async fn has_open_exec_tasks(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: &PersistedAgentSession,
        _: bool,
    ) -> Result<bool, DbErr> {
        unreachable!()
    }
    async fn reconcile_scheduled_actions(
        &self,
        _: &DatabaseConnection,
        _: &mut PersistedAgentSession,
        _: i64,
    ) -> Result<(), crate::schedule::session::LockError> {
        unreachable!()
    }
    async fn settle_scheduled_decision(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: i64,
    ) -> Result<(), crate::schedule::session::LockError> {
        unreachable!()
    }
    async fn settle_fresh_decision(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
        _: i64,
    ) -> Result<(), crate::schedule::session::LockError> {
        unreachable!()
    }
    async fn lock_task_subject(
        &self,
        _: &DatabaseConnection,
        _: i32,
        _: &str,
        _: &str,
    ) -> Result<(), ScheduleStoreError> {
        unreachable!()
    }
    async fn lock_root_children(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), DbErr> {
        unreachable!()
    }
    async fn scheduled_native(
        &self,
        _: &DatabaseConnection,
        _: &crate::entity::agent_delegation_group::Model,
    ) -> Result<crate::schedule::session::NativeDisposition, DbErr> {
        unreachable!()
    }
    async fn required_children_complete(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
    ) -> Result<bool, DbErr> {
        unreachable!()
    }
    async fn verify_fresh_wait_request(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
    ) -> Result<(), crate::schedule::session::LockError> {
        unreachable!()
    }
    fn decode_group(
        &self,
        _: &crate::entity::agent_delegation_group::Model,
    ) -> Result<desk_diagnose_core::subagent::group::DelegationGroup, DbErr> {
        unreachable!()
    }
    async fn group_children(
        &self,
        _: &DatabaseConnection,
        _: &desk_diagnose_core::subagent::group::DelegationGroup,
    ) -> Result<Vec<crate::entity::agent_subagent_run::Model>, DbErr> {
        unreachable!()
    }
    fn decode_creation(
        &self,
        _: &crate::entity::agent_delegation_group::Model,
    ) -> Result<desk_diagnose_core::subagent::creation::CreationEnvelope, DbErr> {
        unreachable!()
    }
    async fn prepare_notification(
        &self,
        _: &DatabaseConnection,
        _: &mut PersistedAgentSession,
    ) -> Result<bool, DbErr> {
        unreachable!()
    }
    async fn ready_notification_source(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
    ) -> Result<
        Option<(
            desk_diagnose_core::subagent::group::DelegationGroup,
            desk_diagnose_core::subagent::creation::CreationEnvelope,
        )>,
        DbErr,
    > {
        unreachable!()
    }
    async fn ready_wait_source(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
    ) -> Result<
        Option<(
            desk_diagnose_core::subagent::wait::ParentWait,
            desk_diagnose_core::subagent::group::DelegationGroup,
            desk_diagnose_core::subagent::creation::CreationEnvelope,
        )>,
        DbErr,
    > {
        unreachable!()
    }
    async fn mark_notification_attempt(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
    ) -> Result<(), DbErr> {
        unreachable!()
    }
    async fn close_scheduled_group(
        &self,
        _: &DatabaseConnection,
        _: &crate::entity::agent_delegation_group::Model,
        _: i64,
    ) -> Result<
        (
            usize,
            desk_diagnose_core::model_observability::permission::PendingEnds,
        ),
        DbErr,
    > {
        unreachable!()
    }
    async fn directory_receipt(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
    ) -> Result<desk_diagnose_core::file_scope::transaction::FileScopeReceipt, ScheduleStoreError>
    {
        unreachable!()
    }
    async fn lock_child_source(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
    ) -> Result<(), DbErr> {
        unreachable!()
    }
    async fn owns_device(&self, _: &DatabaseConnection, _: i32, _: &str) -> Result<bool, DbErr> {
        unreachable!()
    }
    async fn has_open_actions(&self, _: &DatabaseConnection, _: &str) -> Result<bool, DbErr> {
        unreachable!()
    }
    async fn close_wait_decision(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
        _: i64,
    ) -> Result<(), ScheduleStoreError> {
        unreachable!()
    }
    async fn lock_permission_owner(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), crate::schedule::session::LockError> {
        unreachable!()
    }

    async fn consume_scheduled_decision(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
        _: &[desk_agent_protocol::capability_grant::CapabilityGrant],
        _: i64,
    ) -> Result<(), crate::schedule::session::LockError> {
        unreachable!()
    }
    async fn initialize_continuation_group(
        &self,
        _: &DatabaseConnection,
        _: &mut PersistedAgentSession,
        _: i64,
    ) -> Result<(), DbErr> {
        unreachable!()
    }
    async fn assistant_data_limits(
        &self,
        _: &DatabaseConnection,
    ) -> Result<Box<desk_agent_protocol::ai_assistant::limits::AssistantDataLimits>, DbErr> {
        unreachable!()
    }
    async fn active_goal_conflict(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: i32,
        _: Option<&str>,
    ) -> Result<bool, DbErr> {
        unreachable!()
    }
    fn account_owner(&self, _: i32) -> bool {
        true
    }
    async fn original_read_context(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
    ) -> Result<
        Option<desk_diagnose_core::input_read_context::ReadContextSelection>,
        ScheduleStoreError,
    > {
        unreachable!()
    }
    async fn definition_instructions(
        &self,
        _: &DatabaseConnection,
        _: i32,
    ) -> Result<
        desk_agent_protocol::ai_assistant::user_instructions::UserInstructions,
        crate::AssistantStoreError,
    > {
        unreachable!()
    }
    async fn record_prompt_retention(
        &self,
        _: &DatabaseConnection,
        _: &mut entity::Model,
        _: &PersistedAgentSession,
        _: &desk_diagnose_core::chat::ChatMessage,
    ) -> Result<(), ScheduleStoreError> {
        unreachable!()
    }
    async fn validate_model_control(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &desk_diagnose_core::chat::ToolCall,
    ) -> Result<(), DbErr> {
        unreachable!()
    }
    async fn open_for_schedule_proposal(
        &self,
        _: &DatabaseConnection,
        _: &(),
        _: &mut PersistedAgentSession,
        _: u64,
    ) -> Result<bool, DbErr> {
        unreachable!()
    }
    async fn check_resume_retention(
        &self,
        _: &DatabaseConnection,
        _: &desk_agent_protocol::schedule::ScheduleSpec,
        _: i64,
    ) -> Result<(), ScheduleStoreError> {
        Ok(())
    }
    async fn owner_prompt_retention(
        &self,
        _: &DatabaseConnection,
        _: &str,
        _: &str,
    ) -> Result<String, ScheduleStoreError> {
        unreachable!()
    }
}
