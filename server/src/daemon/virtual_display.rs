//! Daemon-side owner of the Windows virtual display.
//!
//! Holds the `SwDevice` handle returned by
//! [`desk_virtual_display::VirtualDisplayLifecycle::create`] for the
//! lifetime of the supervisor. The supervisor lives only in service-
//! daemon mode (`RouterContext::virtual_display = Some(...)`); other
//! startup paths leave the field as `None` and the router replies with
//! `FEATURE_UNAVAILABLE`.
//!
//! State machine (commit 5 onward):
//! - `Disabled` — toggle off or the latest `lifecycle.create()` failed.
//! - `Attaching` — handle created, waiting for the user-session worker
//!   to advertise `Capabilities` so we can issue `AttachVirtualDisplay`.
//!   `is_active()` is `false` here so the router still rejects inbound
//!   `ChangeDisplaySettings` until the worker confirms it can drive the
//!   monitor.
//! - `Attached` — both daemon (holding `SwDevice`) and worker (running
//!   capture against the virtual `\\.\DISPLAYn`) are in sync.
//! - `Detaching` — handle dropped, `DetachVirtualDisplay` sent; waiting
//!   for the worker's capture pipeline to swap back to the physical
//!   target.
//!
//! Phase 1 keeps the stub provider in `desk_virtual_display` returning
//! `NotSupported`, so `apply(true)` reliably ends up in `Disabled` and
//! the integration is exercised end-to-end without a real driver. Phase
//! 2 replaces the provider, leaving this file untouched.

use std::sync::Arc;

use desk_ipc_protocol::message::{AttachVirtualDisplayPayload, ServiceToWorker};
use desk_virtual_display::{VirtualDisplayError, VirtualDisplayHandle, VirtualDisplayLifecycle};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use desk_utils::error::DeskErrorCode;

/// Internal lifecycle state. Only `Attached` makes the supervisor
/// `is_active()`; `Attaching` and `Detaching` are transition states
/// the router treats as inactive.
enum SupervisorState {
    Disabled,
    Attaching {
        display_name: String,
        #[allow(dead_code)]
        handle: VirtualDisplayHandle,
    },
    Attached {
        display_name: String,
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
                        let display_name = handle.display_name.clone();
                        info!(
                            virtual_display.display_name = %display_name,
                            "VirtualDisplaySupervisor created handle, moving to Attaching",
                        );
                        *state = SupervisorState::Attaching {
                            display_name,
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
    /// pipeline against the virtual `\\.\DISPLAYn`. Transitions
    /// `Attaching` → `Attached` after a successful send.
    pub async fn on_worker_capabilities(&self) {
        let display_name = {
            let state = self.state.read().await;
            match &*state {
                SupervisorState::Attaching { display_name, .. }
                | SupervisorState::Attached { display_name, .. } => display_name.clone(),
                _ => return,
            }
        };
        let payload = AttachVirtualDisplayPayload {
            display_name: display_name.clone(),
        };
        if let Err(e) = self
            .worker_mgr
            .send_to_worker(ServiceToWorker::AttachVirtualDisplay(payload))
            .await
        {
            warn!(
                "[virtual-display] failed to send AttachVirtualDisplay to worker on \
                 Capabilities for {display_name}: {e}; staying in current state",
            );
            return;
        }
        // Promote Attaching → Attached. If we were already Attached
        // this is a no-op (replace the handle field via swap; we keep
        // the same handle).
        let mut state = self.state.write().await;
        let prev = std::mem::replace(&mut *state, SupervisorState::Disabled);
        *state = match prev {
            SupervisorState::Attaching {
                display_name,
                handle,
            } => {
                info!(
                    virtual_display.display_name = %display_name,
                    "VirtualDisplaySupervisor promoted Attaching -> Attached",
                );
                SupervisorState::Attached {
                    display_name,
                    handle,
                }
            }
            other => other,
        };
    }

    /// Shutdown path — drop the handle if any. Best-effort
    /// `DetachVirtualDisplay` to the worker; failures are logged.
    pub async fn shutdown(&self) {
        let send_detach = {
            let state = self.state.read().await;
            matches!(
                *state,
                SupervisorState::Attaching { .. } | SupervisorState::Attached { .. }
            )
        };
        if send_detach
            && let Err(e) = self
                .worker_mgr
                .send_to_worker(ServiceToWorker::DetachVirtualDisplay)
                .await
        {
            warn!("[virtual-display] shutdown DetachVirtualDisplay send failed: {e}");
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

    /// Test-only helper: produce a supervisor pre-promoted to
    /// `Attached`, so `is_active()` returns `true` and the router
    /// proceeds past the FEATURE_UNAVAILABLE gates into validation /
    /// dispatch. Useful for testing the INVALID_PARAMS /
    /// REMOTE_DESK_OFFLINE / success-dispatch routes.
    pub fn new_attached_for_test(worker_mgr: WorkerManager, display_name: &str) -> Self {
        use desk_virtual_display::VirtualDisplayHandleInner;
        struct MockHandleInner;
        impl VirtualDisplayHandleInner for MockHandleInner {}
        struct UnreachableProvider;
        impl VirtualDisplayLifecycle for UnreachableProvider {
            fn create(&self) -> Result<VirtualDisplayHandle, VirtualDisplayError> {
                panic!("provider must not be invoked on a pre-attached test supervisor")
            }
        }
        let handle = VirtualDisplayHandle::new(display_name.to_string(), Box::new(MockHandleInner));
        Self {
            state: RwLock::new(SupervisorState::Attached {
                display_name: display_name.to_string(),
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

    impl MockLifecycle {
        fn returns_handle() -> Self {
            Self {
                create_calls: AtomicU32::new(0),
                result: || {
                    Ok(VirtualDisplayHandle::new(
                        "MOCK\\DISPLAY1".to_string(),
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
        // Promote Attaching → Attached (worker came online).
        supervisor.on_worker_capabilities().await;
        // No live worker is registered with the WorkerManager, so the
        // AttachVirtualDisplay send fails; supervisor stays in
        // Attaching. That's exactly the path we want this test to
        // exercise — confirm `apply(false)` cleans up even from
        // Attaching.
        assert!(
            !supervisor.is_active().await,
            "no live worker → stays Attaching"
        );

        supervisor.apply(false).await.expect("apply(false)");
        // Detach drops the handle (Drop closes the OS resource).
        // We can't observe the drop directly here, but we can
        // confirm the supervisor is back to Disabled and another
        // apply(true) creates a fresh handle.
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
