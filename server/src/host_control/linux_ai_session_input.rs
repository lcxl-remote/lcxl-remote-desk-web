//! Session-scoped native endpoint discovery, sourced only from worker IPC.
use super::session_shell::{RegisteredSessionShell, SessionShellRegistry};
use super::*;
use std::sync::atomic::AtomicU64;

#[derive(Default)]
pub(super) struct Routes {
    revision: u64,
    shells: HashMap<UpstreamSessionId, Route>,
}
struct Route {
    registration: Arc<RegisteredSessionShell>,
    sender: Option<mpsc::UnboundedSender<HostControlMessage>>,
    endpoint: Option<Announcement>,
}
struct Announcement {
    path: String,
    incarnation: u64,
    current: Arc<AtomicU64>,
}
impl Announcement {
    fn live(&self) -> bool {
        self.current.load(Ordering::Acquire) == self.incarnation
    }
}
impl Routes {
    fn route(&mut self, registration: Arc<RegisteredSessionShell>) -> &mut Route {
        let id = registration.websocket_session_id;
        if self.shells.get(&id).is_some_and(|r| {
            r.registration.registration_id != registration.registration_id
                || r.registration.registration_generation != registration.registration_generation
        }) {
            self.shells.remove(&id);
        }
        self.shells.entry(id).or_insert_with(|| Route {
            registration,
            sender: None,
            endpoint: None,
        })
    }
    fn send(&mut self, id: UpstreamSessionId) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("Native route revision exhausted");
        if let Some(route) = self.shells.get(&id) {
            if let Some(sender) = &route.sender {
                let path = route
                    .endpoint
                    .as_ref()
                    .filter(|e| e.live())
                    .map(|e| e.path.clone());
                let _ = sender.send(HostControlMessage::LinuxAiInputEndpoint {
                    revision: self.revision,
                    path,
                });
            }
        }
    }
}
impl HostControlHub {
    pub(crate) fn bind_linux_input_shell(
        &self,
        registry: &SessionShellRegistry,
        registration: Arc<RegisteredSessionShell>,
        sender: mpsc::UnboundedSender<HostControlMessage>,
    ) {
        if self.mode() != HubMode::Aggregator {
            return;
        }
        let mut routes = self.inner.linux_ai_session_input.lock().unwrap();
        if !registry.is_current(&registration) {
            return;
        }
        let id = registration.websocket_session_id;
        routes.route(registration).sender = Some(sender);
        routes.send(id);
    }
    pub(crate) fn remove_linux_input_shell(&self, id: UpstreamSessionId) {
        self.inner
            .linux_ai_session_input
            .lock()
            .unwrap()
            .shells
            .remove(&id);
    }
    pub(crate) fn publish_linux_session_input(
        &self,
        registry: SessionShellRegistry,
        registration: Arc<RegisteredSessionShell>,
        path: Option<String>,
        incarnation: u64,
        current: Arc<AtomicU64>,
    ) {
        if self.mode() != HubMode::Aggregator {
            return;
        }
        let id = registration.websocket_session_id;
        {
            let mut routes = self.inner.linux_ai_session_input.lock().unwrap();
            // Serialize registry removal with route removal. A late worker can
            // neither recreate a disconnected shell nor replace a new binding.
            if !registry.is_current(&registration) || current.load(Ordering::Acquire) != incarnation
            {
                return;
            }
            routes
                .shells
                .retain(|_, r| registry.is_current(&r.registration));
            routes.route(registration.clone()).endpoint = path.clone().map(|path| Announcement {
                path,
                incarnation,
                current: current.clone(),
            });
            routes.send(id);
        }
        let Some(path) = path else { return };
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let Some(inner) = weak.upgrade() else { break };
                let mut routes = inner.linux_ai_session_input.lock().unwrap();
                let Some(route) = routes.shells.get_mut(&id) else {
                    break;
                };
                if route.registration.registration_id != registration.registration_id
                    || route.registration.registration_generation
                        != registration.registration_generation
                    || !route
                        .endpoint
                        .as_ref()
                        .is_some_and(|e| e.path == path && e.incarnation == incarnation)
                {
                    break;
                }
                if current.load(Ordering::Acquire) != incarnation
                    || !registry.is_current(&registration)
                {
                    route.endpoint = None;
                    routes.send(id);
                    break;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_control::protocol::{SESSION_SHELL_PROTOCOL_VERSION, SessionShellInfo};
    fn register(registry: &SessionShellRegistry, ws: u64) -> Arc<RegisteredSessionShell> {
        let identity =
            super::super::session_shell::read_process_identity(std::process::id()).unwrap();
        use base64::Engine as _;
        registry
            .register(
                ws,
                SessionShellInfo {
                    protocol_version: SESSION_SHELL_PROTOCOL_VERSION,
                    app_version: "test".into(),
                    pid: std::process::id(),
                    process_start_ticks: identity.start_ticks,
                    reported_uid: identity.uid,
                    session_id: Some(format!("session-{ws}")),
                    seat: None,
                    session_type: Some("wayland".into()),
                    cwd_base64: base64::engine::general_purpose::STANDARD.encode(b"/tmp"),
                    umask: 0o022,
                    environment: Vec::new(),
                },
            )
            .unwrap()
    }
    fn recv(rx: &mut mpsc::UnboundedReceiver<HostControlMessage>) -> (u64, Option<String>) {
        match rx.try_recv().unwrap() {
            HostControlMessage::LinuxAiInputEndpoint { revision, path } => (revision, path),
            other => panic!("Unexpected native message: {other:?}"),
        }
    }
    #[tokio::test]
    async fn announcement_before_binding_is_replayed_only_to_its_registered_shell() {
        let registry = SessionShellRegistry::default();
        let first = register(&registry, 1);
        let second = register(&registry, 2);
        let hub = HostControlHub::new_aggregator();
        let gate = Arc::new(AtomicU64::new(7));
        hub.publish_linux_session_input(
            registry.clone(),
            first.clone(),
            Some("one".into()),
            7,
            gate,
        );
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        hub.bind_linux_input_shell(&registry, second, tx2);
        assert_eq!(recv(&mut rx2).1, None);
        hub.bind_linux_input_shell(&registry, first, tx1);
        assert_eq!(recv(&mut rx1).1.as_deref(), Some("one"));
        assert!(rx2.try_recv().is_err());
    }
    #[tokio::test]
    async fn replacement_is_not_cleared_by_an_old_watch_and_fencing_withdraws_it() {
        let registry = SessionShellRegistry::default();
        let reg = register(&registry, 1);
        let hub = HostControlHub::new_aggregator();
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.bind_linux_input_shell(&registry, reg.clone(), tx);
        recv(&mut rx);
        let gate = Arc::new(AtomicU64::new(7));
        hub.publish_linux_session_input(
            registry.clone(),
            reg.clone(),
            Some("old".into()),
            7,
            gate.clone(),
        );
        let first_revision = recv(&mut rx).0;
        gate.store(8, Ordering::Release);
        hub.publish_linux_session_input(registry, reg, Some("new".into()), 8, gate.clone());
        let (revision, path) = recv(&mut rx);
        assert!(revision > first_revision);
        assert_eq!(path.as_deref(), Some("new"));
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(rx.try_recv().is_err());
        gate.store(0, Ordering::Release);
        let message = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(message, HostControlMessage::LinuxAiInputEndpoint { revision: r, path: None } if r > revision)
        );
    }
    #[tokio::test]
    async fn disconnected_registration_cannot_be_recreated_by_late_worker_output() {
        let registry = SessionShellRegistry::default();
        let old = register(&registry, 1);
        let hub = HostControlHub::new_aggregator();
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.bind_linux_input_shell(&registry, old.clone(), tx);
        recv(&mut rx);
        registry.unregister_websocket(1);
        hub.remove_linux_input_shell(1);
        hub.publish_linux_session_input(
            registry.clone(),
            old.clone(),
            Some("late".into()),
            7,
            Arc::new(AtomicU64::new(7)),
        );
        assert!(
            hub.inner
                .linux_ai_session_input
                .lock()
                .unwrap()
                .shells
                .is_empty()
        );
        let current = register(&registry, 1);
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.bind_linux_input_shell(&registry, current, tx);
        recv(&mut rx);
        hub.publish_linux_session_input(
            registry,
            old,
            Some("late".into()),
            7,
            Arc::new(AtomicU64::new(7)),
        );
        assert!(rx.try_recv().is_err());
    }
}
