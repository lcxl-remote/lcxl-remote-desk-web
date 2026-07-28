use super::*;

/// `desktop_requires_system_token` is the gate the launch path uses to
/// pick between user-token (`WTSQueryUserToken`) and SYSTEM-token
/// (`OpenProcessToken` + `SetTokenInformation(TokenSessionId)`) paths.
/// The classification has to stay tight: any false positive would
/// downgrade an ordinary worker to SYSTEM (loses user profile / network
/// drives); any false negative would route a Winlogon launch through
/// the user token and `CreateProcessAsUserW` would fail with
/// ERROR_ACCESS_DENIED.
#[test]
fn winlogon_requires_system_token() {
    assert!(desktop_requires_system_token(Some("Winlogon")));
}

#[test]
fn ordinary_desktops_do_not_require_system_token() {
    assert!(!desktop_requires_system_token(Some("Default")));
    assert!(!desktop_requires_system_token(Some("Screen-saver")));
    assert!(!desktop_requires_system_token(None));
}

/// Case-sensitive: Windows desktop names are conventionally fixed-case
/// and our `desktop_monitor::names_equal` is strict. Aligning with that
/// keeps the routing decision consistent with the detection side.
#[test]
fn winlogon_check_is_case_sensitive() {
    assert!(!desktop_requires_system_token(Some("winlogon")));
    assert!(!desktop_requires_system_token(Some("WINLOGON")));
}

/// When the operator disabled the watchdog (debug aid), even an
/// indefinitely-stale heartbeat must not trigger a restart — that's
/// the entire point of the toggle.
#[test]
fn disabled_watchdog_never_fires() {
    assert!(!worker_is_stale(
        false,
        Duration::from_secs(30),
        Duration::from_secs(0),
    ));
    assert!(!worker_is_stale(
        false,
        Duration::from_secs(30),
        Duration::from_secs(3600),
    ));
}

/// Heartbeats are 5s apart and timeout defaults to 30s; healthy
/// elapsed values should not trip the watchdog.
#[test]
fn fresh_heartbeat_does_not_fire() {
    assert!(!worker_is_stale(
        true,
        Duration::from_secs(30),
        Duration::from_secs(0),
    ));
    assert!(!worker_is_stale(
        true,
        Duration::from_secs(30),
        Duration::from_secs(5),
    ));
    assert!(!worker_is_stale(
        true,
        Duration::from_secs(30),
        Duration::from_secs(29),
    ));
}

/// Construct a bare WorkerManager for unit testing the connection-state
/// API. Settings are defaulted (none of these tests touch the watchdog
/// or settings hot-reread).
fn test_manager() -> (WorkerManager, WorkerMessageReceiver) {
    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(
        crate::model::settings::Settings::default(),
    )));
    WorkerManager::new(settings, PcRegistry::new())
}

#[tokio::test]
async fn remote_access_transition_waits_for_matching_worker_ack() {
    let (manager, _worker_rx) = test_manager();
    let (ipc_tx, mut ipc_rx) = mpsc::unbounded_channel();
    manager.install_active_for_test(ipc_tx).await;
    let payload = desk_ipc_protocol::message::RemoteAccessStatePayload {
        operation_id: "lock-op".into(),
        state_version: 8,
        locked: true,
    };
    let waiter = {
        let manager = manager.clone();
        tokio::spawn(async move {
            manager
                .apply_remote_access_state(payload, Duration::from_secs(1))
                .await
        })
    };
    assert!(matches!(
        ipc_rx.recv().await,
        Some(ServiceToWorker::SetRemoteAccessState(_))
    ));
    manager.complete_remote_access_ack(
        desk_ipc_protocol::message::RemoteAccessStateAppliedPayload {
            operation_id: "lock-op".into(),
            state_version: 8,
            cancelled_terminals: 0,
            cancelled_transfers: 0,
            cancelled_execs: 0,
        },
    );
    assert_eq!(waiter.await.unwrap().unwrap(), true);
}

#[tokio::test]
async fn remote_access_transition_times_out_without_ack() {
    let (manager, _worker_rx) = test_manager();
    let (ipc_tx, mut ipc_rx) = mpsc::unbounded_channel();
    manager.install_active_for_test(ipc_tx).await;
    let payload = desk_ipc_protocol::message::RemoteAccessStatePayload {
        operation_id: "no-ack".into(),
        state_version: 9,
        locked: true,
    };
    let result = manager
        .apply_remote_access_state(payload, Duration::from_millis(10))
        .await;
    assert!(matches!(
        ipc_rx.recv().await,
        Some(ServiceToWorker::SetRemoteAccessState(_))
    ));
    assert!(result.unwrap_err().contains("did not acknowledge"));
}

/// Keep-PC contract: `notify_desktop_switch` pauses every PC in the
/// registry it was constructed with. This is the contract the daemon
/// relies on so frames from the about-to-die worker are dropped
/// instead of pushed to the browser with stale references.
#[tokio::test]
async fn notify_desktop_switch_pauses_all_pcs() {
    use crate::daemon::pc_manager::PcRegistry;
    use desk_signal_facade::model::signal::RequestRemoteModel;

    let pc_registry = PcRegistry::new();
    let request_remote = RequestRemoteModel {
        purpose: desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop,
        ice_servers: vec![],
        grant_session_id: None,
    };
    let mut s = crate::model::settings::Settings::default();
    s.args.startup_mode = crate::model::settings::StartupMode::ServiceDaemon;
    for id in ["pc-a", "pc-b"] {
        pc_registry
            .create_for_request_remote(id, &request_remote, &s)
            .await
            .expect("create");
    }

    let settings = web::Data::from(Arc::new(crate::model::settings::SharedSettings::from(s)));
    let (mgr, _rx) = WorkerManager::new(settings, pc_registry.clone());

    // Pre-condition: nothing paused.
    for id in ["pc-a", "pc-b"] {
        let ctx = pc_registry.get(id).await.unwrap();
        assert!(
            !ctx.read()
                .await
                .media_paused
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    mgr.notify_desktop_switch().await;

    // Post-condition: every PC is paused.
    for id in ["pc-a", "pc-b"] {
        let ctx = pc_registry.get(id).await.unwrap();
        assert!(
            ctx.read()
                .await
                .media_paused
                .load(std::sync::atomic::Ordering::Relaxed),
            "notify_desktop_switch must pause {id}"
        );
    }
}

/// `is_inprocess()` defaults to `false` because new managers run in
/// daemon-spawned (named-pipe) mode unless explicitly switched. The
/// flag is meant to be one-way — set once by `start_inprocess_worker`
/// — so the default must never accidentally drift to `true`.
#[test]
fn is_inprocess_false_by_default() {
    let (mgr, _rx) = test_manager();
    assert!(
        !mgr.is_inprocess(),
        "fresh WorkerManager defaults to daemon-spawned (out-of-process) mode"
    );
}

/// `handle_crash_recovery` in in-process mode must not try to spawn
/// a replacement worker. In portable mode there is no external
/// process to relaunch, and `start_worker` would call
/// `CreateProcessAsUserW` from a non-SYSTEM context — succeeding
/// only by accident, mostly failing in confusing ways. The fix:
/// short-circuit the recovery before any spawn.
#[tokio::test]
async fn handle_crash_recovery_is_noop_when_inprocess() {
    let (mgr, _rx) = test_manager();
    mgr.is_inprocess.store(true, Ordering::Relaxed);

    // Should return synchronously without scheduling any recovery work.
    let worker = mgr.mint_worker().incarnation();
    mgr.handle_crash_recovery(worker, 0, None);

    // Yield once so any (incorrectly) spawned task would get a chance
    // to flip state. With the fix in place, nothing is queued.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let inner = mgr.inner.lock().await;
    assert!(
        inner.active_worker.is_none(),
        "in-process crash recovery must not start a worker"
    );
}

/// Capabilities round-trip: `set_worker_capabilities` stores the
/// snapshot and `worker_capabilities()` returns it. The daemon's
/// signaling_proxy relies on this to bridge `WorkerToService::
/// Capabilities` into the `RequestRemote` Init reply path.
#[tokio::test]
async fn worker_capabilities_round_trip() {
    let (mgr, _rx) = test_manager();
    assert!(
        mgr.worker_capabilities().is_none(),
        "capabilities are None until the worker reports"
    );
    let caps = MediaCapabilities {
        video_codecs: vec![
            desk_ipc_protocol::message::MediaCodec::H264,
            desk_ipc_protocol::message::MediaCodec::Vp9,
        ],
        audio_codecs: vec![desk_ipc_protocol::message::MediaCodec::Opus],
        video_encoders: vec!["X264".to_string(), "H264".to_string(), "VP9".to_string()],
        audio_encoders: vec!["OPUS".to_string()],
        video_device_list: std::collections::BTreeMap::new(),
        audio_device_list: std::collections::BTreeMap::new(),
        has_tauri: true,
        is_admin: false,
        desktop_name: "Default".to_string(),
    };
    mgr.set_worker_capabilities(caps.clone());
    let got = mgr.worker_capabilities().expect("capabilities present");
    assert_eq!(got.video_codecs, caps.video_codecs);
    assert_eq!(got.audio_codecs, caps.audio_codecs);
    assert_eq!(got.desktop_name, "Default");
    assert!(got.has_tauri);
}

/// `set_worker_capabilities` must bump `capabilities_version` and
/// notify the watch channel so awaiters can react. Backbone of the
/// `VirtualDisplaySupervisor::ensure_attached` post-attach cache
/// sync wait.
#[tokio::test]
async fn set_worker_capabilities_increments_version_and_notifies_watchers() {
    use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
    let (mgr, _rx) = test_manager();
    assert_eq!(mgr.capabilities_version(), 0, "starts at 0");
    let mut watcher = mgr.subscribe_capabilities_version();
    assert_eq!(*watcher.borrow_and_update(), 0);

    let mut video_device_list: std::collections::BTreeMap<String, Vec<DisplayInfo>> =
        std::collections::BTreeMap::new();
    video_device_list.insert(
        "wgc".to_string(),
        vec![DisplayInfo {
            device_name: "\\\\.\\DISPLAY1".to_string(),
            display_device_name: None,
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            resolutions: vec![],
            attached_to_desktop: true,
            rotation: 0,
        }],
    );
    let caps = MediaCapabilities {
        video_codecs: vec![],
        audio_codecs: vec![],
        video_encoders: vec![],
        audio_encoders: vec![],
        video_device_list,
        audio_device_list: std::collections::BTreeMap::new(),
        has_tauri: false,
        is_admin: false,
        desktop_name: "Default".to_string(),
    };
    mgr.set_worker_capabilities(caps);
    watcher
        .changed()
        .await
        .expect("watch channel must notify on first set");
    assert_eq!(mgr.capabilities_version(), 1);
    assert_eq!(*watcher.borrow_and_update(), 1);

    // Successive sets continue to bump monotonically.
    let caps2 = MediaCapabilities {
        video_codecs: vec![],
        audio_codecs: vec![],
        video_encoders: vec![],
        audio_encoders: vec![],
        video_device_list: std::collections::BTreeMap::new(),
        audio_device_list: std::collections::BTreeMap::new(),
        has_tauri: false,
        is_admin: false,
        desktop_name: "Default".to_string(),
    };
    mgr.set_worker_capabilities(caps2);
    watcher
        .changed()
        .await
        .expect("watch notifies on second set");
    assert_eq!(mgr.capabilities_version(), 2);
}

/// `capabilities_contains_display` semantics: only true when the
/// cache is set AND at least one `DisplayInfo.device_name` across
/// all backend buckets equals the requested display name. Note the
/// outer map keys are backend names ("wgc"), not display names.
#[tokio::test]
async fn capabilities_contains_display_handles_unset_and_per_backend_buckets() {
    use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
    let (mgr, _rx) = test_manager();
    assert!(
        !mgr.capabilities_contains_display("\\\\.\\DISPLAY1"),
        "no cache -> false"
    );

    let mut video_device_list: std::collections::BTreeMap<String, Vec<DisplayInfo>> =
        std::collections::BTreeMap::new();
    let display_info = |name: &str| DisplayInfo {
        device_name: name.to_string(),
        display_device_name: None,
        desktop_coordinates: DisplayRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        },
        resolutions: vec![],
        attached_to_desktop: true,
        rotation: 0,
    };
    video_device_list.insert("wgc".to_string(), vec![display_info("\\\\.\\DISPLAY1")]);
    video_device_list.insert("dxgi".to_string(), vec![display_info("\\\\.\\DISPLAY2")]);
    let caps = MediaCapabilities {
        video_codecs: vec![],
        audio_codecs: vec![],
        video_encoders: vec![],
        audio_encoders: vec![],
        video_device_list,
        audio_device_list: std::collections::BTreeMap::new(),
        has_tauri: false,
        is_admin: false,
        desktop_name: "Default".to_string(),
    };
    mgr.set_worker_capabilities(caps);

    assert!(
        mgr.capabilities_contains_display("\\\\.\\DISPLAY1"),
        "wgc bucket matches"
    );
    assert!(
        mgr.capabilities_contains_display("\\\\.\\DISPLAY2"),
        "dxgi bucket matches"
    );
    assert!(
        !mgr.capabilities_contains_display("\\\\.\\DISPLAY9"),
        "absent display -> false"
    );
    assert!(
        !mgr.capabilities_contains_display("wgc"),
        "backend name is the map key, not a display name; must not match",
    );
}

/// Boundary: strictly greater than. Setting timeout exactly equal
/// to a round multiple of the heartbeat interval shouldn't cause
/// jitter-driven false fires.
#[test]
fn heartbeat_at_exactly_timeout_does_not_fire() {
    assert!(!worker_is_stale(
        true,
        Duration::from_secs(30),
        Duration::from_secs(30),
    ));
}

/// Once elapsed exceeds the timeout the watchdog must report
/// stuck — this is the entire reason the watchdog exists.
#[test]
fn stale_heartbeat_fires_when_enabled() {
    assert!(worker_is_stale(
        true,
        Duration::from_secs(30),
        Duration::from_secs(31),
    ));
    assert!(worker_is_stale(
        true,
        Duration::from_secs(30),
        Duration::from_secs(120),
    ));
}
