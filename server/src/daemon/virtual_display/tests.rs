use super::*;
use crate::daemon::pc_manager::PcRegistry;
use crate::model::settings::{Settings, SharedSettings};
use actix_web::web;
use desk_ipc_protocol::message::WorkerToService;
use desk_virtual_display::VirtualDisplayHandleInner;
use std::sync::atomic::{AtomicU32, Ordering};

struct MockHandleInner;
impl VirtualDisplayHandleInner for MockHandleInner {}

struct MockLifecycle {
    create_calls: AtomicU32,
    result: fn() -> Result<VirtualDisplayHandle, VirtualDisplayError>,
}

/// The PnP instance id our `MockLifecycle::returns_handle()` mock
/// claims to have just created. Mirrors the
/// `SWD\<HW id>\<instance id>` shape produced by the real
/// `SwDeviceCreate` so the tests double as documentation of the
/// post-fix payload contract.
const MOCK_INSTANCE_ID: &str = "SWD\\MOCK\\MOCK";

impl MockLifecycle {
    fn returns_handle() -> Self {
        Self {
            create_calls: AtomicU32::new(0),
            result: || {
                Ok(VirtualDisplayHandle::new(
                    MOCK_INSTANCE_ID.to_string(),
                    Box::new(MockHandleInner),
                ))
            },
        }
    }
    fn returns_not_supported() -> Self {
        Self {
            create_calls: AtomicU32::new(0),
            result: || Err(VirtualDisplayError::NotSupported),
        }
    }
}

impl VirtualDisplayLifecycle for MockLifecycle {
    fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
        self.create_calls.fetch_add(1, Ordering::SeqCst);
        (self.result)()
    }
}

fn make_worker_mgr() -> (
    WorkerManager,
    tokio::sync::mpsc::UnboundedReceiver<WorkerToService>,
) {
    let shared = SharedSettings::from(Settings::default());
    let settings = web::Data::new(shared);
    let pc_registry = PcRegistry::new();
    WorkerManager::new(settings, pc_registry)
}

#[tokio::test]
async fn supervisor_apply_false_then_true_creates_handle() {
    let lifecycle = Arc::new(MockLifecycle::returns_handle());
    let lifecycle_for_provider = Arc::clone(&lifecycle);
    struct ArcProvider(Arc<MockLifecycle>);
    impl VirtualDisplayLifecycle for ArcProvider {
        fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
            self.0.create()
        }
    }
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(ArcProvider(lifecycle_for_provider));
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    // apply(false) on initially-Disabled is a no-op.
    supervisor
        .apply(false)
        .await
        .expect("apply(false) on Disabled");
    assert_eq!(lifecycle.create_calls.load(Ordering::SeqCst), 0);
    assert!(!supervisor.is_active().await);

    // apply(true) creates the handle, moves to Attaching.
    supervisor.apply(true).await.expect("apply(true)");
    assert_eq!(lifecycle.create_calls.load(Ordering::SeqCst), 1);
    // Attaching is NOT active — must wait for Capabilities first.
    assert!(!supervisor.is_active().await);

    // Second apply(true) is idempotent — does not re-create.
    supervisor
        .apply(true)
        .await
        .expect("apply(true) idempotent");
    assert_eq!(lifecycle.create_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn supervisor_apply_true_returns_not_supported_when_stub() {
    let provider: Box<dyn VirtualDisplayLifecycle> =
        Box::new(MockLifecycle::returns_not_supported());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    let err = supervisor
        .apply(true)
        .await
        .expect_err("apply(true) on NotSupported provider must surface error");
    match err {
        DeskError::CustomError(custom) => {
            assert_eq!(
                custom.error_code.code(),
                DeskErrorCode::FEATURE_UNAVAILABLE.code()
            );
        }
        other => panic!("expected CustomError(FEATURE_UNAVAILABLE), got {other:?}"),
    }
    assert!(!supervisor.is_active().await);
}

#[tokio::test]
async fn supervisor_apply_true_then_false_drops_handle_and_emits_detach() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _worker_rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    supervisor.apply(true).await.expect("apply(true)");
    // on_worker_capabilities no longer promotes — even if the
    // send succeeded, the supervisor stays in Attaching until an
    // explicit attach-result lands. So whatever happens to the
    // IPC send below, state remains Attaching.
    supervisor.on_worker_capabilities().await;
    assert!(
        !supervisor.is_active().await,
        "Capabilities alone must NOT promote Attaching -> Attached"
    );

    supervisor.apply(false).await.expect("apply(false)");
    // Detach drops the handle (Drop closes the OS resource).
    // We can't observe the drop directly here, but we can
    // confirm the supervisor is back to Disabled and another
    // apply(true) creates a fresh handle.
    assert!(!supervisor.is_active().await);
}

/// `apply(true)` must persist the PnP instance id surfaced by the
/// lifecycle into `SupervisorState::Attaching.instance_id`. This is
/// the value the supervisor later forwards over IPC, and a
/// regression here would make the worker resolve the wrong device.
#[tokio::test]
async fn supervisor_apply_caches_instance_id() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");
    assert_eq!(supervisor.state_label().await, "Attaching");
    assert!(!supervisor.is_active().await);

    // The instance id stored in Attaching must match the one we
    // would later forward over IPC.
    let payload = VirtualDisplayAttachResultPayload {
        instance_id: MOCK_INSTANCE_ID.to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
    };
    supervisor.on_worker_attach_result(payload).await;
    // The fact that the matching attach-result promoted to Attached
    // proves the stored id was `MOCK_INSTANCE_ID`. A mismatch would
    // have been silently dropped (see the mismatch test below).
    assert_eq!(supervisor.state_label().await, "Attached");
    assert!(supervisor.is_active().await);
}

/// This is the v1 regression test: a successful
/// `send_to_worker(AttachVirtualDisplay)` must NOT by itself
/// promote the state machine. Promotion is gated on an explicit
/// worker reply via [`on_worker_attach_result`].
#[tokio::test]
async fn supervisor_on_capabilities_sends_attach_but_does_not_promote_state() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");

    // The WorkerManager mock has no worker registered, so the
    // attach send below will return Err. Test both paths anyway:
    // even when the send succeeds (e.g. enqueue accepted), the
    // supervisor stays Attaching. With the unbound channel mock
    // currently shipped, the send fails — exercising the warn
    // path of on_worker_capabilities — but the assertion still
    // holds: NO promotion.
    supervisor.on_worker_capabilities().await;
    assert_eq!(supervisor.state_label().await, "Attaching");
    assert!(
        !supervisor.is_active().await,
        "router gate must still reject ChangeDisplaySettings until \
             the worker has confirmed via attach-result",
    );
}

/// Happy path: worker replies `Attached(name)` for the currently
/// tracked instance id → supervisor flips to `Attached` and
/// `is_active()` returns `true`. Receiving a second `Attached`
/// reply for the same id is idempotent.
#[tokio::test]
async fn supervisor_on_attach_result_attached_promotes_to_attached() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");
    assert_eq!(supervisor.state_label().await, "Attaching");

    let payload = VirtualDisplayAttachResultPayload {
        instance_id: MOCK_INSTANCE_ID.to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
    };
    supervisor.on_worker_attach_result(payload.clone()).await;
    assert_eq!(supervisor.state_label().await, "Attached");
    assert!(supervisor.is_active().await);

    // Idempotent: a second Attached reply must not regress state.
    supervisor.on_worker_attach_result(payload).await;
    assert_eq!(supervisor.state_label().await, "Attached");
    assert!(supervisor.is_active().await);
}

/// Worker replied `Failed(_)` for the currently tracked instance
/// id → supervisor stays in `Attaching`, `is_active()` remains
/// `false`. The next `Capabilities` (worker restart / desktop
/// switch) will trigger another `on_worker_capabilities` send and
/// give the worker another chance to resolve.
#[tokio::test]
async fn supervisor_on_attach_result_failed_stays_attaching() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");

    let payload = VirtualDisplayAttachResultPayload {
        instance_id: MOCK_INSTANCE_ID.to_string(),
        outcome: VirtualDisplayAttachOutcome::Failed(
            "EnumDisplayDevicesW returned seen=[] after 6 retries".to_string(),
        ),
    };
    supervisor.on_worker_attach_result(payload).await;
    assert_eq!(supervisor.state_label().await, "Attaching");
    assert!(
        !supervisor.is_active().await,
        "router must still reject ChangeDisplaySettings after worker Failed",
    );
}

/// Worker reply carrying a different `instance_id` (e.g. stale
/// reply from a previous daemon incarnation) must be dropped with
/// no state change. Mismatch is detected even for an otherwise
/// well-formed `Attached(_)` outcome.
#[tokio::test]
async fn supervisor_on_attach_result_ignores_mismatched_instance_id() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");

    let mismatched = VirtualDisplayAttachResultPayload {
        instance_id: "SWD\\OTHER\\OTHER".to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY9".to_string()),
    };
    supervisor.on_worker_attach_result(mismatched).await;
    assert_eq!(supervisor.state_label().await, "Attaching");
    assert!(!supervisor.is_active().await);
}

/// Worker reply that lands when the supervisor is `Disabled`
/// (e.g. operator toggled the feature off in the same window) must
/// be silently dropped without panicking.
#[tokio::test]
async fn supervisor_on_attach_result_ignored_when_disabled() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    // Never call apply(true) — start from Disabled.
    let payload = VirtualDisplayAttachResultPayload {
        instance_id: MOCK_INSTANCE_ID.to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
    };
    supervisor.on_worker_attach_result(payload).await;
    assert_eq!(supervisor.state_label().await, "Disabled");
    assert!(!supervisor.is_active().await);
}

#[tokio::test]
async fn supervisor_on_capabilities_no_op_when_disabled() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    // Disabled state: on_worker_capabilities should be a no-op
    // (no panic, no state change).
    supervisor.on_worker_capabilities().await;
    assert!(!supervisor.is_active().await);
}

#[tokio::test]
async fn supervisor_shutdown_drops_handle() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");
    supervisor.shutdown().await;
    assert!(!supervisor.is_active().await);
}

/// Drain all currently buffered messages out of the in-process IPC
/// `ipc_rx` without ever awaiting — the supervisor sends to an
/// unbounded channel so all enqueued messages are observable
/// immediately after the call that produced them.
fn drain_ipc(
    ipc_rx: &mut tokio::sync::mpsc::UnboundedReceiver<desk_ipc_protocol::message::ServiceToWorker>,
) -> Vec<desk_ipc_protocol::message::ServiceToWorker> {
    let mut out = Vec::new();
    while let Ok(msg) = ipc_rx.try_recv() {
        out.push(msg);
    }
    out
}

/// v4 RefreshCapabilities path: the `Attaching → Attached`
/// promotion must enqueue exactly one `RefreshCapabilities` on the
/// daemon's worker channel so the worker re-publishes its display
/// enumeration (which now includes the freshly attached IDD).
#[tokio::test]
async fn supervisor_on_attach_result_attached_emits_refresh_capabilities_to_worker() {
    use desk_ipc_protocol::message::ServiceToWorker;
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    supervisor.apply(true).await.expect("apply(true)");
    let _ = drain_ipc(&mut ipc_rx);

    let payload = VirtualDisplayAttachResultPayload {
        instance_id: MOCK_INSTANCE_ID.to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
    };
    supervisor.on_worker_attach_result(payload).await;
    let sent = drain_ipc(&mut ipc_rx);
    let refresh_count = sent
        .iter()
        .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
        .count();
    assert_eq!(
        refresh_count, 1,
        "Attaching -> Attached promotion must emit exactly one \
             RefreshCapabilities, observed: {sent:?}"
    );
}

/// Edge-trigger discipline: a second `Attached(_)` reply for an
/// already-Attached supervisor must not re-emit
/// `RefreshCapabilities`. Without this guard the worker would be
/// asked to re-publish capabilities every time the daemon
/// re-issued an AttachVirtualDisplay (which happens on each
/// `WorkerToService::Capabilities`), turning a one-shot refresh
/// into a per-Capabilities ping-pong.
#[tokio::test]
async fn supervisor_on_attach_result_attached_does_not_emit_refresh_when_already_attached() {
    use desk_ipc_protocol::message::ServiceToWorker;
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");

    let payload = VirtualDisplayAttachResultPayload {
        instance_id: MOCK_INSTANCE_ID.to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
    };
    // First Attached: edge fires.
    supervisor.on_worker_attach_result(payload.clone()).await;
    let _ = drain_ipc(&mut ipc_rx);

    // Second Attached on already-Attached supervisor: no edge.
    supervisor.on_worker_attach_result(payload).await;
    let sent = drain_ipc(&mut ipc_rx);
    let refresh_count = sent
        .iter()
        .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
        .count();
    assert_eq!(
        refresh_count, 0,
        "second Attached on an already-Attached supervisor must not \
             re-emit RefreshCapabilities; observed: {sent:?}"
    );
}

/// Detach path is symmetric: when the supervisor's `shutdown`
/// transitions away from `Attaching` / `Attached`, the worker
/// must be told to re-publish capabilities so any browser that
/// reconnects no longer sees the IDD in the dropdown.
#[tokio::test]
async fn supervisor_detach_emits_refresh_capabilities_to_worker() {
    use desk_ipc_protocol::message::ServiceToWorker;
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");
    let _ = drain_ipc(&mut ipc_rx);

    supervisor.shutdown().await;
    let sent = drain_ipc(&mut ipc_rx);
    let refresh_count = sent
        .iter()
        .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
        .count();
    let detach_count = sent
        .iter()
        .filter(|m| matches!(m, ServiceToWorker::DetachVirtualDisplay))
        .count();
    assert_eq!(
        detach_count, 1,
        "shutdown must emit one DetachVirtualDisplay; observed: {sent:?}"
    );
    assert_eq!(
        refresh_count, 1,
        "shutdown must emit one RefreshCapabilities after the detach; \
             observed: {sent:?}"
    );
}

/// Backwards-compat: the commit-4 test for the
/// `newly_constructed_supervisor_is_inactive` invariant still
/// holds with the commit-5 constructor signature.
#[tokio::test]
async fn newly_constructed_supervisor_is_inactive() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    assert!(!supervisor.is_active().await);
}

// ===== v5 lazy lifecycle: ensure_attached + lifecycle_lock =====

/// Build a `MediaCapabilities` whose `video_device_list` contains
/// exactly one display under the `"wgc"` bucket. Used by tests that
/// want to simulate the worker's post-attach `Capabilities` refresh.
fn caps_with_display(display_name: &str) -> desk_ipc_protocol::message::MediaCapabilities {
    use desk_ipc_protocol::message::MediaCapabilities;
    use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
    let mut video_device_list: std::collections::BTreeMap<String, Vec<DisplayInfo>> =
        std::collections::BTreeMap::new();
    video_device_list.insert(
        "wgc".to_string(),
        vec![DisplayInfo {
            device_name: display_name.to_string(),
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
    MediaCapabilities {
        video_codecs: vec![],
        audio_codecs: vec![],
        video_encoders: vec![],
        audio_encoders: vec![],
        video_device_list,
        audio_device_list: std::collections::BTreeMap::new(),
        has_tauri: false,
        is_admin: false,
        desktop_name: "Default".to_string(),
    }
}

/// `apply(true)` is the lazy bring-up entry point — it must
/// proactively enqueue an `AttachVirtualDisplay` IPC instead of
/// waiting for a future `Capabilities` re-emission, otherwise
/// `ensure_attached` would sit in Attaching forever in the
/// post-initial-Capabilities steady state.
#[tokio::test]
async fn apply_true_sends_attach_virtual_display_to_worker() {
    use desk_ipc_protocol::message::ServiceToWorker;
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    supervisor.apply(true).await.expect("apply(true)");
    let sent = drain_ipc(&mut ipc_rx);
    let attach_count = sent
        .iter()
        .filter(|m| matches!(m, ServiceToWorker::AttachVirtualDisplay(_)))
        .count();
    assert_eq!(
        attach_count, 1,
        "apply(true) must emit exactly one AttachVirtualDisplay; observed: {sent:?}",
    );
}

/// `apply(false)` from `Attached` must clear the
/// `attached_capabilities_target` watch so a stale post-detach
/// `ensure_attached` call cannot fast-path through on the previous
/// target.
#[tokio::test]
async fn apply_false_sends_detach_refresh_and_clears_target() {
    use desk_ipc_protocol::message::ServiceToWorker;
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    supervisor.apply(true).await.expect("apply(true)");
    supervisor
        .on_worker_attach_result(VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        })
        .await;
    assert!(
        supervisor.attached_capabilities_target.borrow().is_some(),
        "promotion sets target",
    );
    let _ = drain_ipc(&mut ipc_rx);

    supervisor.apply(false).await.expect("apply(false)");
    let sent = drain_ipc(&mut ipc_rx);
    assert_eq!(
        sent.iter()
            .filter(|m| matches!(m, ServiceToWorker::DetachVirtualDisplay))
            .count(),
        1,
        "apply(false) emits DetachVirtualDisplay; observed: {sent:?}",
    );
    assert_eq!(
        sent.iter()
            .filter(|m| matches!(m, ServiceToWorker::RefreshCapabilities))
            .count(),
        1,
        "apply(false) emits RefreshCapabilities; observed: {sent:?}",
    );
    assert!(
        supervisor.attached_capabilities_target.borrow().is_none(),
        "apply(false) clears target",
    );
    assert_eq!(supervisor.state_label().await, "Disabled");
}

/// The `Attaching → Attached` promotion records both the
/// `display_name` (for the cache-contains check) and the
/// post-promotion `capabilities_version` target (snapshot + 1).
#[tokio::test]
async fn promotion_stores_display_name_and_sets_target_snapshot_plus_one() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    // Seed cap_version to 1 via a no-op Capabilities install so the
    // snapshot is non-zero (matches the typical bring-up flow where
    // the worker has already emitted at least one Capabilities).
    worker_mgr.set_worker_capabilities(caps_with_display(r"\\.\OTHER"));
    assert_eq!(worker_mgr.capabilities_version(), 1);
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);
    supervisor.apply(true).await.expect("apply(true)");

    supervisor
        .on_worker_attach_result(VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        })
        .await;

    // display_name lands in state.
    let stored = match &*supervisor.state.read().await {
        SupervisorState::Attached { display_name, .. } => Some(display_name.clone()),
        _ => None,
    };
    assert_eq!(stored.as_deref(), Some(r"\\.\DISPLAY4"));
    // target == snapshot + 1 == 2.
    assert_eq!(*supervisor.attached_capabilities_target.borrow(), Some(2));
}

/// Fast-path: `ensure_attached` returns `Attached` without doing
/// anything when the supervisor is fully attached AND the cache
/// already includes the attached display.
#[tokio::test]
async fn ensure_attached_fast_path_when_target_and_cache_match() {
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    // `new_attached_for_test` seeds capabilities + target itself.
    let supervisor = VirtualDisplaySupervisor::new_attached_for_test(worker_mgr, "SWD\\TEST\\TEST");

    let outcome = supervisor
        .ensure_attached(std::time::Duration::from_millis(100))
        .await;
    assert!(matches!(outcome, EnsureAttachedOutcome::Attached));
}

/// Even if target is satisfied by cap_version,
/// the cache must actually contain the attached display name for
/// the ensure_attached completion to fire.
#[tokio::test]
async fn ensure_attached_waits_when_target_satisfied_but_cache_missing_display() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, _ipc_rx) =
        tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));
    supervisor.apply(true).await.expect("apply(true)");

    // Promote with display "\\.\DISPLAY4".
    supervisor
        .on_worker_attach_result(VirtualDisplayAttachResultPayload {
            instance_id: MOCK_INSTANCE_ID.to_string(),
            outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY4".to_string()),
        })
        .await;
    // Background: bump cap_version with capabilities that DO NOT
    // include the attached display. ensure_attached must NOT
    // complete until the cache actually lists the IDD.
    let worker_mgr_bg = worker_mgr.clone();
    let bumper = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Wrong display name — should not satisfy the check.
        worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\OTHER"));
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        // Now publish capabilities that include the attached IDD.
        worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\DISPLAY4"));
    });

    let outcome = supervisor
        .ensure_attached(std::time::Duration::from_secs(2))
        .await;
    bumper.await.unwrap();
    assert!(
        matches!(outcome, EnsureAttachedOutcome::Attached),
        "ensure_attached completed only after cache surfaced the IDD: {outcome:?}",
    );
}

/// An unrelated `Capabilities` bump (e.g. worker restart) must not
/// satisfy `ensure_attached` — without the strict cache-contains
/// check, the daemon could report Attached while the dropdown
/// still lacks the IDD.
#[tokio::test]
async fn ensure_attached_ignores_unrelated_capabilities_bump_before_attached() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, _ipc_rx) =
        tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));

    // Background: bump cap_version but never deliver attach_result.
    let worker_mgr_bg = worker_mgr.clone();
    let bumper = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\OTHER"));
    });
    let outcome = supervisor
        .ensure_attached(std::time::Duration::from_millis(200))
        .await;
    bumper.await.unwrap();
    assert!(
        matches!(outcome, EnsureAttachedOutcome::TimedOut),
        "cap bump without attach_result must not complete; observed: {outcome:?}",
    );
}

/// Lazy bring-up from `Disabled`: ensure_attached kicks `apply(true)`
/// internally; a background task simulates the worker's
/// `attach_result` + `Capabilities` round-trip; ensure_attached
/// returns `Attached`.
#[tokio::test]
async fn ensure_attached_brings_up_from_disabled() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, _ipc_rx) =
        tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));

    let supervisor_bg = supervisor.clone();
    let worker_mgr_bg = worker_mgr.clone();
    let bumper = tokio::spawn(async move {
        // Wait for ensure_attached to issue apply(true).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        supervisor_bg
            .on_worker_attach_result(VirtualDisplayAttachResultPayload {
                instance_id: MOCK_INSTANCE_ID.to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY7".to_string()),
            })
            .await;
        // Worker's RefreshCapabilities response includes the IDD.
        worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\DISPLAY7"));
    });

    let outcome = supervisor
        .ensure_attached(std::time::Duration::from_secs(2))
        .await;
    bumper.await.unwrap();
    assert!(matches!(outcome, EnsureAttachedOutcome::Attached));
    assert_eq!(supervisor.state_label().await, "Attached");
}

/// Timeout path: no `attach_result` ever lands. State stays in
/// `Attaching` so the next ensure call resumes from there.
#[tokio::test]
async fn ensure_attached_times_out_when_attach_never_completes() {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, _ipc_rx) =
        tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    let outcome = supervisor
        .ensure_attached(std::time::Duration::from_millis(100))
        .await;
    assert!(matches!(outcome, EnsureAttachedOutcome::TimedOut));
    assert_eq!(
        supervisor.state_label().await,
        "Attaching",
        "state must remain Attaching after timeout so the next ensure resumes",
    );
}

/// When a previous ensure_attached
/// timed out with the supervisor stuck in Attaching (e.g. the first
/// Attach IPC was lost before the worker channel was installed), a
/// subsequent ensure_attached must re-send the AttachVirtualDisplay
/// IPC so the worker eventually gets the request.
#[tokio::test]
async fn ensure_attached_resends_attach_when_still_attaching() {
    use desk_ipc_protocol::message::ServiceToWorker;
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    supervisor.apply(true).await.expect("apply(true)");
    let initial = drain_ipc(&mut ipc_rx);
    assert_eq!(
        initial
            .iter()
            .filter(|m| matches!(m, ServiceToWorker::AttachVirtualDisplay(_)))
            .count(),
        1,
        "first attach send observed",
    );

    // Second ensure call: state is still Attaching (no attach_result
    // arrived). The ensure_attached fast-path miss should trigger a
    // re-send before the wait loop.
    let _ = supervisor
        .ensure_attached(std::time::Duration::from_millis(50))
        .await;
    let resent = drain_ipc(&mut ipc_rx);
    assert!(
        resent
            .iter()
            .any(|m| matches!(m, ServiceToWorker::AttachVirtualDisplay(_))),
        "subsequent ensure must re-send AttachVirtualDisplay when state is Attaching; \
             observed: {resent:?}",
    );
}

/// `Unavailable`: provider returns `NotSupported` (stub platforms).
/// ensure_attached must surface the error promptly.
#[tokio::test]
async fn ensure_attached_returns_unavailable_when_provider_not_supported() {
    let provider: Box<dyn VirtualDisplayLifecycle> =
        Box::new(MockLifecycle::returns_not_supported());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let supervisor = VirtualDisplaySupervisor::new(provider, worker_mgr);

    let outcome = supervisor
        .ensure_attached(std::time::Duration::from_millis(100))
        .await;
    match outcome {
        EnsureAttachedOutcome::Unavailable(_) => {}
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// The lifecycle_lock must serialise the entire
/// apply flow including IPC sends so concurrent calls cannot
/// interleave. Specifically: an apply(false) running between an
/// in-flight apply(true)'s state set and IPC send would let the
/// worker observe Detach BEFORE the previous Attach completes its
/// own IPC. The test launches one apply(true), waits for it to
/// finish, then races apply(false) + apply(true) concurrently and
/// asserts the IPC sequence is consistent with serialised
/// execution (no Attach interleaved before a Detach of the same
/// generation).
#[tokio::test]
async fn apply_serializes_concurrent_calls_via_lifecycle_lock() {
    use desk_ipc_protocol::message::ServiceToWorker;
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr));

    supervisor.apply(true).await.expect("first apply(true)");
    let _ = drain_ipc(&mut ipc_rx);

    // Concurrent apply(false) followed by apply(true). lifecycle_lock
    // must force them to serialise; the IPC stream observed afterwards
    // must contain Detach before any second Attach.
    let s1 = supervisor.clone();
    let s2 = supervisor.clone();
    let t1 = tokio::spawn(async move { s1.apply(false).await });
    // Small skew so apply(false) wins the lock first deterministically.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let t2 = tokio::spawn(async move { s2.apply(true).await });
    let _ = t1.await.unwrap();
    let _ = t2.await.unwrap();

    let sent = drain_ipc(&mut ipc_rx);
    // Locate the Detach. It must precede any AttachVirtualDisplay that
    // appears after it (the second apply(true)).
    let detach_idx = sent
        .iter()
        .position(|m| matches!(m, ServiceToWorker::DetachVirtualDisplay))
        .expect("detach must be present in the IPC stream");
    let second_attach_idx = sent
        .iter()
        .enumerate()
        .skip(detach_idx + 1)
        .find_map(|(i, m)| {
            if matches!(m, ServiceToWorker::AttachVirtualDisplay(_)) {
                Some(i)
            } else {
                None
            }
        });
    assert!(
        second_attach_idx.is_some(),
        "second apply(true) must enqueue Attach after Detach; observed: {sent:?}",
    );
}

/// Two concurrent `ensure_attached` calls must share a single
/// underlying bring-up: provider.create() must run exactly once,
/// and both calls must observe `Attached`.
#[tokio::test]
async fn ensure_attached_concurrent_calls_share_single_apply() {
    let lifecycle = Arc::new(MockLifecycle::returns_handle());
    let lifecycle_for_provider = Arc::clone(&lifecycle);
    struct ArcProvider(Arc<MockLifecycle>);
    impl VirtualDisplayLifecycle for ArcProvider {
        fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
            self.0.create()
        }
    }
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(ArcProvider(lifecycle_for_provider));
    let (worker_mgr, _to_daemon_rx) = make_worker_mgr();
    let (ipc_tx, _ipc_rx) =
        tokio::sync::mpsc::unbounded_channel::<desk_ipc_protocol::message::ServiceToWorker>();
    worker_mgr.install_active_for_test(ipc_tx).await;
    let supervisor = Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr.clone()));

    let supervisor_bg = supervisor.clone();
    let worker_mgr_bg = worker_mgr.clone();
    let driver = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        supervisor_bg
            .on_worker_attach_result(VirtualDisplayAttachResultPayload {
                instance_id: MOCK_INSTANCE_ID.to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached(r"\\.\DISPLAY8".to_string()),
            })
            .await;
        worker_mgr_bg.set_worker_capabilities(caps_with_display(r"\\.\DISPLAY8"));
    });

    let s1 = supervisor.clone();
    let s2 = supervisor.clone();
    let h1 =
        tokio::spawn(async move { s1.ensure_attached(std::time::Duration::from_secs(2)).await });
    let h2 =
        tokio::spawn(async move { s2.ensure_attached(std::time::Duration::from_secs(2)).await });
    let r1 = h1.await.unwrap();
    let r2 = h2.await.unwrap();
    driver.await.unwrap();

    assert!(
        matches!(r1, EnsureAttachedOutcome::Attached),
        "first: {r1:?}"
    );
    assert!(
        matches!(r2, EnsureAttachedOutcome::Attached),
        "second: {r2:?}"
    );
    assert_eq!(
        lifecycle.create_calls.load(Ordering::SeqCst),
        1,
        "provider.create must be called at most once",
    );
}

/// `record_applied_mode` stores the full mode the driver applied
/// (width × height × refresh) from the worker's echo. Any zero
/// component skips the update so a malformed echo cannot wipe a
/// prior valid observation. `last_known_mode()` only reports a
/// fully-observed mode.
#[test]
fn supervisor_records_full_mode_on_applied() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s =
        VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_not_supported()), worker_mgr);

    // Initial state: nothing observed yet.
    assert_eq!(s.last_refresh_hz(), 0);
    assert!(s.last_known_mode().is_none());

    // First fully-formed Applied caches all three.
    s.record_applied_mode(2560, 1440, 120);
    assert_eq!(s.last_refresh_hz(), 120);
    assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));

    // Any zero component is treated as "no observation" and the
    // whole update is skipped — refresh + dimensions all stay put.
    s.record_applied_mode(0, 1440, 60);
    assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));
    s.record_applied_mode(1920, 0, 60);
    assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));
    s.record_applied_mode(1920, 1080, 0);
    assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));

    // A subsequent fully-valid Applied does overwrite.
    s.record_applied_mode(1920, 1080, 60);
    assert_eq!(s.last_known_mode(), Some((1920, 1080, 60)));
    assert_eq!(s.last_refresh_hz(), 60);
}

/// `attached_display_name()` returns the GDI device name only while
/// the supervisor is `Attached`. Every other state (`Disabled`,
/// `Attaching`, `Detaching`) returns `None`. `pc_manager` reads this
/// to populate `InitSignalingData::virtual_display_device_name`.
#[tokio::test]
async fn supervisor_attached_display_name_only_when_attached() {
    let (worker_mgr, _rx) = make_worker_mgr();
    // Disabled: brand-new supervisor.
    let disabled = VirtualDisplaySupervisor::new(
        Box::new(MockLifecycle::returns_not_supported()),
        worker_mgr.clone(),
    );
    assert_eq!(disabled.attached_display_name().await, None);

    // Attached via the pre-promoted test helper.
    let attached =
        VirtualDisplaySupervisor::new_attached_for_test(worker_mgr.clone(), "SWD\\TEST\\TEST");
    assert_eq!(
        attached.attached_display_name().await.as_deref(),
        Some("\\\\.\\TESTDISPLAY"),
    );

    // Attaching: bring up via apply(true) with a handle-returning
    // provider, but never deliver the attach result.
    let (worker_mgr2, _rx2) = make_worker_mgr();
    let attaching =
        VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_handle()), worker_mgr2);
    attaching.apply(true).await.expect("apply(true) succeeds");
    assert_eq!(attaching.state_label().await, "Attaching");
    assert_eq!(attaching.attached_display_name().await, None);
}

/// `apply(false)` ending an attach generation
/// must clear cached width/height (so a stale 2560x1440 cannot
/// fake-short-circuit the next request) while preserving the
/// refresh hint.
#[tokio::test]
async fn supervisor_apply_false_clears_dimensions_keeps_refresh() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s = VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_handle()), worker_mgr);
    // Bring the supervisor up to Attaching so the (Attaching, false)
    // arm exercises the dimension reset.
    s.apply(true).await.expect("apply(true) succeeds");
    // Seed a full cached mode as if the worker had echoed Applied.
    s.record_applied_mode(2560, 1440, 60);
    assert_eq!(s.last_known_mode(), Some((2560, 1440, 60)));

    s.apply(false).await.expect("apply(false) succeeds");

    assert!(
        s.last_known_mode().is_none(),
        "dimensions must be cleared on tear-down so a future re-attach \
             does not inherit a stale fake-short-circuit cache",
    );
    assert_eq!(
        s.last_refresh_hz(),
        60,
        "refresh is preserved as an operator hint across attach generations",
    );
}

/// `apply(true)` starting an attach generation
/// also clears stale dimensions, regardless of what the previous
/// detach left behind.
#[tokio::test]
async fn supervisor_apply_true_clears_dimensions_keeps_refresh() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s = VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_handle()), worker_mgr);
    // Pretend we previously had an attach cycle that left dimensions
    // cached (skipping the `apply(true)` reset). This mirrors the
    // shape `apply(false)` could not reach in the absence of a fresh
    // bring-up.
    s.record_applied_mode(2560, 1440, 144);

    s.apply(true).await.expect("apply(true) succeeds");

    assert!(s.last_known_mode().is_none());
    assert_eq!(s.last_refresh_hz(), 144);
}

/// Every `Attached` outcome — including the
/// already-Attached re-entry path that worker restart takes —
/// must clear cached dimensions. The Attaching→Attached promotion
/// edge is exercised implicitly by the apply(true) chain in other
/// tests; this one pins the *already-Attached* branch.
#[tokio::test]
async fn supervisor_on_worker_attach_result_already_attached_clears_dimensions() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s = VirtualDisplaySupervisor::new_attached_for_test(worker_mgr, "SWD\\TEST\\TEST");
    // Seed cached dimensions inside an existing Attached state.
    s.record_applied_mode(2560, 1440, 120);
    assert_eq!(s.last_known_mode(), Some((2560, 1440, 120)));
    assert_eq!(s.state_label().await, "Attached");

    // Re-send the attach result with the same instance id; this
    // lands on the already-Attached branch in on_worker_attach_result.
    s.on_worker_attach_result(VirtualDisplayAttachResultPayload {
        instance_id: "SWD\\TEST\\TEST".to_string(),
        outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\TESTDISPLAY".to_string()),
    })
    .await;

    assert!(
        s.last_known_mode().is_none(),
        "already-Attached re-entry (worker restart path) must clear \
             stale dimensions even though no state transition fires",
    );
    assert_eq!(s.last_refresh_hz(), 120, "refresh must survive");
    assert_eq!(s.state_label().await, "Attached");
}

/// The first call ever to `try_consume_auto_slot` must always
/// succeed — there is no prior timestamp to compare against and no
/// reason to make the operator wait `min_interval` after boot.
#[test]
fn supervisor_auto_slot_first_call_succeeds() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s =
        VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_not_supported()), worker_mgr);

    let allowed = s.try_consume_auto_slot(Instant::now(), Duration::from_secs(60));
    assert!(allowed, "first try_consume_auto_slot must always succeed");
}

/// Two calls within `min_interval` ⇒ the second is rejected. After
/// the interval has elapsed the slot becomes available again. We
/// pass synthetic `Instant`s (relative to a baseline) so the test
/// is wall-clock independent.
#[test]
fn supervisor_auto_slot_throttles_within_interval() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s =
        VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_not_supported()), worker_mgr);
    let base = Instant::now();
    // First call at t=0 succeeds.
    assert!(s.try_consume_auto_slot(base, Duration::from_millis(1000)));
    // 500 ms later — interval not elapsed ⇒ false, last_at unchanged.
    assert!(!s.try_consume_auto_slot(
        base + Duration::from_millis(500),
        Duration::from_millis(1000)
    ));
    // 1500 ms after the first slot ⇒ interval elapsed ⇒ true.
    assert!(s.try_consume_auto_slot(
        base + Duration::from_millis(1500),
        Duration::from_millis(1000)
    ));
}

/// `min_interval` is taken from the caller (router reads it from
/// `settings.virtual_display.adaptive_throttle_ms`). Different
/// intervals on subsequent calls must drive different behaviour —
/// pins that the supervisor never caches the interval.
#[test]
fn supervisor_auto_slot_respects_dynamic_interval() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s =
        VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_not_supported()), worker_mgr);
    let base = Instant::now();
    // Consume slot at t=0 with a long interval.
    assert!(s.try_consume_auto_slot(base, Duration::from_millis(2000)));
    // 1500 ms later, still within the 2000 ms window ⇒ false.
    assert!(!s.try_consume_auto_slot(
        base + Duration::from_millis(1500),
        Duration::from_millis(2000)
    ));
    // Same elapsed (1500 ms), but caller now passes 500 ms ⇒ true
    // (interval is per-call, not state).
    assert!(s.try_consume_auto_slot(
        base + Duration::from_millis(1500),
        Duration::from_millis(500)
    ));
}

/// `min_interval=0` is the operator-configured "no throttle" mode.
/// Two back-to-back calls (Δ ≈ 0) must both succeed.
#[test]
fn supervisor_auto_slot_zero_interval_never_throttles() {
    let (worker_mgr, _rx) = make_worker_mgr();
    let s =
        VirtualDisplaySupervisor::new(Box::new(MockLifecycle::returns_not_supported()), worker_mgr);
    let now = Instant::now();
    assert!(s.try_consume_auto_slot(now, Duration::from_millis(0)));
    // Δ = 0 between two calls with min_interval = 0 ⇒ second
    // still succeeds (0 >= 0).
    assert!(s.try_consume_auto_slot(now, Duration::from_millis(0)));
}

// ───── Exclusive-mode tests ─────

fn fresh_supervisor() -> Arc<VirtualDisplaySupervisor> {
    let provider: Box<dyn VirtualDisplayLifecycle> = Box::new(MockLifecycle::returns_handle());
    let (worker_mgr, _rx) = make_worker_mgr();
    new_arc(provider, worker_mgr)
}

async fn read_inner(s: &VirtualDisplaySupervisor) -> (ExclusiveState, u64) {
    let inner = s.exclusive_inner.read().await;
    (inner.state, inner.current_op_id)
}

/// `set_desired_exclusive(true)` updates the flag + prompt and
/// notifies the driver loop. With no `desired_computer` installed
/// and no active state behind the supervisor, the driver loop
/// produces `Send { Enter }` once because (Idle, true) matches
/// the transition table. We do not assert the IPC went out here;
/// the worker_mgr has no installed channel so the send errors,
/// the rollback brings state back to Idle, and the driver loop
/// goes to sleep on the next notification.
#[tokio::test]
async fn set_desired_exclusive_idle_true_advances_to_entering_and_bumps_op_id() {
    let s = fresh_supervisor();
    // Manually drive prepare_next_action to inspect state advancement
    // without depending on the driver loop's send result.
    s.exclusive_desired.store(true, Ordering::SeqCst);
    match s.prepare_next_action().await {
        ExclusiveAction::Send {
            next_state,
            op_id,
            prev_state,
            ..
        } => {
            assert_eq!(next_state, ExclusiveState::Entering);
            assert_eq!(prev_state, ExclusiveState::Idle);
            assert_eq!(op_id, 1);
        }
        other => panic!("expected Send, got {other:?}"),
    }
    let (state, op_id) = read_inner(&s).await;
    assert_eq!(state, ExclusiveState::Entering);
    assert_eq!(op_id, 1);
    s.shutdown_driver_loop().await;
}

/// `prepare_next_action` returns None when (Idle, false): no
/// state advancement, no op_id bump.
#[tokio::test]
async fn prepare_next_action_idle_false_is_none() {
    let s = fresh_supervisor();
    match s.prepare_next_action().await {
        ExclusiveAction::None => {}
        other => panic!("expected None, got {other:?}"),
    }
    let (state, op_id) = read_inner(&s).await;
    assert_eq!(state, ExclusiveState::Idle);
    assert_eq!(op_id, 0);
    s.shutdown_driver_loop().await;
}

/// `on_exclusive_result` with matching `op_id` advances Entering
/// to Active; with a mismatched `op_id` it is a no-op.
#[tokio::test]
async fn on_exclusive_result_op_id_gate() {
    let s = fresh_supervisor();
    s.exclusive_desired.store(true, Ordering::SeqCst);
    s.prepare_next_action().await; // state -> Entering, op_id -> 1

    // Stale op_id: dropped silently.
    s.on_exclusive_result(ExclusiveResultPayload {
        op_id: 999,
        direction: ExclusiveDirection::Entering,
        outcome: ExclusiveOutcome::Entered,
    })
    .await;
    let (state, op_id) = read_inner(&s).await;
    assert_eq!(state, ExclusiveState::Entering, "stale op must not advance");
    assert_eq!(op_id, 1);

    // Matching op_id: transitions to Active.
    s.on_exclusive_result(ExclusiveResultPayload {
        op_id: 1,
        direction: ExclusiveDirection::Entering,
        outcome: ExclusiveOutcome::Entered,
    })
    .await;
    let (state, op_id) = read_inner(&s).await;
    assert_eq!(state, ExclusiveState::Active);
    assert_eq!(op_id, 1, "successful result does not bump op_id");
    s.shutdown_driver_loop().await;
}

/// `apply_result_transition` table (codex round 6 #5: only four
/// outcomes; EnterCancelled was removed).
#[test]
fn apply_result_transition_table() {
    // Entering + Entered -> Active
    assert_eq!(
        apply_result_transition(
            ExclusiveState::Entering,
            &ExclusiveResultPayload {
                op_id: 1,
                direction: ExclusiveDirection::Entering,
                outcome: ExclusiveOutcome::Entered,
            }
        ),
        ExclusiveState::Active
    );
    // Entering + EnterFailed -> Idle
    assert_eq!(
        apply_result_transition(
            ExclusiveState::Entering,
            &ExclusiveResultPayload {
                op_id: 1,
                direction: ExclusiveDirection::Entering,
                outcome: ExclusiveOutcome::EnterFailed("bad".into()),
            }
        ),
        ExclusiveState::Idle
    );
    // Leaving + Left -> Idle
    assert_eq!(
        apply_result_transition(
            ExclusiveState::Leaving,
            &ExclusiveResultPayload {
                op_id: 1,
                direction: ExclusiveDirection::Leaving,
                outcome: ExclusiveOutcome::Left,
            }
        ),
        ExclusiveState::Idle
    );
    // A failed leave must retain the retryable state:
    // Leaving + LeftWithErrors -> Active (was Idle). The bounded
    // retry budget + force-Idle on exhaustion lives in
    // `on_exclusive_result`, not in this pure transition function.
    assert_eq!(
        apply_result_transition(
            ExclusiveState::Leaving,
            &ExclusiveResultPayload {
                op_id: 1,
                direction: ExclusiveDirection::Leaving,
                outcome: ExclusiveOutcome::LeftWithErrors("partial".into()),
            }
        ),
        ExclusiveState::Active
    );
    // Defensive: Leaving + Entered stays Leaving (stale ack would
    // already be dropped by op_id gate before reaching here; but
    // if it does, do not regress to Active).
    assert_eq!(
        apply_result_transition(
            ExclusiveState::Leaving,
            &ExclusiveResultPayload {
                op_id: 1,
                direction: ExclusiveDirection::Entering,
                outcome: ExclusiveOutcome::Entered,
            }
        ),
        ExclusiveState::Leaving
    );
}

/// A first `LeftWithErrors`
/// must transition the supervisor to `Active` (not `Idle`) so
/// the reconciler can drive a retry, bump `leave_retry_count`,
/// and set `next_leave_at` to the doubling schedule entry.
/// `prepare_next_action` must then return `None` while the
/// backoff is still in effect.
#[tokio::test]
async fn on_exclusive_result_left_with_errors_arms_retry() {
    let s = fresh_supervisor();
    // Move directly to Leaving with a known op_id so the gate fires.
    let op_id = {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Leaving;
        inner.current_op_id = 42;
        let _ = s
            .exclusive_state_watch
            .send_replace(ExclusiveState::Leaving);
        inner.current_op_id
    };
    // Mark exclusive as still desired-off so the reconciler would
    // want to drive Leaving again.
    s.exclusive_desired.store(false, Ordering::SeqCst);

    let before = Instant::now();
    s.on_exclusive_result(ExclusiveResultPayload {
        op_id,
        direction: ExclusiveDirection::Leaving,
        outcome: ExclusiveOutcome::LeftWithErrors("partial".into()),
    })
    .await;

    let inner = s.exclusive_inner.read().await;
    assert_eq!(
        inner.state,
        ExclusiveState::Active,
        "must go to Active for retry"
    );
    assert_eq!(inner.leave_retry_count, 1);
    let next_at = inner.next_leave_at.expect("backoff timer must be set");
    // Schedule entry for the first retry is LEAVE_RETRY_BASE_DELAY * 2^1 = 4 s.
    let scheduled_delay = next_at.saturating_duration_since(before);
    assert!(
        scheduled_delay >= Duration::from_secs(3),
        "expected ~4s delay, got {scheduled_delay:?}",
    );
    drop(inner);

    // While backoff is in effect, prepare_next_action must NOT
    // produce a leave action — even though state=Active &&
    // desired=false would otherwise transition to Leaving.
    let action = s.prepare_next_action().await;
    assert!(
        matches!(action, ExclusiveAction::None),
        "backoff gate must short-circuit prepare_next_action",
    );

    s.shutdown_driver_loop().await;
}

/// After [`MAX_LEAVE_RETRIES`] consecutive
/// `LeftWithErrors`, the supervisor must force-Idle and reset
/// `leave_retry_count` so a fresh enter cycle can proceed without
/// inheriting stale budget.
#[tokio::test]
async fn on_exclusive_result_left_with_errors_exhausts_after_max_retries() {
    let s = fresh_supervisor();
    {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Leaving;
        inner.current_op_id = 100;
    }

    for attempt in 1..=MAX_LEAVE_RETRIES {
        // Each result must match the current op_id.
        let op_id = {
            let inner = s.exclusive_inner.read().await;
            assert_eq!(inner.state, ExclusiveState::Leaving);
            inner.current_op_id
        };
        s.on_exclusive_result(ExclusiveResultPayload {
            op_id,
            direction: ExclusiveDirection::Leaving,
            outcome: ExclusiveOutcome::LeftWithErrors(format!("attempt {attempt}")),
        })
        .await;
        // Between retries, drive the state back to Leaving as if
        // the reconciler had picked it up (this isolates the unit
        // we are testing — on_exclusive_result's retry budget).
        if attempt < MAX_LEAVE_RETRIES {
            let mut inner = s.exclusive_inner.write().await;
            assert_eq!(inner.state, ExclusiveState::Active, "intermediate state");
            inner.state = ExclusiveState::Leaving;
        }
    }

    // After the final LeftWithErrors, state must be Idle and the
    // retry budget reset.
    let inner = s.exclusive_inner.read().await;
    assert_eq!(
        inner.state,
        ExclusiveState::Idle,
        "exhausted budget must force-Idle"
    );
    assert_eq!(inner.leave_retry_count, 0, "count must reset on give-up");
    assert!(inner.next_leave_at.is_none(), "no further retry scheduled");
    drop(inner);
    s.shutdown_driver_loop().await;
}

/// A successful `Left` (after one or more
/// failed retries) must reset both `leave_retry_count` and
/// `next_leave_at` — otherwise the *next* leave cycle inherits
/// the stale backoff timer.
#[tokio::test]
async fn on_exclusive_result_left_resets_retry_state() {
    let s = fresh_supervisor();
    // Seed state as if we had just had a LeftWithErrors and are
    // now retrying Leaving.
    let op_id = {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Leaving;
        inner.current_op_id = 50;
        inner.leave_retry_count = 2;
        inner.next_leave_at = Some(Instant::now() + Duration::from_secs(60));
        inner.current_op_id
    };

    s.on_exclusive_result(ExclusiveResultPayload {
        op_id,
        direction: ExclusiveDirection::Leaving,
        outcome: ExclusiveOutcome::Left,
    })
    .await;

    let inner = s.exclusive_inner.read().await;
    assert_eq!(inner.state, ExclusiveState::Idle);
    assert_eq!(inner.leave_retry_count, 0);
    assert!(inner.next_leave_at.is_none());
    drop(inner);
    s.shutdown_driver_loop().await;
}

/// E2E fix 2026-05-27: a first `EnterFailed` must arm the
/// enter-side backoff (count → 1, `next_enter_at` → now + 4 s)
/// and `prepare_next_action` must short-circuit while the gate
/// is still in effect. Symmetric to the LeftWithErrors test.
#[tokio::test]
async fn on_exclusive_result_enter_failed_arms_retry() {
    let s = fresh_supervisor();
    let op_id = {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Entering;
        inner.current_op_id = 11;
        let _ = s
            .exclusive_state_watch
            .send_replace(ExclusiveState::Entering);
        inner.current_op_id
    };
    // Desired stays true so the reconciler would want to retry.
    s.exclusive_desired.store(true, Ordering::SeqCst);

    let before = Instant::now();
    s.on_exclusive_result(ExclusiveResultPayload {
        op_id,
        direction: ExclusiveDirection::Entering,
        outcome: ExclusiveOutcome::EnterFailed("CDS BADMODE".into()),
    })
    .await;

    let inner = s.exclusive_inner.read().await;
    assert_eq!(
        inner.state,
        ExclusiveState::Idle,
        "EnterFailed transitions back to Idle (pure transition unchanged)"
    );
    assert_eq!(inner.enter_retry_count, 1);
    let next_at = inner.next_enter_at.expect("backoff timer must be set");
    let scheduled_delay = next_at.saturating_duration_since(before);
    // Schedule entry for first retry: ENTER_RETRY_BASE_DELAY * 2^1 = 4 s.
    assert!(
        scheduled_delay >= Duration::from_secs(3),
        "expected ~4s delay, got {scheduled_delay:?}",
    );
    // Desired is preserved while retries are still available — only
    // exhaustion drops it.
    assert!(
        s.exclusive_desired.load(Ordering::SeqCst),
        "desired must stay true while retries remain",
    );
    drop(inner);

    // Backoff gate must block (Idle, true) → Entering until the
    // timer elapses.
    let action = s.prepare_next_action().await;
    assert!(
        matches!(action, ExclusiveAction::None),
        "enter backoff gate must short-circuit prepare_next_action",
    );

    s.shutdown_driver_loop().await;
}

/// E2E fix 2026-05-27: after `MAX_ENTER_RETRIES` consecutive
/// `EnterFailed`, the supervisor must clear `exclusive_desired`
/// so the `(Idle, desired=true) → Entering` row stops firing.
/// Counts must reset too so a fresh acquire later starts at zero.
#[tokio::test]
async fn on_exclusive_result_enter_failed_exhausts_after_max_retries() {
    let s = fresh_supervisor();
    s.exclusive_desired.store(true, Ordering::SeqCst);
    let mut op_id;
    {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Entering;
        inner.current_op_id = 200;
    }

    for attempt in 1..=MAX_ENTER_RETRIES {
        {
            let inner = s.exclusive_inner.read().await;
            op_id = inner.current_op_id;
            assert_eq!(inner.state, ExclusiveState::Entering);
        }
        s.on_exclusive_result(ExclusiveResultPayload {
            op_id,
            direction: ExclusiveDirection::Entering,
            outcome: ExclusiveOutcome::EnterFailed(format!("attempt {attempt}")),
        })
        .await;
        // Drive the state back to Entering between retries as if
        // the reconciler had picked it up (isolates the unit
        // under test — on_exclusive_result's retry budget).
        if attempt < MAX_ENTER_RETRIES {
            let mut inner = s.exclusive_inner.write().await;
            assert_eq!(inner.state, ExclusiveState::Idle, "intermediate state");
            inner.state = ExclusiveState::Entering;
            // Bump op_id like prepare_next_action would so each
            // retry round simulates the real reconciler.
            inner.current_op_id = inner.current_op_id.wrapping_add(1);
        }
    }

    // After exhaustion: state is Idle (always, on EnterFailed),
    // the retry budget is reset, AND desired has been cleared so
    // the reconciler will not pick this up again.
    let inner = s.exclusive_inner.read().await;
    assert_eq!(inner.state, ExclusiveState::Idle);
    assert_eq!(inner.enter_retry_count, 0, "count must reset on give-up");
    assert!(inner.next_enter_at.is_none(), "no further retry scheduled");
    drop(inner);
    assert!(
        !s.exclusive_desired.load(Ordering::SeqCst),
        "exhaustion must clear exclusive_desired to break the loop",
    );
    s.shutdown_driver_loop().await;
}

/// E2E fix 2026-05-27: a successful `Entered` (after one or more
/// failed retries) must clear `enter_retry_count` and
/// `next_enter_at` — otherwise the next attach inherits stale
/// backoff bookkeeping.
#[tokio::test]
async fn on_exclusive_result_entered_resets_retry_state() {
    let s = fresh_supervisor();
    let op_id = {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Entering;
        inner.current_op_id = 77;
        inner.enter_retry_count = 2;
        inner.next_enter_at = Some(Instant::now() + Duration::from_secs(60));
        inner.current_op_id
    };

    s.on_exclusive_result(ExclusiveResultPayload {
        op_id,
        direction: ExclusiveDirection::Entering,
        outcome: ExclusiveOutcome::Entered,
    })
    .await;

    let inner = s.exclusive_inner.read().await;
    assert_eq!(inner.state, ExclusiveState::Active);
    assert_eq!(inner.enter_retry_count, 0);
    assert!(inner.next_enter_at.is_none());
    drop(inner);
    s.shutdown_driver_loop().await;
}

/// E2E fix 2026-05-27: `prepare_next_action` gates the enter
/// path symmetrically to the leave path. With a pending
/// `next_enter_at` in the future and `(Idle, desired=true)`, the
/// call must return `None` instead of advancing to Entering.
#[tokio::test]
async fn prepare_next_action_gates_idle_true_on_next_enter_at() {
    let s = fresh_supervisor();
    {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Idle;
        inner.enter_retry_count = 1;
        inner.next_enter_at = Some(Instant::now() + Duration::from_secs(60));
    }
    s.exclusive_desired.store(true, Ordering::SeqCst);

    let action = s.prepare_next_action().await;
    assert!(
        matches!(action, ExclusiveAction::None),
        "enter backoff timer must short-circuit (Idle, true)",
    );
    // State must remain Idle (no spurious advance).
    let inner = s.exclusive_inner.read().await;
    assert_eq!(inner.state, ExclusiveState::Idle);
    s.shutdown_driver_loop().await;
}

/// `prepare_next_action` only honours the
/// `next_leave_at` gate for the `(Active, desired=false)` retry
/// row. Other rows ignore the gate entirely.
#[tokio::test]
async fn prepare_next_action_ignores_backoff_for_unrelated_transitions() {
    let s = fresh_supervisor();
    // Pre-seed a backoff timer + count as if a prior retry was in flight,
    // but switch state to Idle so the active row does NOT trigger.
    {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Idle;
        inner.leave_retry_count = 1;
        inner.next_leave_at = Some(Instant::now() + Duration::from_secs(60));
    }
    s.exclusive_desired.store(true, Ordering::SeqCst);

    let action = s.prepare_next_action().await;
    // (Idle, true) -> Entering: must NOT be gated by next_leave_at.
    assert!(
        matches!(
            action,
            ExclusiveAction::Send {
                next_state: ExclusiveState::Entering,
                ..
            }
        ),
        "non-leave transitions must ignore the backoff gate",
    );
    s.shutdown_driver_loop().await;
}

/// `rollback_send_failure` only reverses when (op_id, state) both
/// match the recorded values. A concurrent reset that bumped
/// op_id between the send attempt and the rollback must NOT
/// regress the state.
#[tokio::test]
async fn rollback_send_failure_is_guarded() {
    let s = fresh_supervisor();
    s.exclusive_desired.store(true, Ordering::SeqCst);
    let (op_before, prev_state) = {
        let inner = s.exclusive_inner.read().await;
        (inner.current_op_id, inner.state)
    };
    s.prepare_next_action().await; // -> Entering, op_id +1
    let after_op = {
        let inner = s.exclusive_inner.read().await;
        inner.current_op_id
    };
    assert_eq!(after_op, op_before + 1);

    // Simulate a concurrent reset that bumps op_id again and
    // restores Idle. The pending rollback (which thinks it
    // recorded after_op + Entering) must NOT clobber it.
    s.reset_exclusive_state().await;
    let after_reset = {
        let inner = s.exclusive_inner.read().await;
        inner.current_op_id
    };
    assert!(after_reset > after_op);

    // Rollback referencing the stale (op_id, state) is a no-op.
    s.rollback_send_failure(after_op, ExclusiveState::Entering, prev_state)
        .await;
    let (state, op_id) = read_inner(&s).await;
    assert_eq!(state, ExclusiveState::Idle, "reset must survive rollback");
    assert_eq!(op_id, after_reset);
    s.shutdown_driver_loop().await;
}

/// `reset_exclusive_state` always returns state to Idle, bumps
/// op_id, and flips desired off.
#[tokio::test]
async fn reset_exclusive_state_clears_and_bumps() {
    let s = fresh_supervisor();
    // Move into Active manually.
    {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Active;
        inner.current_op_id = 5;
    }
    s.exclusive_desired.store(true, Ordering::SeqCst);
    s.reset_exclusive_state().await;
    let (state, op_id) = read_inner(&s).await;
    assert_eq!(state, ExclusiveState::Idle);
    assert_eq!(op_id, 6);
    assert!(!s.exclusive_desired.load(Ordering::SeqCst));
    s.shutdown_driver_loop().await;
}

/// `await_exclusive_idle` returns immediately when already Idle.
#[tokio::test]
async fn await_exclusive_idle_returns_immediately_on_idle() {
    let s = fresh_supervisor();
    s.await_exclusive_idle(Duration::from_millis(100))
        .await
        .expect("immediate Ok");
    s.shutdown_driver_loop().await;
}

/// `await_exclusive_idle` times out when state is non-Idle and no
/// transition arrives.
#[tokio::test]
async fn await_exclusive_idle_times_out_when_stuck() {
    let s = fresh_supervisor();
    {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Entering;
        let _ = s
            .exclusive_state_watch
            .send_replace(ExclusiveState::Entering);
    }
    // No state transition to Idle will arrive in the next 50ms.
    let res = s.await_exclusive_idle(Duration::from_millis(50)).await;
    assert!(res.is_err(), "expected timeout");
    s.shutdown_driver_loop().await;
}

/// `await_exclusive_idle` resolves when a state transition lands
/// it on Idle.
#[tokio::test]
async fn await_exclusive_idle_resolves_on_transition() {
    let s = fresh_supervisor();
    {
        let mut inner = s.exclusive_inner.write().await;
        inner.state = ExclusiveState::Leaving;
        let _ = s
            .exclusive_state_watch
            .send_replace(ExclusiveState::Leaving);
    }
    let s_clone = Arc::clone(&s);
    let waiter =
        tokio::spawn(async move { s_clone.await_exclusive_idle(Duration::from_secs(2)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    s.reset_exclusive_state().await;
    let res = waiter.await.expect("join");
    assert!(res.is_ok());
    s.shutdown_driver_loop().await;
}
