use super::*;
use crate::agent_session_store::SignalAgentSessionStore;
use crate::controller::device_assistant_session::recovery::resolve;
use desk_signal_facade::model::connection::SharedConnectionMap;

#[tokio::test]
async fn offline_recovery_reads_original_snapshot_and_stops_only_the_original_task() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new_for_actor(file_db(&dir.path().join("offline.db")).await, "1").await;
    f.bind().await;
    f.accept().await.unwrap();
    let mut row: agent_capability_dispatch_outbox::ActiveModel = f.outbox().await.into();
    row.created_at = Set(Utc::now() - chrono::Duration::seconds(9));
    row.update(&f.store.db).await.unwrap();
    assert!(
        f.store
            .promote_computer_background(&f.plan.execution_generation, "run-1", "1", "device-1")
            .await
            .unwrap()
    );
    let sessions = SignalAgentSessionStore::new(f.store.db.clone());
    let row = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        PersistedAgentSession::decode_json(&row.state_json)
            .unwrap()
            .version,
        row.version
    );
    let connections = SharedConnectionMap::new();
    assert!(
        resolve(
            &sessions,
            &connections,
            "1",
            "old-host",
            None,
            Some("intent")
        )
        .await
        .unwrap()
        .is_none()
    );
    for (actor, run) in [("other", "run-1"), ("1", "missing")] {
        assert!(
            resolve(&sessions, &connections, actor, "old-host", Some(run), None)
                .await
                .unwrap()
                .is_none()
        );
    }
    let (run, device) = resolve(
        &sessions,
        &connections,
        "1",
        "old-host",
        Some("run-1"),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!((&run[..], &device[..]), ("run-1", "device-1"));
    let snapshot = sessions
        .read_assistant_snapshot_for_subject(&run, "1", &device)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.background_tasks.len(), 1);
    let stop = f
        .store
        .request_computer_background_cancel(
            &f.plan.action_request_id,
            &run,
            "1",
            &device,
            "offline-stop",
            "stop",
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stop.state,
        desk_diagnose_core::dynamic_run::BackgroundTaskState::CancelRequested
    );
    let candidate = f
        .store
        .computer_cancel_candidate(f.plan.work_id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(candidate.connection_id, "host-original");
    assert_eq!(
        agent_action_item::Entity::find()
            .count(&f.store.db)
            .await
            .unwrap(),
        1
    );
    let row = agent_session::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut state = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    state.device_id = "other".into();
    let mut active: agent_session::ActiveModel = row.into();
    active.state_json = Set(state.encode_json_for_storage().unwrap());
    active.update(&f.store.db).await.unwrap();
    assert!(
        sessions
            .recovery_device("run-1", "1")
            .await
            .unwrap()
            .is_none()
    );
}
