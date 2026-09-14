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
    let denied_quota = desk_ipc_protocol::message::FileRecoveryQuotaRequest {
        request_id: "quota-denied".into(),
        os_user: "not-the-worker-user".into(),
        command: desk_ipc_protocol::message::FileRecoveryQuotaCommand::Read,
    };
    manager
        .handle_file_recovery_quota(None, WorkerIncarnation(2), denied_quota.clone())
        .await;
    assert!(rx.try_recv().is_err());
    manager
        .handle_file_recovery_quota(None, WorkerIncarnation(1), denied_quota)
        .await;
    let reply = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        reply,
        ServiceToWorker::FileRecoveryQuotaReplied(
            desk_ipc_protocol::message::FileRecoveryQuotaReply {
                outcome: desk_ipc_protocol::message::FileRecoveryQuotaOutcome::IdentityChanged,
                ..
            }
        )
    ));
    let recovery = desk_ipc_protocol::message::FileRecoveryRequestPayload {
        request_id: "recovery-request".into(),
        connection_id: None,
        authority: "a".repeat(64),
        actor_id: "1".into(),
        device_id: "device".into(),
        request: desk_agent_protocol::file_recovery::FileRecoveryRequest {
            expected_authority: Some("a".repeat(64)),
            expected_os_user: Some("501".into()),
            command: desk_agent_protocol::file_recovery::FileRecoveryCommand::DeleteConversation {
                conversation_id: "conversation".into(),
            },
        },
    };
    manager
        .send_file_recovery_request(recovery.clone())
        .await
        .unwrap();
    let Some(ServiceToWorker::ManageFileRecovery(delivered)) = rx.recv().await else {
        panic!("recovery expected")
    };
    assert_eq!(delivered.request.expected_os_user.as_deref(), Some("501"));
    manager.enable_session_targeting_for_test();
    assert!(
        manager
            .send_file_recovery_request(recovery.clone())
            .await
            .is_err()
    );
    let mut discovery = recovery;
    discovery.request.expected_os_user = None;
    assert!(manager.send_file_recovery_request(discovery).await.is_err());
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
