//! Contract of the session reads and writes the shared rules ask a host for.
use super::*;
use crate::schedule::session::{ScheduleLocks, SessionRow, SessionRows};
use desk_agent_protocol::AgentScope;
use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
use sea_orm::{Database, DatabaseConnection, DbErr};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Lock(String),
    Find(String, Option<String>, Option<String>, OrgMode),
    Touch(TouchColumn, Guard),
    Write(Guard, Patch),
}

/// Owned copies of the guard and patch a rule passed.
#[derive(Debug, Clone, PartialEq)]
struct Guard {
    id: Option<i64>,
    conversation: Option<String>,
    actor: Option<String>,
    device: Option<String>,
    version: Option<i64>,
    lease_token: Option<i64>,
    state_json: bool,
    org: OrgMode,
}

#[derive(Debug, Clone, PartialEq)]
struct Patch {
    state_json: bool,
    version: Option<i64>,
    lease_token: Option<i64>,
    lease_deadline_ms: Option<Option<i64>>,
    updated_at: bool,
    turn_state: Option<String>,
}

struct Recorder {
    session: PersistedAgentSession,
    calls: Mutex<Vec<Call>>,
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
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
        let SessionKey::Conversation {
            conversation,
            actor,
            device,
        } = key
        else {
            panic!("lookup by id")
        };
        self.calls.lock().unwrap().push(Call::Find(
            conversation.into(),
            actor.map(str::to_owned),
            device.map(str::to_owned),
            org,
        ));
        Ok(Some(SessionRow {
            raw: (),
            id: 11,
            conversation_id: conversation.into(),
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
    async fn directory_receipt(
        &self,
        _: &DatabaseConnection,
        _: &PersistedAgentSession,
        _: &str,
    ) -> Result<desk_diagnose_core::file_scope::transaction::FileScopeReceipt, ScheduleStoreError>
    {
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

fn scope() -> AgentScope {
    AgentScope {
        granted: vec![],
        mode: desk_agent_protocol::ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    }
}

fn session(input_revision: u64) -> PersistedAgentSession {
    let mut session = PersistedAgentSession::new("source", "1", "device", 1, scope(), "now");
    session.surface = AgentSessionSurface::AiAssistant;
    session.input_revision = input_revision;
    session
        .begin_focus_epoch(input_revision, Vec::new())
        .unwrap();
    session.version = 4;
    session
}

fn task(kind: &str, status: &str) -> entity::Model {
    entity::Model {
        id: 1,
        schedule_id: "task".into(),
        owner_user_id: 1,
        target_device_id: "device".into(),
        kind: kind.into(),
        status: status.into(),
        title: "t".into(),
        prompt: "p".into(),
        locale: None,
        model_id: None,
        spec_json: "{}".into(),
        calc_version: SCHEDULE_CALC_VERSION.into(),
        revision: 2,
        task_revision: 1,
        next_run_at: None,
        recurrence_cursor_at: None,
        grace_seconds: 300,
        creation_source: "owner".into(),
        creation_identity: "c".into(),
        creation_payload_sha256: "0".repeat(64),
        source_conversation_id: Some("source".into()),
        requirement_revision: Some(3),
        retrieval_consent_json: None,
        prompt_retention_json: None,
        proposal_conversation_id: None,
        failure_state_json: "{}".into(),
        active_run_id: None,
        created_at: 0,
        updated_at: 0,
    }
}

#[tokio::test]
async fn an_activation_receipt_writes_only_the_transcript_version_and_time() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    let host = Recorder {
        session: session(3),
        calls: Mutex::default(),
    };
    record_decision_on(&host, &db, &task("conversation_resume", "active"), None)
        .await
        .unwrap();
    // The CAS keys on the row id and version only and leaves the lease, its
    // deadline and turn state to their own writers.
    assert_eq!(
        host.calls(),
        [
            Call::Find("source".into(), None, None, OrgMode::PersonalOnly),
            Call::Write(
                Guard {
                    id: Some(11),
                    conversation: None,
                    actor: None,
                    device: None,
                    version: Some(4),
                    lease_token: None,
                    state_json: false,
                    org: OrgMode::Any,
                },
                Patch {
                    state_json: true,
                    version: Some(5),
                    lease_token: None,
                    lease_deadline_ms: None,
                    updated_at: true,
                    turn_state: None,
                },
            ),
        ]
    );
}

#[tokio::test]
async fn the_source_requirement_lock_fences_the_exact_envelope() {
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
    let mut task = task("conversation_resume", "active");
    task.retrieval_consent_json = Some(consent.encode().unwrap());
    let host = Recorder {
        session: session(3),
        calls: Mutex::default(),
    };
    let db = Database::connect("sqlite::memory:").await.unwrap();
    lock_original_requirement(&host, &db, 1, &task)
        .await
        .unwrap();
    assert_eq!(
        host.calls(),
        [
            Call::Find(
                "source".into(),
                Some("1".into()),
                Some("device".into()),
                OrgMode::PersonalOnly
            ),
            Call::Touch(
                TouchColumn::Version,
                Guard {
                    id: Some(11),
                    conversation: Some("source".into()),
                    actor: Some("1".into()),
                    device: Some("device".into()),
                    version: Some(4),
                    lease_token: None,
                    state_json: true,
                    org: OrgMode::Any,
                },
            ),
        ]
    );
}
