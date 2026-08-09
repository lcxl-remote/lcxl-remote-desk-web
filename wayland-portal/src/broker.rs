use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    AuthorizationTarget, PortalAvailability, PortalCapabilities, PortalError, PortalPhase,
    PortalSnapshot, PortalStream, RestoreToken, RestoreTokenStore,
};

#[async_trait]
pub trait LivePortalSession: Send + Sync {
    fn target(&self) -> AuthorizationTarget;
    fn stream(&self) -> &PortalStream;

    #[cfg(target_os = "linux")]
    fn duplicate_pipewire_fd(&self) -> Result<std::os::fd::OwnedFd, PortalError>;

    async fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), PortalError>;
    async fn notify_pointer_button(&self, button: u32, state: u32) -> Result<(), PortalError>;
    async fn notify_pointer_axis(&self, delta_x: f64, delta_y: f64) -> Result<(), PortalError>;
    async fn notify_keyboard_keycode(&self, keycode: i32, state: u32) -> Result<(), PortalError>;
    /// Resolves when the desktop Portal closes or loses this session.
    fn closure_token(&self) -> CancellationToken {
        CancellationToken::new()
    }

    async fn close(&self) -> Result<(), PortalError>;
}

pub struct PreparedPortalSession {
    pub session: Arc<dyn LivePortalSession>,
    pub selected_device_types: u32,
    pub restore_token: Option<String>,
}

#[async_trait]
pub trait PortalBackend: Send + Sync {
    async fn probe(&self) -> Result<PortalAvailability, PortalError>;

    async fn prepare(
        &self,
        target: AuthorizationTarget,
        restore_token: Option<String>,
        cancel: CancellationToken,
    ) -> Result<PreparedPortalSession, PortalError>;
}

#[derive(Clone)]
struct FallbackReadySession {
    snapshot: PortalSnapshot,
    session: Arc<dyn LivePortalSession>,
}

#[derive(Clone)]
struct InFlight {
    operation_id: String,
    target: AuthorizationTarget,
    generation: u64,
    cancel: CancellationToken,
    fallback: Option<FallbackReadySession>,
}

struct BrokerState {
    snapshot: PortalSnapshot,
    session: Option<Arc<dyn LivePortalSession>>,
    in_flight: Option<InFlight>,
}

pub struct WaylandPortalBroker {
    backend: Arc<dyn PortalBackend>,
    token_store: Option<RestoreTokenStore>,
    state: Mutex<BrokerState>,
    snapshot_tx: watch::Sender<PortalSnapshot>,
    ready_session: RwLock<Option<Arc<dyn LivePortalSession>>>,
}

impl WaylandPortalBroker {
    pub async fn new(
        backend: Arc<dyn PortalBackend>,
        token_store: Option<RestoreTokenStore>,
    ) -> Result<Arc<Self>, PortalError> {
        let availability = backend.probe().await?;
        let snapshot = if availability.monitor_available {
            PortalSnapshot::not_configured(availability)
        } else {
            PortalSnapshot::unsupported("ScreenCast portal does not offer monitor capture")
        };
        let (snapshot_tx, _) = watch::channel(snapshot.clone());
        Ok(Arc::new(Self {
            backend,
            token_store,
            state: Mutex::new(BrokerState {
                snapshot,
                session: None,
                in_flight: None,
            }),
            snapshot_tx,
            ready_session: RwLock::new(None),
        }))
    }

    pub fn subscribe(&self) -> watch::Receiver<PortalSnapshot> {
        self.snapshot_tx.subscribe()
    }

    pub async fn snapshot(&self) -> PortalSnapshot {
        self.state.lock().await.snapshot.clone()
    }

    pub async fn restore_if_available(self: &Arc<Self>) -> Result<Option<String>, PortalError> {
        let Some(store) = &self.token_store else {
            return Ok(None);
        };
        let Some(token) = store.consume()? else {
            return Ok(None);
        };
        let operation_id = format!("restore-{}", rand::random::<u128>());
        self.begin(operation_id.clone(), token.target, Some(token.token), true)
            .await?;
        Ok(Some(operation_id))
    }

    pub async fn authorize(
        self: &Arc<Self>,
        operation_id: String,
        target: AuthorizationTarget,
    ) -> Result<String, PortalError> {
        self.begin(operation_id, target, None, false).await
    }

    async fn begin(
        self: &Arc<Self>,
        operation_id: String,
        target: AuthorizationTarget,
        restore_token: Option<String>,
        restoring: bool,
    ) -> Result<String, PortalError> {
        if operation_id.is_empty() {
            return Err(PortalError::InvalidOperationId);
        }

        let (generation, cancel, old_session) = {
            let mut state = self.state.lock().await;
            if state.snapshot.phase == PortalPhase::Unsupported {
                return Err(PortalError::Unsupported(
                    state
                        .snapshot
                        .reason
                        .clone()
                        .unwrap_or_else(|| "Portal unavailable".into()),
                ));
            }
            if let Some(current) = &state.in_flight
                && target <= current.target
            {
                return Ok(current.operation_id.clone());
            }
            if let Some(current) = state.in_flight.take() {
                current.cancel.cancel();
            }
            let generation = state.snapshot.generation.saturating_add(1);
            let cancel = CancellationToken::new();
            let preserve_ready_session = !restoring
                && state.snapshot.phase == PortalPhase::Ready
                && state
                    .snapshot
                    .target
                    .is_some_and(|current_target| current_target < target)
                && state.session.is_some();
            let fallback = preserve_ready_session.then(|| FallbackReadySession {
                snapshot: state.snapshot.clone(),
                session: state.session.take().expect("ready session checked above"),
            });
            state.in_flight = Some(InFlight {
                operation_id: operation_id.clone(),
                target,
                generation,
                cancel: cancel.clone(),
                fallback,
            });
            state.snapshot.phase = if restoring {
                PortalPhase::Restoring
            } else {
                PortalPhase::Preparing
            };
            state.snapshot.capabilities = PortalCapabilities::default();
            state.snapshot.target = Some(target);
            state.snapshot.operation_id = Some(operation_id.clone());
            state.snapshot.generation = generation;
            state.snapshot.restore_token_persisted = false;
            state.snapshot.requires_local_action = false;
            state.snapshot.reason_code = None;
            state.snapshot.reason = None;
            let snapshot = state.snapshot.clone();
            let old_session = if preserve_ready_session {
                None
            } else {
                *self
                    .ready_session
                    .write()
                    .expect("Portal ready-session lock poisoned") = None;
                state.session.take()
            };
            self.snapshot_tx.send_replace(snapshot);
            (generation, cancel, old_session)
        };

        if let Some(session) = old_session {
            tokio::spawn(async move {
                let _ = session.close().await;
            });
        }

        let broker = Arc::clone(self);
        let returned_operation_id = operation_id.clone();
        tokio::spawn(async move {
            let result = broker.backend.prepare(target, restore_token, cancel).await;
            broker
                .complete(operation_id, generation, target, restoring, result)
                .await;
        });
        Ok(returned_operation_id)
    }

    async fn complete(
        self: &Arc<Self>,
        operation_id: String,
        generation: u64,
        target: AuthorizationTarget,
        restoring: bool,
        result: Result<PreparedPortalSession, PortalError>,
    ) {
        let mut state = self.state.lock().await;
        let is_current = state.in_flight.as_ref().is_some_and(|current| {
            current.operation_id == operation_id && current.generation == generation
        });
        if !is_current {
            if let Ok(prepared) = result {
                tokio::spawn(async move {
                    let _ = prepared.session.close().await;
                });
            }
            return;
        }
        let current = state
            .in_flight
            .take()
            .expect("current operation checked above");
        let fallback = current.fallback;

        match result {
            Ok(prepared) => {
                let input_ready = target.needs_input()
                    && prepared.selected_device_types & crate::REQUIRED_INPUT_DEVICE_TYPES
                        == crate::REQUIRED_INPUT_DEVICE_TYPES;
                if target.needs_input() && !input_ready {
                    let session = prepared.session;
                    tokio::spawn(async move {
                        let _ = session.close().await;
                    });
                    if !self.restore_fallback(&mut state, fallback, generation) {
                        state.snapshot.phase = PortalPhase::NeedsAuthorization;
                        state.snapshot.capabilities = PortalCapabilities::default();
                        state.snapshot.requires_local_action = true;
                        state.snapshot.reason_code = Some(
                            desk_utils::error::DeskErrorCode::WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED,
                        );
                        state.snapshot.reason =
                            Some("Keyboard and pointer were not both granted".into());
                    }
                } else {
                    let rotated = prepared.restore_token.as_ref().map(|token| RestoreToken {
                        target,
                        token: token.clone(),
                    });
                    let restore_token_persisted = self.token_store.as_ref().is_some_and(|store| {
                        match store.rotate(rotated.as_ref()) {
                            Ok(()) => rotated.is_some(),
                            Err(error) => {
                                log::warn!(
                                    "Could not persist Wayland Portal restore token: {error}"
                                );
                                false
                            }
                        }
                    });
                    *self
                        .ready_session
                        .write()
                        .expect("Portal ready-session lock poisoned") =
                        Some(prepared.session.clone());
                    state.snapshot.phase = PortalPhase::Ready;
                    state.snapshot.capabilities = PortalCapabilities {
                        screen_ready: true,
                        input_ready,
                    };
                    state.snapshot.restore_token_persisted = restore_token_persisted;
                    state.snapshot.requires_local_action = false;
                    state.snapshot.reason_code = None;
                    state.snapshot.reason = None;
                    state.session = Some(prepared.session.clone());
                    if let Some(fallback) = fallback {
                        tokio::spawn(async move {
                            let _ = fallback.session.close().await;
                        });
                    }
                    let closed = prepared.session.closure_token();
                    let session = prepared.session;
                    let broker = Arc::clone(self);
                    tokio::spawn(async move {
                        closed.cancelled().await;
                        broker.invalidate_closed_session(generation, &session).await;
                    });
                }
            }
            Err(PortalError::Cancelled) => {
                if !self.restore_fallback(&mut state, fallback, generation) {
                    state.snapshot.phase = PortalPhase::NeedsAuthorization;
                    state.snapshot.capabilities = PortalCapabilities::default();
                    state.snapshot.requires_local_action = true;
                    state.snapshot.reason_code = Some(
                        desk_utils::error::DeskErrorCode::WAYLAND_PORTAL_AUTHORIZATION_CANCELLED,
                    );
                    state.snapshot.reason = Some("Authorization was cancelled".into());
                }
            }
            Err(error) => {
                if !self.restore_fallback(&mut state, fallback, generation) {
                    state.snapshot.phase = if restoring {
                        PortalPhase::NeedsAuthorization
                    } else {
                        PortalPhase::Failed
                    };
                    state.snapshot.capabilities = PortalCapabilities::default();
                    state.snapshot.requires_local_action = true;
                    state.snapshot.reason_code = Some(error.user_code());
                    state.snapshot.reason = Some(error.user_reason());
                }
            }
        }
        state.snapshot.operation_id = None;
        self.snapshot_tx.send_replace(state.snapshot.clone());
    }

    pub async fn cancel(self: &Arc<Self>, operation_id: &str, generation: u64) -> bool {
        let mut state = self.state.lock().await;
        let Some(current) = state.in_flight.as_ref() else {
            return false;
        };
        if current.operation_id != operation_id || current.generation != generation {
            return false;
        }
        let current = state
            .in_flight
            .take()
            .expect("current operation checked above");
        current.cancel.cancel();
        if !self.restore_fallback(&mut state, current.fallback, generation) {
            state.snapshot.phase = PortalPhase::NeedsAuthorization;
            state.snapshot.capabilities = PortalCapabilities::default();
            state.snapshot.operation_id = None;
            state.snapshot.requires_local_action = true;
            state.snapshot.reason_code =
                Some(desk_utils::error::DeskErrorCode::WAYLAND_PORTAL_AUTHORIZATION_CANCELLED);
            state.snapshot.reason = Some("Authorization was cancelled".into());
        }
        self.snapshot_tx.send_replace(state.snapshot.clone());
        true
    }

    pub fn try_borrow_session(
        &self,
        needs_input: bool,
    ) -> Result<Arc<dyn LivePortalSession>, PortalError> {
        let session = self
            .ready_session
            .read()
            .expect("Portal ready-session lock poisoned")
            .clone()
            .ok_or(PortalError::AuthorizationRequired)?;
        if needs_input && !session.target().needs_input() {
            return Err(PortalError::AuthorizationRequired);
        }
        Ok(session)
    }

    async fn invalidate_closed_session(
        &self,
        generation: u64,
        closed_session: &Arc<dyn LivePortalSession>,
    ) {
        let mut state = self.state.lock().await;
        let is_current = state.snapshot.generation == generation
            && state
                .session
                .as_ref()
                .is_some_and(|session| Arc::ptr_eq(session, closed_session));
        if !is_current {
            return;
        }
        state.session = None;
        *self
            .ready_session
            .write()
            .expect("Portal ready-session lock poisoned") = None;
        state.snapshot.phase = PortalPhase::NeedsAuthorization;
        state.snapshot.capabilities = PortalCapabilities::default();
        state.snapshot.operation_id = None;
        state.snapshot.requires_local_action = true;
        state.snapshot.reason_code =
            Some(desk_utils::error::DeskErrorCode::WAYLAND_PORTAL_SESSION_CLOSED);
        state.snapshot.reason = Some("The desktop Portal closed the session".into());
        self.snapshot_tx.send_replace(state.snapshot.clone());
    }

    fn restore_fallback(
        self: &Arc<Self>,
        state: &mut BrokerState,
        fallback: Option<FallbackReadySession>,
        generation: u64,
    ) -> bool {
        let Some(fallback) = fallback else {
            return false;
        };
        let closed = fallback.session.closure_token();
        if closed.is_cancelled() {
            *self
                .ready_session
                .write()
                .expect("Portal ready-session lock poisoned") = None;
            return false;
        }

        state.snapshot = fallback.snapshot;
        state.snapshot.generation = generation;
        state.snapshot.operation_id = None;
        state.session = Some(fallback.session.clone());
        *self
            .ready_session
            .write()
            .expect("Portal ready-session lock poisoned") = Some(fallback.session.clone());

        let session = fallback.session;
        let broker = Arc::clone(self);
        tokio::spawn(async move {
            closed.cancelled().await;
            broker.invalidate_closed_session(generation, &session).await;
        });
        true
    }

    pub async fn invalidate(&self, reason: impl Into<String>) {
        let mut state = self.state.lock().await;
        let fallback_session = if let Some(current) = state.in_flight.take() {
            current.cancel.cancel();
            current.fallback.map(|fallback| fallback.session)
        } else {
            None
        };
        let session = state.session.take();
        *self
            .ready_session
            .write()
            .expect("Portal ready-session lock poisoned") = None;
        state.snapshot.phase = PortalPhase::NeedsAuthorization;
        state.snapshot.capabilities = PortalCapabilities::default();
        state.snapshot.operation_id = None;
        state.snapshot.requires_local_action = true;
        state.snapshot.reason_code =
            Some(desk_utils::error::DeskErrorCode::WAYLAND_PORTAL_BACKEND_FAILED);
        state.snapshot.reason = Some(reason.into());
        self.snapshot_tx.send_replace(state.snapshot.clone());
        drop(state);
        if let Some(session) = session {
            let _ = session.close().await;
        }
        if let Some(session) = fallback_session {
            let _ = session.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::{DEVICE_TYPE_KEYBOARD, DEVICE_TYPE_POINTER};

    struct FakeSession {
        target: AuthorizationTarget,
        stream: PortalStream,
        closed: CancellationToken,
    }

    #[async_trait]
    impl LivePortalSession for FakeSession {
        fn target(&self) -> AuthorizationTarget {
            self.target
        }

        fn stream(&self) -> &PortalStream {
            &self.stream
        }

        fn closure_token(&self) -> CancellationToken {
            self.closed.clone()
        }

        #[cfg(target_os = "linux")]
        fn duplicate_pipewire_fd(&self) -> Result<std::os::fd::OwnedFd, PortalError> {
            Err(PortalError::Backend(
                "fake session has no PipeWire fd".into(),
            ))
        }

        async fn notify_pointer_motion_absolute(&self, _: f64, _: f64) -> Result<(), PortalError> {
            Ok(())
        }
        async fn notify_pointer_button(&self, _: u32, _: u32) -> Result<(), PortalError> {
            Ok(())
        }
        async fn notify_pointer_axis(&self, _: f64, _: f64) -> Result<(), PortalError> {
            Ok(())
        }
        async fn notify_keyboard_keycode(&self, _: i32, _: u32) -> Result<(), PortalError> {
            Ok(())
        }
        async fn close(&self) -> Result<(), PortalError> {
            self.closed.cancel();
            Ok(())
        }
    }

    type FakeResult = Result<(u32, Option<String>), PortalError>;

    struct FakeBackend {
        results: StdMutex<VecDeque<FakeResult>>,
    }

    #[async_trait]
    impl PortalBackend for FakeBackend {
        async fn probe(&self) -> Result<PortalAvailability, PortalError> {
            Ok(PortalAvailability {
                remote_desktop_version: 2,
                available_source_types: 1,
                available_device_types: 3,
                monitor_available: true,
                keyboard_available: true,
                pointer_available: true,
                stable_app_id: true,
                persistent_restore: true,
            })
        }

        async fn prepare(
            &self,
            target: AuthorizationTarget,
            _: Option<String>,
            cancel: CancellationToken,
        ) -> Result<PreparedPortalSession, PortalError> {
            tokio::task::yield_now().await;
            if cancel.is_cancelled() {
                return Err(PortalError::Cancelled);
            }
            let (selected_device_types, restore_token) = self
                .results
                .lock()
                .expect("fake results lock")
                .pop_front()
                .unwrap_or(Ok((0, None)))?;
            Ok(PreparedPortalSession {
                session: Arc::new(FakeSession {
                    target,
                    stream: PortalStream {
                        node_id: 42,
                        id: None,
                        position: None,
                        size: None,
                        mapping_id: None,
                    },
                    closed: CancellationToken::new(),
                }),
                selected_device_types,
                restore_token,
            })
        }
    }

    async fn broker_with_results(results: Vec<FakeResult>) -> Arc<WaylandPortalBroker> {
        broker_with_store(results, None).await
    }

    async fn broker_with_store(
        results: Vec<FakeResult>,
        token_store: Option<RestoreTokenStore>,
    ) -> Arc<WaylandPortalBroker> {
        WaylandPortalBroker::new(
            Arc::new(FakeBackend {
                results: StdMutex::new(results.into()),
            }),
            token_store,
        )
        .await
        .expect("broker")
    }

    async fn wait_for_phase(broker: &WaylandPortalBroker, phase: PortalPhase) -> PortalSnapshot {
        broker
            .subscribe()
            .wait_for(|snapshot| snapshot.phase == phase)
            .await
            .expect("snapshot channel")
            .clone()
    }

    #[tokio::test]
    async fn screen_only_does_not_require_input_capability() {
        let broker = broker_with_results(vec![Ok((0, None))]).await;
        broker
            .authorize("screen".into(), AuthorizationTarget::ScreenOnly)
            .await
            .expect("authorize");
        let snapshot = wait_for_phase(&broker, PortalPhase::Ready).await;
        assert!(snapshot.admits(false));
        assert!(!snapshot.admits(true));
    }

    #[tokio::test]
    async fn screen_and_input_requires_keyboard_and_pointer() {
        let broker = broker_with_results(vec![Ok((DEVICE_TYPE_KEYBOARD, None))]).await;
        broker
            .authorize("input".into(), AuthorizationTarget::ScreenAndInput)
            .await
            .expect("authorize");
        let rejected = wait_for_phase(&broker, PortalPhase::NeedsAuthorization).await;
        assert_eq!(
            rejected.reason_code,
            Some(desk_utils::error::DeskErrorCode::WAYLAND_PORTAL_INPUT_PERMISSION_REQUIRED)
        );

        let broker =
            broker_with_results(vec![Ok((DEVICE_TYPE_KEYBOARD | DEVICE_TYPE_POINTER, None))]).await;
        broker
            .authorize("input".into(), AuthorizationTarget::ScreenAndInput)
            .await
            .expect("authorize");
        assert!(
            wait_for_phase(&broker, PortalPhase::Ready)
                .await
                .admits(true)
        );
    }

    #[tokio::test]
    async fn cancelled_input_upgrade_restores_screen_only_session() {
        let broker = broker_with_results(vec![Ok((0, None)), Err(PortalError::Cancelled)]).await;
        broker
            .authorize("screen".into(), AuthorizationTarget::ScreenOnly)
            .await
            .expect("screen authorize");
        wait_for_phase(&broker, PortalPhase::Ready).await;
        let original = broker.try_borrow_session(false).expect("screen session");

        let mut snapshots = broker.subscribe();
        broker
            .authorize("upgrade".into(), AuthorizationTarget::ScreenAndInput)
            .await
            .expect("input upgrade");
        let restored = snapshots
            .wait_for(|snapshot| snapshot.phase == PortalPhase::Ready && snapshot.generation == 2)
            .await
            .expect("restored snapshot")
            .clone();

        assert!(restored.admits(false));
        assert!(!restored.admits(true));
        let current = broker.try_borrow_session(false).expect("restored session");
        assert!(Arc::ptr_eq(&original, &current));
        assert!(!original.closure_token().is_cancelled());
    }

    #[tokio::test]
    async fn explicit_cancel_of_input_upgrade_restores_screen_only_session() {
        let broker = broker_with_results(vec![Ok((0, None)), Ok((0, None))]).await;
        broker
            .authorize("screen".into(), AuthorizationTarget::ScreenOnly)
            .await
            .expect("screen authorize");
        wait_for_phase(&broker, PortalPhase::Ready).await;
        let original = broker.try_borrow_session(false).expect("screen session");

        broker
            .authorize("upgrade".into(), AuthorizationTarget::ScreenAndInput)
            .await
            .expect("input upgrade");
        let preparing = broker.snapshot().await;
        assert_eq!(preparing.phase, PortalPhase::Preparing);
        assert!(broker.cancel("upgrade", preparing.generation).await);

        let restored = broker.snapshot().await;
        assert_eq!(restored.phase, PortalPhase::Ready);
        assert!(restored.admits(false));
        assert!(!restored.admits(true));
        let current = broker.try_borrow_session(false).expect("restored session");
        assert!(Arc::ptr_eq(&original, &current));
        assert!(!original.closure_token().is_cancelled());
    }

    #[tokio::test]
    async fn token_persistence_failure_keeps_ready_but_reports_session_only() {
        let directory = tempfile::tempdir().expect("tempdir");
        let blocked_parent = directory.path().join("not-a-directory");
        fs::write(&blocked_parent, b"block directory creation").expect("write blocker");
        let store = RestoreTokenStore::new(
            blocked_parent.join("wayland-portal.json"),
            "com.lcxl.remote-desk",
            "gnome",
        );
        let broker = broker_with_store(vec![Ok((0, Some("next-token".into())))], Some(store)).await;

        broker
            .authorize("screen".into(), AuthorizationTarget::ScreenOnly)
            .await
            .expect("authorize");
        let snapshot = wait_for_phase(&broker, PortalPhase::Ready).await;

        assert!(snapshot.admits(false));
        assert!(!snapshot.restore_token_persisted);
    }

    #[tokio::test]
    async fn persisted_replacement_token_is_reported_only_after_rotation_succeeds() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = RestoreTokenStore::new(
            directory.path().join("wayland-portal.json"),
            "com.lcxl.remote-desk",
            "gnome",
        );
        let broker = broker_with_store(
            vec![Ok((0, Some("next-token".into())))],
            Some(store.clone()),
        )
        .await;

        broker
            .authorize("screen".into(), AuthorizationTarget::ScreenOnly)
            .await
            .expect("authorize");
        let snapshot = wait_for_phase(&broker, PortalPhase::Ready).await;

        assert!(snapshot.restore_token_persisted);
        assert_eq!(
            store.consume().expect("consume persisted token"),
            Some(RestoreToken {
                target: AuthorizationTarget::ScreenOnly,
                token: "next-token".into(),
            })
        );
    }

    #[tokio::test]
    async fn duplicate_or_weaker_request_joins_single_flight() {
        let broker = broker_with_results(vec![Ok((0, None))]).await;
        let first = broker
            .authorize("first".into(), AuthorizationTarget::ScreenAndInput)
            .await
            .expect("first");
        let joined = broker
            .authorize("second".into(), AuthorizationTarget::ScreenOnly)
            .await
            .expect("joined");
        assert_eq!(joined, first);
    }

    #[tokio::test]
    async fn desktop_session_closed_invalidates_current_generation() {
        let broker = broker_with_results(vec![Ok((0, None))]).await;
        broker
            .authorize("screen".into(), AuthorizationTarget::ScreenOnly)
            .await
            .expect("authorize");

        let mut snapshots = broker.subscribe();
        snapshots
            .wait_for(|snapshot| snapshot.phase == PortalPhase::Ready)
            .await
            .expect("ready snapshot");
        let session = broker.try_borrow_session(false).expect("ready session");
        session.closure_token().cancel();
        snapshots
            .wait_for(|snapshot| snapshot.phase == PortalPhase::NeedsAuthorization)
            .await
            .expect("closed snapshot");

        let closed = broker.snapshot().await;
        assert!(!closed.admits(false));
        assert_eq!(
            closed.reason_code,
            Some(desk_utils::error::DeskErrorCode::WAYLAND_PORTAL_SESSION_CLOSED)
        );
        assert!(matches!(
            broker.try_borrow_session(false),
            Err(PortalError::AuthorizationRequired)
        ));
    }
}
