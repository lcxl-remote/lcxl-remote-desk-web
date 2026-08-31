use super::*;
use crate::agent_session_store::SignalAgentSessionStore;
use crate::capability_grant_store::computer_cancel::wire_request_id;
use desk_agent_protocol::computer_use::{ComputerActionPhase, ComputerActionStateReport};
use desk_diagnose_core::dynamic_run::{BackgroundTaskRecord, BackgroundTaskState};

mod wire;

async fn reopen(path: &std::path::Path) -> DatabaseConnection {
    Database::connect(format!("sqlite://{}?mode=rw", path.display()))
        .await
        .unwrap()
}

async fn ready(f: &Fixture) {
    f.bind().await;
    f.accept().await.unwrap();
    let mut outbox: agent_capability_dispatch_outbox::ActiveModel = f.outbox().await.into();
    outbox.created_at = Set(Utc::now() - chrono::Duration::seconds(9));
    outbox.update(&f.store.db).await.unwrap();
    assert!(
        f.store
            .promote_computer_background(
                &f.plan.execution_generation,
                "run-1",
                &f.plan.approved_actor_id,
                "device-1"
            )
            .await
            .unwrap()
    );
}

async fn request(
    f: &Fixture,
    id: &str,
    reason: &str,
) -> Result<Option<BackgroundTaskRecord>, DbErr> {
    f.store
        .request_computer_background_cancel(
            &f.plan.action_request_id,
            "run-1",
            &f.plan.approved_actor_id,
            "device-1",
            id,
            reason,
        )
        .await
}

fn state(f: &Fixture) -> ComputerActionStateReport {
    ComputerActionStateReport {
        work_id: f.plan.work_id.clone(),
        action_request_id: f.plan.action_request_id.clone(),
        execution_generation: f.plan.execution_generation.clone(),
        phase: ComputerActionPhase::CancelRequested,
        result: None,
    }
}

async fn work(f: &Fixture) -> agent_action_item::Model {
    agent_action_item::Entity::find_by_id(f.plan.work_id.parse::<i64>().unwrap())
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn stop_intent_is_subject_bound_atomic_idempotent_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stop.db");
    let f = Fixture::new(file_db(&path).await).await;
    assert!(request(&f, "stop-1", "private stop reason").await.is_err());
    ready(&f).await;
    let old_session = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let old_work = work(&f).await;
    for (run, actor, device) in [
        ("other", "actor-1", "device-1"),
        ("run-1", "other", "device-1"),
        ("run-1", "actor-1", "other"),
    ] {
        assert!(
            f.store
                .request_computer_background_cancel(
                    &f.plan.action_request_id,
                    run,
                    actor,
                    device,
                    "stop-1",
                    "private stop reason"
                )
                .await
                .is_err()
        );
        assert_eq!(work(&f).await, old_work);
    }
    // Failed persistence rolls back both intent and the original work marker.
    f.store.db.execute_unprepared("CREATE TRIGGER reject_stop BEFORE UPDATE OF computer_background_json ON agent_capability_dispatch_outbox BEGIN SELECT RAISE(ABORT, 'fixture stop save failure'); END").await.unwrap();
    assert!(request(&f, "stop-1", "private stop reason").await.is_err());
    assert_eq!(work(&f).await, old_work);
    f.store
        .db
        .execute_unprepared("DROP TRIGGER reject_stop")
        .await
        .unwrap();
    let task = request(&f, "stop-1", "private stop reason")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.state, BackgroundTaskState::CancelRequested);
    assert_eq!(task.progress_sequence, 1);
    let original = f.outbox().await;
    assert!(
        !original
            .computer_background_json
            .as_ref()
            .unwrap()
            .contains("private stop reason")
    );
    let stopped_work = work(&f).await;
    assert_eq!(
        request(&f, "stop-1", "private stop reason")
            .await
            .unwrap()
            .unwrap(),
        task
    );
    for (id, reason) in [
        ("stop-2", "private stop reason"),
        ("stop-1", "changed"),
        ("\n", "reason"),
    ] {
        assert!(request(&f, id, reason).await.is_err());
    }
    assert_eq!(original, f.outbox().await);
    assert_eq!(stopped_work, work(&f).await);
    assert_eq!(
        old_session,
        agent_session::Entity::find()
            .one(&f.store.db)
            .await
            .unwrap()
            .unwrap()
    );
    let snapshot = SignalAgentSessionStore::new(f.store.db.clone())
        .read_assistant_snapshot_for_subject("run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.background_tasks[0], task);
    let id = stopped_work.id;
    drop(f);
    let reopened = SignalCapabilityGrantStore::new(reopen(&path).await);
    assert!(
        reopened
            .computer_cancel_candidate(id)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        agent_action_item::Entity::find()
            .count(&reopened.db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        agent_capability_grant::Entity::find()
            .one(&reopened.db)
            .await
            .unwrap()
            .unwrap()
            .remaining_uses,
        0
    );
}

#[tokio::test]
async fn stop_observation_requires_original_identity_and_never_replaces_late_result() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("ack.db")).await).await;
    ready(&f).await;
    let id = work(&f).await.id;
    let ack = state(&f);
    let frame_id = wire_request_id(id, &ack.execution_generation);
    assert!(
        f.store
            .accept_computer_cancel_state("host-original", "device-1", &frame_id, &ack)
            .await
            .is_err()
    );
    request(&f, "stop-1", "reason").await.unwrap();
    let original = f.outbox().await;
    for (connection, audience, request_id) in [
        ("new-host", "device-1", frame_id.as_str()),
        ("host-original", "other", frame_id.as_str()),
        ("host-original", "device-1", "other"),
    ] {
        assert!(
            !f.store
                .accept_computer_cancel_state(connection, audience, request_id, &ack)
                .await
                .unwrap_or(false)
        );
        assert_eq!(original, f.outbox().await);
    }
    for mutation in ["work", "call", "generation", "phase", "result"] {
        let mut bad = ack.clone();
        match mutation {
            "work" => bad.work_id = "99999".into(),
            "call" => bad.action_request_id = "other".into(),
            "generation" => bad.execution_generation = "other".into(),
            "phase" => bad.phase = ComputerActionPhase::Completed,
            _ => {
                bad.result =
                    Some(desk_agent_protocol::computer_use::ComputerActionResultClass::Verified)
            }
        }
        assert!(
            !f.store
                .accept_computer_cancel_state("host-original", "device-1", &frame_id, &bad)
                .await
                .unwrap_or(false)
        );
        assert_eq!(original, f.outbox().await);
    }
    assert!(
        f.store
            .accept_computer_cancel_state("host-original", "device-1", &frame_id, &ack)
            .await
            .unwrap()
    );
    let observed = f.outbox().await;
    assert!(
        !f.store
            .accept_computer_cancel_state("host-original", "device-1", &frame_id, &ack)
            .await
            .unwrap()
    );
    assert_eq!(observed, f.outbox().await);
    assert!(
        f.store
            .computer_cancel_candidate(id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(work(&f).await.result_json, None);
    assert_eq!(
        request(&f, "stop-1", "reason")
            .await
            .unwrap()
            .unwrap()
            .state,
        BackgroundTaskState::CancelRequested
    );
    let completed = super::completion::verified(&f.plan);
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            &completed,
        )
        .await
        .unwrap();
    let receipt = f
        .store
        .read_computer_result(&f.plan.execution_generation, "run-1", "actor-1", "device-1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.outcome, CapabilityDispatchOutcome::Succeeded);
    assert_eq!(
        request(&f, "stop-1", "reason")
            .await
            .unwrap()
            .unwrap()
            .state,
        BackgroundTaskState::Succeeded
    );
    assert!(request(&f, "stop-1", "changed").await.is_err());
    assert_eq!(receipt.work, work(&f).await);
}

#[tokio::test]
async fn independent_sqlite_stops_and_completion_preserve_one_intent_and_original_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("race.db");
    let f = Fixture::new(file_db(&path).await).await;
    ready(&f).await;
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(9));
    let mut tasks = vec![];
    for index in 0..8 {
        let db = reopen(&path).await;
        let plan = f.plan.clone();
        let gate = gate.clone();
        tasks.push(tokio::spawn(async move {
            let store = SignalCapabilityGrantStore::new(db);
            gate.wait().await;
            if index == 0 {
                store
                    .accept_computer_completion(
                        "host-original",
                        "device-1",
                        &plan.execution_generation,
                        &super::completion::failed(&plan),
                    )
                    .await
                    .unwrap();
            } else {
                store
                    .request_computer_background_cancel(
                        &plan.action_request_id,
                        "run-1",
                        "actor-1",
                        "device-1",
                        "stop-race",
                        "same reason",
                    )
                    .await
                    .unwrap()
                    .unwrap();
            }
        }));
    }
    gate.wait().await;
    for task in tasks {
        task.await.unwrap();
    }
    let terminal = request(&f, "stop-race", "same reason")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.state, BackgroundTaskState::Failed);
    assert_eq!(terminal.progress_sequence, 3);
    assert_eq!(terminal.result_envelope_ids.len(), 1);
    assert!(
        f.store
            .computer_cancel_candidate(work(&f).await.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        agent_action_item::Entity::find()
            .count(&f.store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn expiry_remains_unknown_and_only_native_pause_completes_as_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("expiry-stop.db")).await).await;
    ready(&f).await;
    request(&f, "stop-1", "reason").await.unwrap();
    let id = work(&f).await.id;
    // Session teardown is an unknown observation, not proof of a stopped action.
    f.store
        .mark_dispatch_outcome_unknown(
            &f.plan.execution_generation,
            &f.plan.action_request_id,
            1,
            Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    assert_eq!(
        request(&f, "stop-1", "reason")
            .await
            .unwrap()
            .unwrap()
            .state,
        BackgroundTaskState::OutcomeUnknown
    );
    assert!(
        f.store
            .computer_cancel_candidate(id)
            .await
            .unwrap()
            .is_some()
    );
    let mut paused = super::completion::failed(&f.plan);
    paused.result = desk_agent_protocol::computer_use::ComputerActionResultClass::PausedByUser;
    f.store
        .accept_computer_completion(
            "host-original",
            "device-1",
            &f.plan.execution_generation,
            &paused,
        )
        .await
        .unwrap();
    let task = request(&f, "stop-1", "reason").await.unwrap().unwrap();
    assert_eq!(task.state, BackgroundTaskState::Cancelled);
    assert_eq!(task.result_envelope_ids.len(), 1);
    assert_eq!(task.progress_sequence, 3);
    assert!(
        f.store
            .computer_cancel_candidate(id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn malformed_stop_metadata_fails_closed_and_does_not_alter_original_result() {
    for field in [
        "reason_sha256",
        "requested_at_unix_ms",
        "observed_at_unix_ms",
        "cancel_generation",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let f = Fixture::new(file_db(&dir.path().join("bad-stop.db")).await).await;
        ready(&f).await;
        request(&f, "stop-1", "reason").await.unwrap();
        let id = work(&f).await.id;
        if field == "cancel_generation" {
            let mut active: agent_action_item::ActiveModel = work(&f).await.into();
            active.cancel_generation = Set(Some("other".into()));
            active.update(&f.store.db).await.unwrap();
        } else {
            let row = f.outbox().await;
            let mut json: serde_json::Value =
                serde_json::from_str(row.computer_background_json.as_deref().unwrap()).unwrap();
            json["cancel"][field] = if field == "reason_sha256" {
                serde_json::json!("invalid")
            } else {
                serde_json::json!(0)
            };
            let mut active: agent_capability_dispatch_outbox::ActiveModel = row.into();
            active.computer_background_json = Set(Some(json.to_string()));
            active.update(&f.store.db).await.unwrap();
        }
        assert!(f.store.computer_cancel_candidate(id).await.is_err());
        assert!(request(&f, "stop-1", "reason").await.is_err());
        assert_eq!(work(&f).await.result_json, None);
    }
}

#[tokio::test]
async fn stop_scan_advances_past_a_full_corrupt_page_to_original_pending_work() {
    let dir = tempfile::tempdir().unwrap();
    let db = file_db(&dir.path().join("stop-page.db")).await;
    // Reserve a later positive key before preparation, preserving its complete
    // production-minted identity while inserting earlier malformed fixtures.
    db.execute_unprepared(
        "INSERT INTO sqlite_sequence(name, seq) VALUES ('agent_action_item', 1000)",
    )
    .await
    .unwrap();
    let f = Fixture::new(db).await;
    ready(&f).await;
    request(&f, "stop-1", "reason").await.unwrap();
    let original = work(&f).await;
    assert_eq!(original.id, 1001);
    for index in 1..=33 {
        let mut bad: agent_action_item::ActiveModel = original.clone().into();
        bad = bad.reset_all();
        bad.id = Set(index);
        bad.action_request_id = Set(format!("bad-stop-{index}"));
        bad.exec_request_id = Set(None);
        bad.claim_token = Set(None);
        bad.completion_event_id = Set(format!("bad-stop-event-{index}"));
        bad.insert(&f.store.db).await.unwrap();
    }
    let dispatcher = crate::computer_cancel_dispatch::SignalComputerCancelDispatcher::new(
        f.store.db.clone(),
        std::sync::Arc::new(desk_signal_facade::model::connection::SharedConnectionMap::new()),
    );
    assert_eq!(dispatcher.scan_once(0).await.unwrap(), (Some(32), 0));
    assert_eq!(dispatcher.scan_once(32).await.unwrap(), (None, 0));
    let (ids, next) = f.store.computer_cancel_page(32).await.unwrap();
    assert_eq!(ids, [33, original.id]);
    assert_eq!(next, None);
    assert!(
        f.store
            .computer_cancel_candidate(original.id)
            .await
            .unwrap()
            .is_some()
    );
}
