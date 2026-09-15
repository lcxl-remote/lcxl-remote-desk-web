use super::*;

#[tokio::test]
async fn reply_requires_original_live_worker_session_and_user() {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    let (manager, _messages) = WorkerManager::new(settings, PcRegistry::new());
    let mut session = 0;
    unsafe {
        windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(
            std::process::id(),
            &mut session,
        )
    }
    .unwrap();
    assert_ne!(session, 0, "run in an interactive Windows user session");
    let key = WorkerKey {
        session: desk_ipc_protocol::message::SessionKey {
            platform_session_id: session.to_string(),
            session_generation: 1,
        },
        desktop: DesktopTarget::WindowsDefault,
    };
    let (ipc, _commands) = mpsc::unbounded_channel();
    let incarnation = manager.install_resident_for_test(key.clone(), ipc).await;
    let sid = {
        let mut inner = manager.inner.lock().await;
        let worker = inner.resident_workers.get_mut(&key).unwrap();
        worker.session_id = session;
        worker.desktop_name = Some("Default".into());
        worker.inprocess_task = Some(tokio::spawn(std::future::pending()));
        file_recovery_quota::windows_worker_user(worker).unwrap()
    };
    let id = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = oneshot::channel();
    manager.local_recovery_requests.0.lock().unwrap().insert(
        id.clone(),
        Pending {
            key: key.clone(),
            incarnation,
            sid: sid.clone(),
            session_id: session,
            tx,
        },
    );
    let reply = || LocalFileRecoveryReply {
        request_id: id.clone(),
        outcome: Ok(LocalFileRecoveryOutcome::Export(vec![1, 2, 3])),
    };
    manager
        .complete_local_file_recovery(None, incarnation, reply())
        .await;
    manager
        .complete_local_file_recovery(Some(&key), WorkerIncarnation(incarnation.0 + 1), reply())
        .await;
    assert!(matches!(
        rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    {
        let mut requests = manager.local_recovery_requests.0.lock().unwrap();
        requests.get_mut(&id).unwrap().sid = "S-1-5-18".into();
    }
    manager
        .complete_local_file_recovery(Some(&key), incarnation, reply())
        .await;
    assert!(matches!(
        rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    {
        let mut requests = manager.local_recovery_requests.0.lock().unwrap();
        let pending = requests.get_mut(&id).unwrap();
        pending.sid = sid;
        pending.session_id = session + 1;
    }
    manager
        .complete_local_file_recovery(Some(&key), incarnation, reply())
        .await;
    assert!(matches!(
        rx.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    manager
        .local_recovery_requests
        .0
        .lock()
        .unwrap()
        .get_mut(&id)
        .unwrap()
        .session_id = session;
    manager
        .complete_local_file_recovery(Some(&key), incarnation, reply())
        .await;
    assert_eq!(
        rx.await.unwrap().unwrap().into_export().unwrap(),
        vec![1, 2, 3]
    );
    manager
        .complete_local_file_recovery(Some(&key), incarnation, reply())
        .await;
    assert!(manager.local_recovery_requests.0.lock().unwrap().is_empty());
    manager
        .inner
        .lock()
        .await
        .resident_workers
        .get_mut(&key)
        .unwrap()
        .inprocess_task
        .take()
        .unwrap()
        .abort();
}

#[test]
fn cancelled_http_waiter_removes_pending_request() {
    let requests = PendingRequests::default();
    let id = uuid::Uuid::new_v4().to_string();
    let (tx, rx) = oneshot::channel();
    requests.0.lock().unwrap().insert(
        id.clone(),
        Pending {
            key: WorkerKey {
                session: desk_ipc_protocol::message::SessionKey {
                    platform_session_id: "1".into(),
                    session_generation: 1,
                },
                desktop: DesktopTarget::WindowsDefault,
            },
            incarnation: WorkerIncarnation(1),
            sid: "test-user".into(),
            session_id: 1,
            tx,
        },
    );
    drop(RemovePending {
        requests: requests.clone(),
        id,
    });
    assert!(requests.0.lock().unwrap().is_empty());
    assert!(rx.blocking_recv().is_err());
}
