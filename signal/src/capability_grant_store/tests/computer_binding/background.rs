use super::*;
use crate::{agent_session_store::SignalAgentSessionStore, entity::agent_exec_task};
use desk_diagnose_core::{dynamic_run::BackgroundTaskState, seam::WaitOutcome};

mod egress;

async fn age(f: &Fixture, seconds: i64) {
    let mut row: agent_capability_dispatch_outbox::ActiveModel = f.outbox().await.into();
    row.created_at = Set(Utc::now() - chrono::Duration::seconds(seconds));
    row.update(&f.store.db).await.unwrap();
}

async fn promote(f: &Fixture) -> bool {
    f.store
        .promote_computer_background(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
        .await
        .unwrap()
}

async fn prepare_delivery(f: &Fixture) {
    let schema = Schema::new(f.store.db.get_database_backend());
    f.store
        .db
        .execute(&schema.create_table_from_entity(agent_exec_task::Entity))
        .await
        .unwrap();
    let row = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut state = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    // The production publisher prunes an expired follow-up without calling a model.
    state.scope_snapshot.expires_at =
        Some((Utc::now() - chrono::Duration::seconds(1)).to_rfc3339());
    let mut row: agent_session::ActiveModel = row.into();
    row.state_json = Set(state.encode_json_for_storage().unwrap());
    row.update(&f.store.db).await.unwrap();
}

#[tokio::test]
async fn original_background_requires_acceptance_and_budget_without_another_work_or_grant_use() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("background.db")).await).await;
    f.bind().await;
    assert!(!promote(&f).await);
    f.accept().await.unwrap();
    assert!(!promote(&f).await);
    age(&f, 9).await;
    let work_before = agent_action_item::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(promote(&f).await);
    let first = f.outbox().await;
    assert!(first.computer_background_json.is_some());
    assert!(promote(&f).await);
    assert_eq!(first, f.outbox().await);
    assert_eq!(
        work_before,
        agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        agent_action_item::Entity::find()
            .count(&f.store.db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        agent_capability_grant::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
            .remaining_uses,
        0
    );
    assert!(matches!(
        f.store
            .wait_computer_result(
                &f.plan.action_request_id,
                &f.plan.execution_generation,
                "run-1",
                "actor-1",
                "device-1"
            )
            .await
            .unwrap(),
        Some(WaitOutcome::StillRunning)
    ));
    assert!(
        f.store
            .promote_computer_background(
                &f.plan.execution_generation,
                "other",
                "actor-1",
                "device-1"
            )
            .await
            .is_err()
    );
    assert!(
        f.store
            .claim_dispatch(
                &f.plan.execution_generation,
                Utc::now().timestamp_millis() as u64
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn owner_snapshot_advances_on_background_and_result_without_invalidating_execution_cas() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("snapshot.db")).await).await;
    f.bind().await;
    f.accept().await.unwrap();
    let sessions = SignalAgentSessionStore::new(f.store.db.clone());
    let before = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let first = sessions
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert!(first.background_tasks.is_empty());
    assert!(
        sessions
            .read_assistant_snapshot_for_subject("run-1", "other", "device-1")
            .await
            .unwrap()
            .is_none()
    );
    age(&f, 9).await;
    assert!(promote(&f).await);
    let running = sessions
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert!(running.session.seq > first.session.seq);
    assert_eq!(running.background_tasks.len(), 1);
    assert_eq!(running.background_tasks[0].task.call_id, f.call.id);
    assert_eq!(
        running.background_tasks[0].state,
        BackgroundTaskState::Running
    );
    assert!(running.background_tasks[0].supports_cancel);
    let again = sessions
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.session.seq, running.session.seq);
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            &completion::failed(&f.plan),
        )
        .await
        .unwrap();
    let done = sessions
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert!(done.session.seq > running.session.seq);
    assert_eq!(done.background_tasks[0].state, BackgroundTaskState::Failed);
    let after = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            before.version,
            before.lease_token,
            before.state_json,
            before.updated_at
        ),
        (
            after.version,
            after.lease_token,
            after.state_json,
            after.updated_at
        )
    );
}

#[tokio::test]
async fn original_publisher_recovers_crash_preserves_receipt_and_consumes_only_after_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("publisher.db");
    let f = Fixture::new(file_db(&path).await).await;
    f.bind().await;
    f.accept().await.unwrap();
    age(&f, 9).await;
    assert!(promote(&f).await);
    prepare_delivery(&f).await;
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            &completion::verified(&f.plan),
        )
        .await
        .unwrap();
    let original = f
        .store
        .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
        .await
        .unwrap();
    let store = SignalCapabilityGrantStore::new(reopened.clone());
    reopened.execute_unprepared("CREATE TRIGGER refuse_completion BEFORE UPDATE ON agent_session BEGIN SELECT RAISE(ABORT, 'synthetic save failure'); END").await.unwrap();
    store.publish_computer_results_once().await.unwrap();
    assert_eq!(
        agent_action_item::Entity::find()
            .one(&reopened)
            .await
            .unwrap()
            .unwrap()
            .completion_delivery_state,
        "pending"
    );
    reopened
        .execute_unprepared("DROP TRIGGER refuse_completion")
        .await
        .unwrap();
    store.publish_computer_results_once().await.unwrap();
    let row = agent_session::Entity::find()
        .one(&reopened)
        .await
        .unwrap()
        .unwrap();
    let state = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(state.version, row.version);
    assert!(state.execution_state.waitable_task().is_none());
    assert!(state.unclosed_tool_call_ids().is_empty());
    assert!(state.pending_auto_triggers.is_empty());
    let message = state
        .conversation
        .iter()
        .find(|message| message.message_id == original.work.completion_event_id)
        .unwrap();
    assert_eq!(message.text, original.output.content);
    assert_eq!(message.tool_call_id.as_deref(), Some(f.call.id.as_str()));
    assert_eq!(
        message.data_envelope.as_ref(),
        Some(&original.receipt.envelope)
    );
    assert_eq!(
        agent_action_item::Entity::find()
            .one(&reopened)
            .await
            .unwrap()
            .unwrap()
            .completion_delivery_state,
        "consumed"
    );
    store.publish_computer_results_once().await.unwrap();
    assert_eq!(
        row,
        agent_session::Entity::find()
            .one(&reopened)
            .await
            .unwrap()
            .unwrap()
    );
}

#[tokio::test]
async fn pending_receipt_after_a_full_invalid_page_is_not_starved() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("paged-publisher.db")).await).await;
    f.bind().await;
    f.accept().await.unwrap();
    age(&f, 9).await;
    assert!(promote(&f).await);
    prepare_delivery(&f).await;
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            &completion::verified(&f.plan),
        )
        .await
        .unwrap();
    let original = agent_action_item::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    // Negative synthetic keys place corrupt legacy rows before the valid row
    // without changing its frozen original identity or any transport binding.
    for index in 1..=129 {
        let mut invalid: agent_action_item::ActiveModel = original.clone().into();
        invalid = invalid.reset_all();
        invalid.id = Set(-index);
        invalid.action_request_id = Set(format!("invalid-{index}"));
        invalid.exec_request_id = Set(None);
        invalid.claim_token = Set(None);
        invalid.completion_event_id = Set(format!("invalid-event-{index}"));
        invalid.insert(&f.store.db).await.unwrap();
    }
    f.store.publish_computer_results_once().await.unwrap();
    assert_eq!(
        agent_action_item::Entity::find_by_id(original.id)
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
            .completion_delivery_state,
        "consumed"
    );
    assert_eq!(
        agent_action_item::Entity::find()
            .filter(agent_action_item::Column::Id.lt(0))
            .filter(agent_action_item::Column::CompletionDeliveryState.eq("pending"))
            .count(&f.store.db)
            .await
            .unwrap(),
        129
    );
}

#[tokio::test]
async fn independent_snapshot_connections_converge_without_changing_the_runtime_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("concurrent-snapshots.db");
    let f = Fixture::new(file_db(&path).await).await;
    f.bind().await;
    f.accept().await.unwrap();
    age(&f, 9).await;
    assert!(promote(&f).await);
    let before = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut jobs = Vec::new();
    for _ in 0..8 {
        let db = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        jobs.push(tokio::spawn(async move {
            let snapshot = SignalAgentSessionStore::new(db.clone())
                .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
                .await
                .unwrap()
                .unwrap();
            db.close().await.unwrap();
            assert_eq!(
                snapshot.background_tasks[0].state,
                BackgroundTaskState::Running
            );
            snapshot.session.seq
        }));
    }
    let mut sequences = Vec::new();
    for job in jobs {
        sequences.push(job.await.unwrap());
    }
    assert!(sequences.iter().all(|seq| *seq == sequences[0]));
    let after = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.state_json, after.state_json);
    assert_eq!(before.version, after.version);
    assert_eq!(before.lease_token, after.lease_token);
    assert_eq!(before.lease_deadline, after.lease_deadline);
    assert_eq!(before.updated_at, after.updated_at);
}

#[tokio::test]
async fn expired_background_becomes_reconcilable_unknown_then_accepts_original_late_failure() {
    use desk_diagnose_core::{
        chat::ChatMessage,
        session::{ActionIdentity, ExecutionState, TurnState, WorkKind},
    };
    let dir = tempfile::tempdir().unwrap();
    let mut f = Fixture::new(file_db(&dir.path().join("expiry.db")).await).await;
    f.plan.timeout_ms = 10_000;
    f.bind().await;
    f.accept().await.unwrap();
    age(&f, 9).await;
    assert!(promote(&f).await);
    prepare_delivery(&f).await;
    let row = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.conversation.push(ChatMessage::tool_result(
        "running",
        &f.call.id,
        "background action is running",
    ));
    session.execution_state = ExecutionState::Executing {
        action: ActionIdentity::new(
            f.plan.work_id.parse().unwrap(),
            &f.plan.action_request_id,
            &f.plan.execution_generation,
            WorkKind::ComputerAction,
        ),
    };
    session.finish_turn(TurnState::Idle, Utc::now().to_rfc3339());
    let mut active: agent_session::ActiveModel = row.into();
    active.state_json = Set(session.encode_json_for_storage().unwrap());
    active.update(&f.store.db).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    f.store.publish_computer_results_once().await.unwrap();
    let unknown_row = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let unknown = PersistedAgentSession::decode_json(&unknown_row.state_json).unwrap();
    assert!(matches!(
        unknown.execution_state,
        ExecutionState::OutcomeUnknown { .. }
    ));
    let message = unknown
        .conversation
        .iter()
        .find(|message| message.message_id == "running")
        .unwrap();
    assert_eq!(
        message.data_envelope.as_ref().unwrap().digest_sha256,
        format!("{:x}", Sha256::digest(message.text.as_bytes()))
    );
    let sessions = SignalAgentSessionStore::new(f.store.db.clone());
    let snapshot = sessions
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.background_tasks[0].state,
        BackgroundTaskState::OutcomeUnknown
    );
    assert!(snapshot.session.unresolved_action.is_some());
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            &completion::failed(&f.plan),
        )
        .await
        .unwrap();
    f.store.publish_computer_results_once().await.unwrap();
    let done = sessions
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.background_tasks[0].state, BackgroundTaskState::Failed);
    assert!(done.session.seq > snapshot.session.seq);
    assert!(done.session.unresolved_action.is_none());
    assert_eq!(
        agent_action_item::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
            .completion_delivery_state,
        "consumed"
    );
}
