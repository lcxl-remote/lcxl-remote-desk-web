//! Telling a worker's messages apart from its replacement's.
//!
//! Replacing a worker does not silence it. Whatever it had already put on the
//! wire, and whatever its bridge had already queued, arrives after the
//! replacement is installed — so the daemon has to be able to look at a message
//! and say which worker it came from. These cover that decision; the handlers
//! downstream all assume it has already been made.
//!
//! Platform-independent by design: all three host forms (portable, desk-server,
//! service-daemon) run the same daemon-worker link, differing only in whether
//! the bridge spans processes.

use super::*;

fn test_manager() -> (WorkerManager, WorkerMessageReceiver) {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    WorkerManager::new(settings, PcRegistry::new())
}

/// Installs a worker and returns its incarnation. The command receiver is
/// dropped: these tests never look at what the daemon sends downwards.
async fn install_worker(manager: &WorkerManager) -> WorkerIncarnation {
    let (ipc_tx, _ipc_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
    manager.install_active_for_test(ipc_tx).await
}

/// The ordinary case: the worker that is running is heard.
#[tokio::test]
async fn the_running_worker_is_heard() {
    let (manager, _worker_rx) = test_manager();
    let worker = install_worker(&manager).await;

    assert!(manager.note_message_from(worker).await);
}

/// A worker that has been replaced is not. Its message describes a process the
/// daemon has already torn down — acting on it means letting a worker that is
/// gone overwrite what the daemon believes about the one that took its place.
#[tokio::test]
async fn a_replaced_worker_is_not() {
    let (manager, _worker_rx) = test_manager();
    let outgoing = install_worker(&manager).await;
    let incoming = install_worker(&manager).await;

    assert_ne!(
        outgoing, incoming,
        "a replacement must be a different worker, not the same slot reused",
    );
    assert!(!manager.note_message_from(outgoing).await);
    assert!(manager.note_message_from(incoming).await);
}

/// Portal authorization is owned by the user-session worker. Replacing that
/// worker must invalidate both ready and in-flight snapshots; otherwise the
/// daemon could keep reporting Pending forever or admit against a dead session.
#[tokio::test]
async fn a_replacement_clears_the_previous_workers_portal_snapshot() {
    let (manager, _worker_rx) = test_manager();
    install_worker(&manager).await;
    manager.set_wayland_portal_snapshot(desk_wayland_portal::PortalSnapshot {
        phase: desk_wayland_portal::PortalPhase::Preparing,
        capabilities: desk_wayland_portal::PortalCapabilities::default(),
        availability: desk_wayland_portal::PortalAvailability::default(),
        target: Some(desk_wayland_portal::AuthorizationTarget::ScreenAndInput),
        operation_id: Some("op-7".to_string()),
        generation: 3,
        restore_token_persisted: false,
        requires_local_action: true,
        reason_code: None,
        reason: None,
    });
    assert!(manager.wayland_portal_snapshot().is_some());
    #[cfg(target_os = "linux")]
    assert_eq!(
        manager.linux_display_server(),
        desk_utils::linux_display::LinuxDisplayServer::Wayland
    );

    install_worker(&manager).await;

    assert!(
        manager.wayland_portal_snapshot().is_none(),
        "a replacement starts NotConfigured until it publishes its own snapshot",
    );
}

/// Every message counts as a sign of life, but only for the worker that sent
/// it. A replaced worker's backlog arriving must not stand in for a replacement
/// that has never spoken — that is exactly the case the watchdog exists to
/// catch, and crediting the backlog would keep it quiet through the whole
/// timeout.
#[tokio::test]
async fn a_replaced_workers_backlog_is_not_its_successors_heartbeat() {
    let (manager, _worker_rx) = test_manager();
    let outgoing = install_worker(&manager).await;
    let incoming = install_worker(&manager).await;

    let installed_at = manager
        .active_worker_snapshot()
        .await
        .expect("a worker is installed")
        .3;

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(!manager.note_message_from(outgoing).await);
    assert_eq!(
        manager
            .active_worker_snapshot()
            .await
            .expect("a worker is installed")
            .3,
        installed_at,
        "the replacement has said nothing, so its last sign of life must not move",
    );

    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(manager.note_message_from(incoming).await);
    assert!(
        manager
            .active_worker_snapshot()
            .await
            .expect("a worker is installed")
            .3
            > installed_at,
        "the replacement speaking for itself is what counts",
    );
}

/// A pipe server waits up to fifteen seconds for its worker to dial in, and a
/// desktop switch inside that window installs a replacement. When the wait then
/// fails, restarting on the abandoned worker's behalf would kill the one that
/// is actually running.
#[tokio::test]
async fn a_replaced_worker_does_not_get_to_order_a_restart() {
    let (manager, _worker_rx) = test_manager();
    let abandoned = install_worker(&manager).await;
    let running = install_worker(&manager).await;

    assert!(!manager.restart_is_still_wanted(abandoned).await);
    assert!(manager.restart_is_still_wanted(running).await);
}

/// A start that failed before installing anything leaves no worker to compare
/// against. The pipe server reporting the failure afterwards is the only thing
/// that will bring a worker back, so it has to be believed.
#[tokio::test]
async fn a_failed_start_stays_recoverable() {
    let (manager, _worker_rx) = test_manager();
    let never_installed = manager.mint_worker().incarnation();

    assert!(manager.restart_is_still_wanted(never_installed).await);
}

/// Incarnations are minted, never recycled. A reused number would make a late
/// message from a long-dead worker indistinguishable from a live one.
#[tokio::test]
async fn incarnations_are_never_reused() {
    let (manager, _worker_rx) = test_manager();

    let minted: Vec<_> = (0..8)
        .map(|_| manager.mint_worker().incarnation())
        .collect();
    let distinct: std::collections::HashSet<_> = minted.iter().copied().collect();

    assert_eq!(distinct.len(), minted.len());
}

/// The sink is what makes the whole scheme work: a bridge serves one worker, so
/// it stamps what it forwards without the worker having to know its own name.
#[tokio::test]
async fn a_sink_stamps_what_it_forwards() {
    let (manager, mut worker_rx) = test_manager();
    let first = manager.mint_worker();
    let second = manager.mint_worker();

    assert_ne!(
        first.incarnation(),
        second.incarnation(),
        "two workers sharing one name would leave the stamp saying nothing",
    );
    assert!(first.send(WorkerToService::Ready));
    assert!(second.send(WorkerToService::Ready));

    assert_eq!(
        worker_rx.recv().await.expect("first message").incarnation,
        first.incarnation(),
    );
    assert_eq!(
        worker_rx.recv().await.expect("second message").incarnation,
        second.incarnation(),
    );
}

/// A worker's other two lanes carry no messages to stamp — a video frame says
/// nothing about which worker captured it — so they ask the same question a
/// different way.
mod lanes {
    use super::*;
    use crate::model::settings::{Settings, SharedSettings, StartupMode};
    use desk_ipc_protocol::message::{MediaCodec, MediaFrame, MediaFrameKind};
    use desk_signal_facade::model::signal::{RemoteSessionPurpose, RequestRemoteModel};

    /// A manager sharing a registry with the caller, so a test can watch what
    /// the drains do to it.
    fn manager_over(registry: PcRegistry) -> (WorkerManager, WorkerMessageReceiver) {
        let mut settings = Settings::default();
        settings.args.startup_mode = StartupMode::ServiceDaemon;
        let settings = web::Data::from(Arc::new(SharedSettings::from(settings)));
        WorkerManager::new(settings, registry)
    }

    async fn paused_connection(registry: &PcRegistry, connection_id: &str) {
        let request_remote = RequestRemoteModel {
            requested_wayland_control_mode: Some("auto".to_string()),
            purpose: RemoteSessionPurpose::RemoteDesktop,
            ice_servers: vec![],
            grant_session_id: None,
        };
        let mut settings = Settings::default();
        settings.args.startup_mode = StartupMode::ServiceDaemon;
        let pc = registry
            .create_for_request_remote(connection_id, &request_remote, &settings)
            .await
            .expect("create");
        {
            let pc = pc.read().await;
            let mut fence = pc.media_output_fence.write().await;
            fence.video_epoch = "test-epoch".to_string();
            fence.video_generation = 1;
        }
        registry.pause_all_media().await;
    }

    fn key_frame(connection_id: &str) -> MediaFrame {
        MediaFrame {
            connection_id: connection_id.into(),
            connection_epoch: "test-epoch".to_string(),
            generation: 1,
            seq: 0,
            ts_ns: 0,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoI,
            codec: MediaCodec::H264,
            payload: vec![0x22; 32],
        }
    }

    async fn is_paused(registry: &PcRegistry, connection_id: &str) -> bool {
        registry
            .get(connection_id)
            .await
            .expect("the connection is registered")
            .read()
            .await
            .media_paused
            .load(Ordering::Relaxed)
    }

    /// The control: a key frame from the worker that is running is exactly what
    /// a paused connection is waiting for, and it must still get through.
    #[tokio::test]
    async fn a_key_frame_from_the_running_worker_resumes_a_paused_connection() {
        let registry = PcRegistry::new();
        let (manager, _worker_rx) = manager_over(registry.clone());
        paused_connection(&registry, "conn-live").await;
        let worker = manager.mint_worker();
        let (media_tx, media_rx) = inprocess::make_media();
        let drain = spawn_media_receiver_task(media_rx, registry.clone(), worker.gate());

        media_tx.send_frame(key_frame("conn-live")).await.unwrap();
        drop(media_tx);
        drain.await.expect("the drain ran to the end of the lane");

        assert!(!is_paused(&registry, "conn-live").await);
    }

    /// And the case that matters: the same frame from a worker that has been
    /// replaced. Nothing on it says so — it is the first key frame the paused
    /// connection has seen, so it would be taken as the replacement's, and the
    /// browser would be shown the desktop the daemon just moved away from.
    #[tokio::test]
    async fn a_key_frame_from_a_replaced_worker_leaves_the_connection_paused() {
        let registry = PcRegistry::new();
        let (manager, _worker_rx) = manager_over(registry.clone());
        paused_connection(&registry, "conn-swapped").await;
        let outgoing = manager.mint_worker();
        let (media_tx, media_rx) = inprocess::make_media();
        let drain = spawn_media_receiver_task(media_rx, registry.clone(), outgoing.gate());

        // A replacement is started. The outgoing worker's lane is still open and
        // still has this frame in it.
        let _incoming = manager.mint_worker();
        media_tx
            .send_frame(key_frame("conn-swapped"))
            .await
            .unwrap();
        drop(media_tx);
        drain.await.expect("the drain ran to the end of the lane");

        assert!(
            is_paused(&registry, "conn-swapped").await,
            "only the worker running now may tell a connection the swap is over",
        );
    }

    /// Taking a worker away with nothing to replace it closes its lanes too. A
    /// shutting-down daemon has no more use for what a worker had queued than a
    /// replaced one does.
    #[tokio::test]
    async fn retiring_the_last_worker_closes_its_lanes() {
        let registry = PcRegistry::new();
        let (manager, _worker_rx) = manager_over(registry.clone());
        let (ipc_tx, _ipc_rx) = mpsc::unbounded_channel::<ServiceToWorker>();
        manager.install_active_for_test(ipc_tx).await;
        let gate = manager.mint_worker().gate();
        assert!(gate.is_current(), "a freshly named worker owns its lanes");

        manager.shutdown_all().await;

        assert!(!gate.is_current());
    }
}
