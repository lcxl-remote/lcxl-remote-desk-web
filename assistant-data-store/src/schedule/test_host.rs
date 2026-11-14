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
    /// Lease columns of the recorded session row.
    pub(crate) lease_token: i64,
    pub(crate) lease_deadline_ms: Option<i64>,
    /// Manager `org_id IS NULL` of the recorded session row.
    pub(crate) personal: bool,
    /// Native delegated effects reported for any group.
    pub(crate) native: super::session::NativeDisposition,
    /// Classes of the conversation's action rows, in id order.
    pub(crate) actions: Vec<super::session::ActionClass>,
}

impl Recorder {
    fn action_views(
        &self,
        conversation: &str,
        filter: super::session::ActionFilter<'_>,
    ) -> Vec<super::session::ActionView<()>> {
        self.actions
            .iter()
            .enumerate()
            .filter(|(_, class)| filter.exclude != Some(**class))
            .map(|(index, class)| super::session::ActionView {
                raw: (),
                id: index as i64 + 1,
                kind: "action".into(),
                class: *class,
                conversation_id: conversation.into(),
                turn_id: "turn".into(),
                actor_id: "1".into(),
                target_device_id: "device".into(),
                action_request_id: None,
                execution_id: None,
                tool_call_id: format!("call-{index}"),
                has_result: false,
                is_side_effecting: false,
                dispatch_intent_at_ms: None,
                manual_resolved_at_ms: None,
            })
            .collect()
    }

    pub(crate) fn new(session: PersistedAgentSession) -> Self {
        Self {
            session,
            calls: Mutex::default(),
            lease_token: 0,
            lease_deadline_ms: None,
            personal: true,
            native: super::session::NativeDisposition::Settled,
            actions: Vec::new(),
        }
    }

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
            lease_token: self.lease_token,
            lease_deadline_ms: self.lease_deadline_ms,
            personal: self.personal,
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
        conversation: &str,
        filter: crate::schedule::session::ActionFilter<'_>,
        _: bool,
    ) -> Result<Vec<crate::schedule::session::ActionView<()>>, DbErr> {
        Ok(self.action_views(conversation, filter))
    }
    async fn find_action(
        &self,
        _: &DatabaseConnection,
        conversation: &str,
        filter: crate::schedule::session::ActionFilter<'_>,
        _: bool,
    ) -> Result<Option<crate::schedule::session::ActionView<()>>, DbErr> {
        Ok(self.action_views(conversation, filter).into_iter().next())
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
        Ok(())
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
        Ok(self.native)
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
        Ok(())
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
        Ok(false)
    }
    async fn close_wait_decision(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
        _: i64,
    ) -> Result<(), ScheduleStoreError> {
        Ok(())
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

/// A continuation task and its occurrence `run` at `status`, frozen with a
/// source-session consent on device `device` and requirement revision 3.
pub(crate) async fn continuation_rows(
    db: &DatabaseConnection,
    status: &str,
    lease_deadline: Option<i64>,
    cancel_requested: bool,
    result_ref: Option<&str>,
) {
    use crate::entity::agent_schedule_run as run;
    use crate::entity_table::EntityTableExt;
    use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
    use sea_orm::{ConnectionTrait, EntityTrait, Schema, Set};
    let schema = Schema::new(db.get_database_backend());
    for statement in [
        schema.entity_table(entity::Entity),
        schema.entity_table(run::Entity),
    ] {
        db.execute(&statement).await.unwrap();
    }
    let mut consent = TaskRetrievalConsent::capture_definition(
        "task",
        1,
        "1",
        "device",
        &Default::default(),
        "UTC",
        1,
    )
    .unwrap();
    consent.source_conversation_id = Some("source".into());
    consent.source_input_revision = Some(3);
    consent.assistant_seed = None;
    let task = entity::Model {
        id: 1,
        schedule_id: "task".into(),
        owner_user_id: 1,
        target_device_id: "device".into(),
        kind: "conversation_resume".into(),
        status: "active".into(),
        title: "t".into(),
        prompt: "p".into(),
        locale: None,
        model_id: None,
        spec_json: "{}".into(),
        calc_version: desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION.into(),
        revision: 2,
        task_revision: 1,
        next_run_at: None,
        recurrence_cursor_at: None,
        grace_seconds: 300,
        creation_source: "manual".into(),
        creation_identity: "c".into(),
        creation_payload_sha256: "0".repeat(64),
        source_conversation_id: Some("source".into()),
        requirement_revision: Some(3),
        retrieval_consent_json: Some(consent.encode().unwrap()),
        prompt_retention_json: None,
        proposal_conversation_id: None,
        failure_state_json: serde_json::to_string(
            &desk_diagnose_core::schedule::lifecycle::FailureState::default(),
        )
        .unwrap(),
        active_run_id: Some("run".into()),
        created_at: 0,
        updated_at: 0,
    };
    let snapshot = serde_json::to_string(&task).unwrap();
    let task: entity::ActiveModel = task.into();
    entity::Entity::insert(task).exec(db).await.unwrap();
    run::Entity::insert(run::ActiveModel {
        run_id: Set("run".into()),
        schedule_id: Set("task".into()),
        owner_user_id: Set(1),
        occurrence_identity: Set("occurrence".into()),
        source: Set("calendar".into()),
        requested_at: Set(0),
        schedule_revision: Set(2),
        recovery_epoch: Set(1),
        task_snapshot_json: Set(snapshot),
        conversation_id: Set("source".into()),
        turn_id: Set("turn".into()),
        status: Set(status.into()),
        start_deadline: Set(i64::MAX),
        lease_epoch: Set(1),
        lease_owner: Set(Some("node".into())),
        lease_deadline: Set(lease_deadline),
        attempt: Set(1),
        failure_accounted: Set(false),
        result_ref: Set(result_ref.map(str::to_owned)),
        missed_count: Set(0),
        started_at: Set(Some(1)),
        cancel_requested_at: Set(cancel_requested.then_some(1)),
        created_at: Set(0),
        updated_at: Set(0),
        ..Default::default()
    })
    .exec(db)
    .await
    .unwrap();
}

/// The continuation's source session at requirement revision 3.
pub(crate) fn continuation_session() -> PersistedAgentSession {
    use desk_diagnose_core::session::{AgentSessionSurface, TriggerOrigin};
    let mut session = PersistedAgentSession::new(
        "source",
        "1",
        "device",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-13T00:00:00Z",
    );
    session.surface = AgentSessionSurface::AiAssistant;
    session.input_revision = 3;
    session.begin_focus_epoch(3, Vec::new()).unwrap();
    session.version = 4;
    session.lease_token = 7;
    session.trigger_origin = TriggerOrigin::ScheduledContinuation;
    session.current_turn_id = Some("turn".into());
    session.current_request_id = Some("run".into());
    session
}

/// A fresh task whose occurrence `run` is still running after its lease.
pub(crate) async fn fresh_rows(db: &DatabaseConnection, lease_deadline: i64) {
    use crate::entity::agent_schedule_run as run;
    use crate::entity_table::EntityTableExt;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema, Set};
    let schema = Schema::new(db.get_database_backend());
    for statement in [
        schema.entity_table(entity::Entity),
        schema.entity_table(run::Entity),
    ] {
        db.execute(&statement).await.unwrap();
    }
    let task = entity::ActiveModel {
        schedule_id: Set("task".into()),
        owner_user_id: Set(1),
        target_device_id: Set("device".into()),
        kind: Set("fresh_task".into()),
        status: Set("active".into()),
        title: Set("t".into()),
        prompt: Set("p".into()),
        spec_json: Set("{}".into()),
        calc_version: Set(desk_diagnose_core::schedule::SCHEDULE_CALC_VERSION.into()),
        revision: Set(2),
        task_revision: Set(1),
        grace_seconds: Set(300),
        creation_source: Set("manual".into()),
        creation_identity: Set("c".into()),
        creation_payload_sha256: Set("0".repeat(64)),
        failure_state_json: Set("{}".into()),
        active_run_id: Set(Some("run".into())),
        created_at: Set(0),
        updated_at: Set(0),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    run::ActiveModel {
        run_id: Set("run".into()),
        schedule_id: Set("task".into()),
        owner_user_id: Set(1),
        occurrence_identity: Set("occurrence".into()),
        source: Set("calendar".into()),
        requested_at: Set(0),
        schedule_revision: Set(2),
        recovery_epoch: Set(1),
        task_snapshot_json: Set(serde_json::to_string(&task).unwrap()),
        conversation_id: Set("run".into()),
        turn_id: Set("run-turn".into()),
        status: Set("running".into()),
        start_deadline: Set(i64::MAX),
        lease_epoch: Set(1),
        lease_owner: Set(Some("node".into())),
        lease_deadline: Set(Some(lease_deadline)),
        attempt: Set(1),
        failure_accounted: Set(false),
        missed_count: Set(0),
        started_at: Set(Some(1)),
        created_at: Set(0),
        updated_at: Set(0),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

/// The fresh occurrence's delegation group `group` with no child runs.
pub(crate) async fn fresh_group(db: &DatabaseConnection) {
    use crate::entity::{agent_delegation_group as group, agent_subagent_run as child};
    use crate::entity_table::EntityTableExt;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Schema, Set};
    let schema = Schema::new(db.get_database_backend());
    for statement in [
        schema.entity_table(group::Entity),
        schema.entity_table(child::Entity),
    ] {
        db.execute(&statement).await.unwrap();
    }
    group::ActiveModel {
        group_id: Set("group".into()),
        root_conversation_id: Set("run".into()),
        actor_id: Set("1".into()),
        device_id: Set("device".into()),
        source_schedule_id: Set(Some("task".into())),
        source_occurrence_id: Set(Some("run".into())),
        source_admission: Set("open".into()),
        source_epoch: Set(1),
        parent_input_revision: Set(1),
        parent_control_revision: Set(1),
        parent_active: Set(true),
        state_json: Set("{}".into()),
        creation_envelope_json: Set("{}".into()),
        model_binding_json: Set("{}".into()),
        version: Set(1),
        deadline_ms: Set(i64::MAX),
        created_at: Set(0),
        updated_at: Set(0),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}
