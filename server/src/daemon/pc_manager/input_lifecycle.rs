//! Per-PC ordering of input admission and held-input release.

use super::*;
use desk_ipc_protocol::message::ReleaseInputStatePayload;
use std::collections::{HashMap, HashSet};

#[derive(Debug)]
pub struct InputLifecycle {
    pub connection_epoch: String,
    pub state: tokio::sync::Mutex<InputLifecycleState>,
}

#[derive(Debug)]
pub struct InputLifecycleState {
    pub connected: bool,
    pub closed: bool,
    pub cleanup_pending: bool,
    channels: HashMap<String, usize>,
    closed_channels: HashSet<String>,
}

impl InputLifecycle {
    pub fn new(connection_epoch: String) -> Self {
        Self {
            connection_epoch,
            state: tokio::sync::Mutex::new(InputLifecycleState {
                connected: true,
                closed: false,
                cleanup_pending: false,
                channels: HashMap::new(),
                closed_channels: HashSet::new(),
            }),
        }
    }

    /// The caller holds the PC's input lock through permission changes and enqueue.
    pub async fn release_locked(
        &self,
        state: &mut InputLifecycleState,
        connection_id: &str,
        worker_mgr: Option<&WorkerManager>,
    ) {
        state.cleanup_pending = true;
        let Some(worker_mgr) = worker_mgr else {
            // SDP-only tests do not install a worker or inject input.
            state.cleanup_pending = false;
            return;
        };
        match worker_mgr
            .send_to_interactive_connection_worker(
                connection_id,
                ServiceToWorker::ReleaseInputState(ReleaseInputStatePayload {
                    connection_id: connection_id.to_owned(),
                    connection_epoch: self.connection_epoch.clone(),
                }),
            )
            .await
        {
            Ok(()) => state.cleanup_pending = false,
            Err(error) => log::warn!(
                "[InputLifecycle] {connection_id}: held-input release could not reach worker: {error}"
            ),
        }
    }
}

impl InputLifecycleState {
    pub fn can_forward(&self) -> bool {
        self.connected && !self.closed && !self.cleanup_pending && self.closed_channels.is_empty()
    }

    pub fn register_channel(&mut self, label: &str, token: usize) {
        self.channels.insert(label.to_owned(), token);
        self.closed_channels.remove(label);
    }

    pub fn is_current_channel(&self, label: &str, token: usize) -> bool {
        !self.closed && self.channels.get(label) == Some(&token)
    }

    pub fn set_channel_open(&mut self, label: &str, token: usize, open: bool) -> bool {
        if !self.is_current_channel(label, token) {
            return false;
        }
        if open {
            self.closed_channels.remove(label);
            false
        } else {
            self.closed_channels.insert(label.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_channel_close_does_not_suspend_replacement() {
        let lifecycle = InputLifecycle::new("epoch".into());
        let mut state = lifecycle.state.lock().await;
        state.register_channel("mouse_event", 1);
        state.register_channel("mouse_event", 2);
        assert!(!state.set_channel_open("mouse_event", 1, false));
        assert!(state.can_forward());
        assert!(state.set_channel_open("mouse_event", 2, false));
        assert!(!state.can_forward());
        assert!(!state.set_channel_open("mouse_event", 2, false));
        state.set_channel_open("mouse_event", 2, true);
        assert!(state.can_forward());
        state.closed = true;
        assert!(!state.is_current_channel("mouse_event", 2));
    }

    #[tokio::test]
    async fn input_release_is_ordered_after_inflight_enqueue() {
        let lifecycle = Arc::new(InputLifecycle::new("epoch".into()));
        let events = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let guard = lifecycle.state.lock().await;
        let release_lifecycle = lifecycle.clone();
        let release_events = events.clone();
        let release = tokio::spawn(async move {
            let mut state = release_lifecycle.state.lock().await;
            state.connected = false;
            release_events.lock().await.push("release");
        });
        events.lock().await.push("down");
        drop(guard);
        release.await.unwrap();
        assert_eq!(*events.lock().await, vec!["down", "release"]);
        assert!(!lifecycle.state.lock().await.can_forward());
    }
    #[tokio::test]
    async fn failed_release_enqueue_keeps_input_closed() {
        let registry = PcRegistry::new();
        let settings = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
            Settings::default(),
        ));
        let (worker, _) = WorkerManager::new(settings, registry);
        let lifecycle = InputLifecycle::new("epoch".into());
        let mut state = lifecycle.state.lock().await;
        lifecycle
            .release_locked(&mut state, "controller", Some(&worker))
            .await;
        assert!(state.cleanup_pending);
        state.connected = true;
        assert!(!state.can_forward());
    }

    #[tokio::test]
    async fn ipc_down_precedes_release_and_reconnect_does_not_replay_it() {
        let settings = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
            Settings::default(),
        ));
        let (worker, _) = WorkerManager::new(settings, PcRegistry::new());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        worker.install_active_for_test(tx).await;
        let lifecycle = Arc::new(InputLifecycle::new("epoch".into()));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let down_gate = lifecycle.clone();
        let down_worker = worker.clone();
        let down = tokio::spawn(async move {
            let state = down_gate.state.lock().await;
            assert!(state.can_forward());
            entered_tx.send(()).unwrap();
            resume_rx.await.unwrap();
            down_worker
                .send_to_interactive_connection_worker(
                    "controller",
                    ServiceToWorker::MouseInput(desk_ipc_protocol::message::InputPayload {
                        connection_id: "controller".into(),
                        data: b"down".to_vec(),
                    }),
                )
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        let release_gate = lifecycle.clone();
        let release_worker = worker.clone();
        let release = tokio::spawn(async move {
            let mut state = release_gate.state.lock().await;
            state.connected = false;
            release_gate
                .release_locked(&mut state, "controller", Some(&release_worker))
                .await;
        });
        resume_tx.send(()).unwrap();
        down.await.unwrap();
        release.await.unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            ServiceToWorker::MouseInput(_)
        ));
        assert!(
            matches!(rx.try_recv().unwrap(), ServiceToWorker::ReleaseInputState(payload)
            if payload.connection_epoch == "epoch")
        );
        let mut state = lifecycle.state.lock().await;
        assert!(!state.can_forward());
        state.connected = true;
        assert!(state.can_forward());
        assert!(rx.try_recv().is_err());
    }
}
