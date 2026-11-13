//! Contract of the session reads and writes the shared rules ask a host for.
use super::*;
use crate::schedule::test_host::{Call, Guard, Patch, Recorder};
use desk_agent_protocol::AgentScope;
use desk_diagnose_core::ai_assistant::scheduled_retrieval::TaskRetrievalConsent;
use sea_orm::Database;

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
    let host = Recorder::new(session(3));
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
    let host = Recorder::new(session(3));
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
