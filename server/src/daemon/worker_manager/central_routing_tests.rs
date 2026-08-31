//! Central actions may use a single worker, never an inferred resident session.
use super::*;

#[tokio::test]
async fn central_actions_use_single_worker_but_never_guess_a_resident_session() {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    let (manager, _messages) = WorkerManager::new(settings, PcRegistry::new());
    let (tx, mut rx) = mpsc::unbounded_channel();
    manager.inner.lock().await.active_worker = Some(WorkerHandle {
        incarnation: WorkerIncarnation(1),
        pipe_name: "test-central-action".into(),
        ipc_tx: tx,
        process_handle: None,
        last_heartbeat_at: Instant::now(),
        capabilities: None,
        session_id: 1,
        desktop_name: None,
        file_sender_tx: Arc::new(RwLock::new(None)),
        inprocess_task: None,
        inprocess_restart: None,
        lane_tasks: vec![],
    });
    for peer in [None, Some("peer")] {
        manager
            .send_central_or_connection_worker(peer, ServiceToWorker::Shutdown)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(ServiceToWorker::Shutdown)));
    }
    manager.enable_session_targeting_for_test();
    for peer in [None, Some("unbound-peer")] {
        assert!(
            manager
                .send_central_or_connection_worker(peer, ServiceToWorker::Shutdown)
                .await
                .is_err()
        );
    }
    assert!(rx.try_recv().is_err());
}
