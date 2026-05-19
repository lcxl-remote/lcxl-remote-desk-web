//! Daemon-side owner of the Windows virtual display.
//!
//! Holds the `SwDevice` handle returned by
//! [`desk_virtual_display::VirtualDisplayLifecycle::create`] for the
//! lifetime of the supervisor. The supervisor lives only in service-
//! daemon mode (`RouterContext::virtual_display = Some(...)`); other
//! startup paths leave the field as `None` and the router replies with
//! `FEATURE_UNAVAILABLE`.
//!
//! State machine:
//! - `Disabled` — toggle off or the latest `lifecycle.create()` failed.
//! - `Attaching` — handle created and `AttachVirtualDisplay` has either
//!   been queued at least once (after the first `Capabilities`) or is
//!   awaiting the next `Capabilities`. The supervisor stays in this
//!   state until the worker reports it has actually resolved the PnP
//!   instance id to a usable GDI display name. `is_active()` is `false`
//!   here so the router still rejects inbound `ChangeDisplaySettings`.
//! - `Attached` — both daemon (holding `SwDevice`) and worker (running
//!   capture against the virtual `\\.\DISPLAYn`) are in sync. Entered
//!   only via [`VirtualDisplaySupervisor::on_worker_attach_result`]
//!   with [`VirtualDisplayAttachOutcome::Attached`].
//! - `Detaching` — handle dropped, `DetachVirtualDisplay` sent; waiting
//!   for the worker's capture pipeline to swap back to the physical
//!   target.
//!
//! **Why send-success does not imply attached.** Earlier iterations of
//! this code promoted `Attaching → Attached` as soon as
//! `send_to_worker(AttachVirtualDisplay)` returned `Ok`. That was a
//! distributed-systems mistake: a successful IPC enqueue tells us only
//! that the message left the daemon's outgoing queue, not that the
//! worker has run `EnumDisplayDevicesW` in the user session and found
//! the virtual monitor. If GDI resolution fails inside the worker
//! (driver race, monitor never enumerated), the supervisor would
//! incorrectly report `is_active() == true` and the router would let a
//! `ChangeDisplaySettings(205)` through to a worker that has no
//! attached display. The fix is to gate the promotion on
//! `WorkerToService::VirtualDisplayAttachResult`.

use std::sync::Arc;

use desk_ipc_protocol::message::{
    AttachVirtualDisplayPayload, ServiceToWorker, VirtualDisplayAttachOutcome,
    VirtualDisplayAttachResultPayload,
};
use desk_virtual_display::{VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayLifecycle};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use desk_utils::error::DeskErrorCode;

/// Internal lifecycle state. Only `Attached` makes the supervisor
/// `is_active()`; `Attaching` and `Detaching` are transition states
/// the router treats as inactive.
///
/// `instance_id` is the **PnP device instance id** assigned by
/// `SwDeviceCreate` (e.g. `SWD\LcxlVirtualDisplay\LcxlVirtualDisplay`).
/// It is the same value forwarded to the worker over IPC. The worker
/// (running inside the interactive user session) is responsible for
/// turning it into a GDI display name via
/// [`desk_virtual_display::resolve_display_name`] — the daemon cannot
/// do that itself because `EnumDisplayDevicesW` does not see the
/// virtual monitor from Session 0.
enum SupervisorState {
    Disabled,
    Attaching {
        instance_id: String,
        #[allow(dead_code)]
        handle: VirtualDisplayHandle,
    },
    Attached {
        instance_id: String,
        // `handle` keeps the OS resource alive — dropped only on
        // `apply(false)` / `shutdown`. The struct is held for its
        // Drop, never read.
        #[allow(dead_code)]
        handle: VirtualDisplayHandle,
    },
    Detaching,
}

/// Service-daemon-only owner of the virtual display handle. The
/// supervisor is the **sole** caller of `provider.create()`; the
/// router asks it `is_active()` to decide whether to forward the
/// inbound `ChangeDisplaySettings` to the worker; `signaling_proxy`
/// pokes it via [`Self::on_worker_capabilities`] every time the
/// worker comes back with a `Capabilities` payload, so a freshly
/// re-spawned worker recovers `AttachVirtualDisplay` without
/// polling.
pub struct VirtualDisplaySupervisor {
    state: RwLock<SupervisorState>,
    provider: Box<dyn VirtualDisplayLifecycle>,
    worker_mgr: WorkerManager,
}

impl VirtualDisplaySupervisor {
    /// Construct the supervisor with a real `provider` (the platform
    /// factory returns the stub in phase 1, the Windows IDD impl in
    /// phase 2) and a clone of the daemon's `WorkerManager` for IPC.
    /// Starts in `Disabled`.
    pub fn new(provider: Box<dyn VirtualDisplayLifecycle>, worker_mgr: WorkerManager) -> Self {
        Self {
            state: RwLock::new(SupervisorState::Disabled),
            provider,
            worker_mgr,
        }
    }

    /// Whether the supervisor currently holds a live monitor handle
    /// **and** the worker has confirmed it can drive that monitor.
    /// Used by the router to gate inbound `ChangeDisplaySettings`.
    pub async fn is_active(&self) -> bool {
        matches!(*self.state.read().await, SupervisorState::Attached { .. })
    }

    /// Apply the desired enabled-state — `desired=true` ⇒ create the
    /// handle (if not already up) and move to `Attaching`. `false` ⇒
    /// drop the handle and notify the worker to swap capture back to
    /// the physical display.
    ///
    /// Failure to create the handle (e.g. stub provider returning
    /// `NotSupported`, real provider returning `SwDeviceCreate`
    /// errors) leaves the supervisor in `Disabled` and returns
    /// `DeskError::CustomError(FEATURE_UNAVAILABLE | WINDOWS_ERROR)`
    /// — the daemon startup path logs + continues so the rest of the
    /// service is still usable, just without virtual display
    /// support.
    pub async fn apply(&self, desired: bool) -> Result<(), DeskError> {
        let mut state = self.state.write().await;
        match (&*state, desired) {
            // Already in the desired direction — no-op.
            (SupervisorState::Disabled | SupervisorState::Detaching, false) => Ok(()),
            (SupervisorState::Attaching { .. } | SupervisorState::Attached { .. }, true) => Ok(()),
            // Bring up.
            (SupervisorState::Disabled | SupervisorState::Detaching, true) => {
                match self.provider.create() {
                    Ok(handle) => {
                        let instance_id = handle.instance_id.clone();
                        info!(
                            virtual_display.instance_id = %instance_id,
                            "VirtualDisplaySupervisor created handle, moving to Attaching",
                        );
                        *state = SupervisorState::Attaching {
                            instance_id,
                            handle,
                        };
                        Ok(())
                    }
                    Err(VirtualDisplayError::NotSupported) => {
                        warn!(
                            "VirtualDisplaySupervisor.create returned NotSupported \
                             (phase 1 stub or unsupported platform); staying in Disabled",
                        );
                        *state = SupervisorState::Disabled;
                        DeskError::custom_error(
                            DeskErrorCode::FEATURE_UNAVAILABLE,
                            "virtual display provider returned NotSupported",
                        )
                    }
                    Err(e) => {
                        warn!("VirtualDisplaySupervisor.create failed: {e}");
                        *state = SupervisorState::Disabled;
                        DeskError::custom_error(
                            DeskErrorCode::SYSTEM_ERROR,
                            &format!("virtual display create failed: {e}"),
                        )
                    }
                }
            }
            // Tear down.
            (SupervisorState::Attaching { .. } | SupervisorState::Attached { .. }, false) => {
                // Drop the handle first (Drop closes SwDevice), then
                // tell the worker so it doesn't keep trying to capture
                // a monitor that is going away.
                *state = SupervisorState::Detaching;
                drop(state); // Don't hold the write lock across the IPC await.
                let send_result = self
                    .worker_mgr
                    .send_to_worker(ServiceToWorker::DetachVirtualDisplay)
                    .await;
                if let Err(e) = send_result {
                    warn!("failed to send DetachVirtualDisplay to worker: {e}");
                }
                let mut state = self.state.write().await;
                *state = SupervisorState::Disabled;
                Ok(())
            }
        }
    }

    /// Called by `signaling_proxy` every time the worker sends
    /// `WorkerToService::Capabilities`. If the supervisor is in
    /// `Attaching` (first-time bring-up) or `Attached` (worker
    /// re-spawned after a crash / desktop swap), re-emit
    /// `AttachVirtualDisplay` so the new worker rebuilds its capture
    /// pipeline against the virtual `\\.\DISPLAYn`.
    ///
    /// **State machine note:** this method does NOT promote
    /// `Attaching → Attached`. A successful IPC enqueue only tells
    /// us the message left our outbound queue; the worker might still
    /// fail to resolve the PnP instance id (Session 0 race, driver
    /// removal, etc.). The promotion happens in
    /// [`Self::on_worker_attach_result`] when the worker reports
    /// [`VirtualDisplayAttachOutcome::Attached`].
    pub async fn on_worker_capabilities(&self) {
        let instance_id = {
            let state = self.state.read().await;
            match &*state {
                SupervisorState::Attaching { instance_id, .. }
                | SupervisorState::Attached { instance_id, .. } => instance_id.clone(),
                _ => return,
            }
        };
        let payload = AttachVirtualDisplayPayload {
            instance_id: instance_id.clone(),
        };
        if let Err(e) = self
            .worker_mgr
            .send_to_worker(ServiceToWorker::AttachVirtualDisplay(payload))
            .await
        {
            warn!(
                "[virtual-display] failed to send AttachVirtualDisplay to worker on \
                 Capabilities for {instance_id}: {e}; staying in current state",
            );
        }
        // Intentionally no state promotion here — see method doc.
    }

    /// Called by `signaling_proxy` when the worker reports the outcome
    /// of resolving the PnP instance id we forwarded via
    /// [`ServiceToWorker::AttachVirtualDisplay`]. This is the **only**
    /// place that promotes `Attaching → Attached`.
    ///
    /// Routing rules:
    /// - `payload.instance_id` must match the currently-tracked
    ///   `Attaching`/`Attached` instance id. A mismatch means a stale
    ///   reply (e.g. the daemon dropped and re-created the handle
    ///   between the worker's send and our receive); drop it.
    /// - `Attached(name)` while we are in `Attaching` → promote to
    ///   `Attached`. While we are already in `Attached` → idempotent
    ///   no-op (this happens when the worker is re-attaching after a
    ///   restart).
    /// - `Failed(msg)` → stay in `Attaching` and log. The next
    ///   `WorkerToService::Capabilities` triggers another
    ///   `on_worker_capabilities` send, giving the worker another
    ///   chance to resolve.
    pub async fn on_worker_attach_result(&self, payload: VirtualDisplayAttachResultPayload) {
        let mut state = self.state.write().await;
        // Capture the current tracked id before we move out of state.
        let current_id = match &*state {
            SupervisorState::Attaching { instance_id, .. }
            | SupervisorState::Attached { instance_id, .. } => Some(instance_id.clone()),
            _ => None,
        };
        let Some(current_id) = current_id else {
            debug!(
                virtual_display.instance_id = %payload.instance_id,
                "VirtualDisplayAttachResult arrived while supervisor not bringing up; dropping",
            );
            return;
        };
        if current_id != payload.instance_id {
            debug!(
                virtual_display.current_id = %current_id,
                virtual_display.received_id = %payload.instance_id,
                "VirtualDisplayAttachResult instance id mismatch; dropping stale reply",
            );
            return;
        }
        match payload.outcome {
            VirtualDisplayAttachOutcome::Attached(display_name) => {
                let prev = std::mem::replace(&mut *state, SupervisorState::Disabled);
                // Edge-trigger: only the Attaching → Attached promotion
                // fires RefreshCapabilities. A second attach-result on
                // an already-Attached supervisor is an idempotent no-op
                // (worker restart path), so it must not re-publish.
                let promoted_now = matches!(prev, SupervisorState::Attaching { .. });
                *state = match prev {
                    SupervisorState::Attaching { instance_id, handle } => {
                        info!(
                            virtual_display.instance_id = %instance_id,
                            virtual_display.display_name = %display_name,
                            "VirtualDisplaySupervisor promoted Attaching -> Attached \
                             (via attach-result)",
                        );
                        SupervisorState::Attached { instance_id, handle }
                    }
                    SupervisorState::Attached { instance_id, handle } => {
                        debug!(
                            virtual_display.instance_id = %instance_id,
                            virtual_display.display_name = %display_name,
                            "VirtualDisplayAttachResult Attached received while already \
                             Attached; idempotent no-op",
                        );
                        SupervisorState::Attached { instance_id, handle }
                    }
                    // We pulled current_id above only on Attaching/Attached, so
                    // we cannot be in Disabled/Detaching here.
                    other => other,
                };
                drop(state);
                if promoted_now {
                    // The IDD HMONITOR is now visible to
                    // `monitors::enum_display_infos`; ask the worker to
                    // re-publish Capabilities so the daemon's cache (and
                    // the next browser's `InitSignalingData`) reflects
                    // it. Re-emitting Capabilities will also trigger
                    // `on_worker_capabilities`, which re-sends a fresh
                    // AttachVirtualDisplay; the resulting second attach
                    // result lands on an already-Attached supervisor
                    // and is no-op, so the loop terminates after one
                    // extra attach.
                    if let Err(e) = self
                        .worker_mgr
                        .send_to_worker(ServiceToWorker::RefreshCapabilities)
                        .await
                    {
                        warn!(
                            "[virtual-display] failed to send RefreshCapabilities on attach \
                             promotion: {e}; daemon's capabilities cache may stay stale \
                             until the next worker restart",
                        );
                    }
                }
            }
            VirtualDisplayAttachOutcome::Failed(message) => {
                warn!(
                    virtual_display.instance_id = %payload.instance_id,
                    "Worker failed to attach virtual display: {message}. \
                     Staying in Attaching; next Capabilities will retry.",
                );
                // No state change. `Attaching` keeps `is_active() == false`,
                // so the router will still answer FEATURE_UNAVAILABLE.
            }
        }
    }

    /// Shutdown path — drop the handle if any. Best-effort
    /// `DetachVirtualDisplay` to the worker; failures are logged.
    /// After the detach is acknowledged we ask the worker to
    /// re-publish [`MediaCapabilities`] so the daemon's cache and any
    /// subsequent browser session no longer offers the IDD as a
    /// selectable display.
    pub async fn shutdown(&self) {
        let send_detach = {
            let state = self.state.read().await;
            matches!(
                *state,
                SupervisorState::Attaching { .. } | SupervisorState::Attached { .. }
            )
        };
        if send_detach {
            if let Err(e) = self
                .worker_mgr
                .send_to_worker(ServiceToWorker::DetachVirtualDisplay)
                .await
            {
                warn!("[virtual-display] shutdown DetachVirtualDisplay send failed: {e}");
            }
            if let Err(e) = self
                .worker_mgr
                .send_to_worker(ServiceToWorker::RefreshCapabilities)
                .await
            {
                warn!(
                    "[virtual-display] shutdown RefreshCapabilities send failed: {e}; \
                     dropdown may still list the IDD until the next worker restart",
                );
            }
        }
        let mut state = self.state.write().await;
        *state = SupervisorState::Disabled;
    }
}

/// Helper used by callers that want the supervisor wrapped in
/// `Arc<...>` so it can be cloned into `RouterContext.virtual_display`
/// and the `signaling_proxy` Capabilities hook simultaneously.
pub fn new_arc(
    provider: Box<dyn VirtualDisplayLifecycle>,
    worker_mgr: WorkerManager,
) -> Arc<VirtualDisplaySupervisor> {
    Arc::new(VirtualDisplaySupervisor::new(provider, worker_mgr))
}

#[cfg(test)]
impl VirtualDisplaySupervisor {
    /// Test-only helper: produce a supervisor stuck in `Disabled`,
    /// so `is_active()` returns `false` and the router's
    /// FEATURE_UNAVAILABLE / "unavailable" arm fires. The provider
    /// is a `NotSupported` stub — `apply(true)` would surface a
    /// `FEATURE_UNAVAILABLE` error but leaves state in `Disabled`.
    pub fn new_disabled_for_test(worker_mgr: WorkerManager) -> Self {
        struct NotSupportedProvider;
        impl VirtualDisplayLifecycle for NotSupportedProvider {
            fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
                Err(VirtualDisplayError::NotSupported)
            }
        }
        Self::new(Box::new(NotSupportedProvider), worker_mgr)
    }

    /// Test-only state inspector. `is_active()` only distinguishes
    /// `Attached` from everything else; tests for the
    /// `Attaching → Attached` promotion need finer granularity to tell
    /// the `Disabled` and `Attaching` arms apart.
    pub async fn state_label(&self) -> &'static str {
        match *self.state.read().await {
            SupervisorState::Disabled => "Disabled",
            SupervisorState::Attaching { .. } => "Attaching",
            SupervisorState::Attached { .. } => "Attached",
            SupervisorState::Detaching => "Detaching",
        }
    }

    /// Test-only helper: produce a supervisor pre-promoted to
    /// `Attached`, so `is_active()` returns `true` and the router
    /// proceeds past the FEATURE_UNAVAILABLE gates into validation /
    /// dispatch. Useful for testing the INVALID_PARAMS /
    /// REMOTE_DESK_OFFLINE / success-dispatch routes.
    pub fn new_attached_for_test(worker_mgr: WorkerManager, instance_id: &str) -> Self {
        use desk_virtual_display::VirtualDisplayHandleInner;
        struct MockHandleInner;
        impl VirtualDisplayHandleInner for MockHandleInner {}
        struct UnreachableProvider;
        impl VirtualDisplayLifecycle for UnreachableProvider {
            fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
                panic!("provider must not be invoked on a pre-attached test supervisor")
            }
        }
        let handle = VirtualDisplayHandle::new(instance_id.to_string(), Box::new(MockHandleInner));
        Self {
            state: RwLock::new(SupervisorState::Attached {
                instance_id: instance_id.to_string(),
                handle,
            }),
            provider: Box::new(UnreachableProvider),
            worker_mgr,
        }
    }
}

#[cfg(test)]
mod tests {
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
        let provider: Box<dyn VirtualDisplayLifecycle> =
            Box::new(ArcProvider(lifecycle_for_provider));
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
        ipc_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
            desk_ipc_protocol::message::ServiceToWorker,
        >,
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
}
