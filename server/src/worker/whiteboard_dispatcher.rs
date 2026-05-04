//! # Worker-side whiteboard dispatcher (Arch IV PR 4 cut 3)
//!
//! Mirrors `service::whiteboard_event::handle_whiteboard_event` but
//! receives draw messages over the IPC event pipe instead of from a
//! `webrtc::data_channel::on_message` callback. Whiteboard traffic is
//! one-way browser→host: each browser DC text frame becomes a
//! `ServiceToWorker::WhiteboardCommand`, and the dispatcher forwards
//! the JSON to the Tauri whiteboard manager via the
//! `host_control_hub` bridge.
//!
//! ## Show / Hide lifecycle
//!
//! Arch III sends a `WhiteboardCommand::Show(connection_id)` on every
//! incoming draw message (the Tauri overlay is idempotent w.r.t.
//! Show), and `WhiteboardCommand::Hide(...)` on DC close. PR 4 cut 3
//! preserves the Show-on-every-message behaviour so the overlay
//! semantics stay identical, and emits Hide on `stop_connection` —
//! that's the closest worker-side analogue of the browser closing the
//! DC. (In Arch IV the daemon owns the DC's `on_close` callback; we
//! could surface a dedicated IPC for it later, but `StopMedia`
//! already fires when the browser disconnects.)
//!
//! ## Permission gating
//!
//! The daemon's DC router gates browser→worker forwarding on
//! `accept_control` (`pc_manager::route_is_permitted`). Arch III
//! additionally cached `check_security_permission(allow_whiteboard)`
//! per-DC; that finer-grained gate is not yet plumbed across the
//! daemon/worker boundary. Like file_transfer (cut 2) the worker
//! trusts the daemon's gate and does not re-check.

use std::collections::HashSet;
use std::sync::Arc;

use desk_input_injection::model::host_control::WhiteboardCommand;
use desk_ipc_protocol::message::{OpaqueConnectionPayload, StartMediaPayload, StopMediaPayload};
use log::{debug, error, info, warn};
use tokio::sync::Mutex as TokioMutex;

use crate::host_control::HostControlHub;
use crate::host_control::bridge::bridge_whiteboard_to_hub;

struct DispatcherInner {
    sender: std::sync::mpsc::Sender<WhiteboardCommand>,
    active_connections: HashSet<String>,
}

/// Worker-side whiteboard dispatcher. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct WhiteboardDispatcher {
    inner: Arc<TokioMutex<DispatcherInner>>,
}

impl WhiteboardDispatcher {
    /// Construct a dispatcher whose draw / show / hide commands fan
    /// into the supplied `host_control_hub`. Spawns the bridge thread
    /// once (same shape as Arch III's `DeskSession::new`).
    pub fn new(host_control_hub: Arc<HostControlHub>) -> Self {
        let sender = bridge_whiteboard_to_hub(host_control_hub);
        Self {
            inner: Arc::new(TokioMutex::new(DispatcherInner {
                sender,
                active_connections: HashSet::new(),
            })),
        }
    }

    /// For tests: construct with a pre-built sender so a mock can
    /// observe the emitted commands without spinning up the hub.
    #[cfg(test)]
    fn from_sender(sender: std::sync::mpsc::Sender<WhiteboardCommand>) -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(DispatcherInner {
                sender,
                active_connections: HashSet::new(),
            })),
        }
    }

    /// Add a connection to the active set so subsequent IPC
    /// commands for it are processed.
    pub async fn start_connection(&self, payload: &StartMediaPayload) {
        let mut inner = self.inner.lock().await;
        if inner
            .active_connections
            .insert(payload.connection_id.clone())
        {
            info!(
                "[WhiteboardDispatcher] {}: subscribed (active_count={})",
                payload.connection_id,
                inner.active_connections.len()
            );
        }
    }

    /// Remove a connection. Emits a `WhiteboardCommand::Hide` so the
    /// Tauri overlay closes if this was the connection that drew
    /// onto it (the overlay manager dedupes / reference-counts shows
    /// internally — we just send Hide and let it decide).
    pub async fn stop_connection(&self, payload: &StopMediaPayload) {
        let mut inner = self.inner.lock().await;
        let removed = inner.active_connections.remove(&payload.connection_id);
        if removed {
            if let Err(e) = inner
                .sender
                .send(WhiteboardCommand::Hide(payload.connection_id.clone()))
            {
                warn!(
                    "[WhiteboardDispatcher] {}: hide command send failed (bridge thread gone): {e}",
                    payload.connection_id
                );
            }
            info!(
                "[WhiteboardDispatcher] {}: unsubscribed (active_count={})",
                payload.connection_id,
                inner.active_connections.len()
            );
        }
    }

    /// Drop every connection. Fires Hide for each one so the overlay
    /// manager can release any per-connection state.
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        for connection_id in inner.active_connections.iter() {
            let _ = inner
                .sender
                .send(WhiteboardCommand::Hide(connection_id.clone()));
        }
        inner.active_connections.clear();
    }

    /// Apply an incoming whiteboard command. The bytes are an opaque
    /// JSON draw payload that the Tauri whiteboard manager parses;
    /// the dispatcher only validates UTF-8 and forwards through.
    pub async fn handle_command(&self, payload: OpaqueConnectionPayload) {
        let inner = self.inner.lock().await;
        if !inner.active_connections.contains(&payload.connection_id) {
            debug!(
                "[WhiteboardDispatcher] {}: command for inactive connection — dropping",
                payload.connection_id
            );
            return;
        }
        let s = match std::str::from_utf8(&payload.data) {
            Ok(s) => s.to_string(),
            Err(e) => {
                error!(
                    "[WhiteboardDispatcher] {}: payload not UTF-8: {e}",
                    payload.connection_id
                );
                return;
            }
        };
        // Mirror Arch III: send Show on every message so the overlay
        // is guaranteed to be visible even if the user dismissed it
        // mid-session. The Tauri manager dedupes.
        if let Err(e) = inner
            .sender
            .send(WhiteboardCommand::Show(payload.connection_id.clone()))
        {
            error!(
                "[WhiteboardDispatcher] {}: show command send failed: {e}",
                payload.connection_id
            );
            return;
        }
        if let Err(e) = inner.sender.send(WhiteboardCommand::DrawMessage(s)) {
            error!(
                "[WhiteboardDispatcher] {}: draw command send failed: {e}",
                payload.connection_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::MediaCodec;
    use std::sync::mpsc as std_mpsc;
    use std::time::Duration;

    fn dispatcher() -> (WhiteboardDispatcher, std_mpsc::Receiver<WhiteboardCommand>) {
        let (tx, rx) = std_mpsc::channel::<WhiteboardCommand>();
        (WhiteboardDispatcher::from_sender(tx), rx)
    }

    fn start_payload(connection_id: &str) -> StartMediaPayload {
        StartMediaPayload {
            connection_id: connection_id.to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 30,
            bitrate_kbps: 0,
            quality: 0,
        }
    }

    fn drain_with_timeout(
        rx: &std_mpsc::Receiver<WhiteboardCommand>,
        timeout: Duration,
    ) -> Vec<WhiteboardCommand> {
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
            match rx.recv_timeout(remaining) {
                Ok(cmd) => out.push(cmd),
                Err(_) => break,
            }
        }
        out
    }

    /// Active connection: a single text command produces Show + DrawMessage.
    #[tokio::test]
    async fn handle_command_emits_show_then_draw() {
        let (d, rx) = dispatcher();
        d.start_connection(&start_payload("c1")).await;
        d.handle_command(OpaqueConnectionPayload {
            connection_id: "c1".into(),
            data: br#"{"action":"begin","x":0,"y":0}"#.to_vec(),
        })
        .await;
        let cmds = drain_with_timeout(&rx, Duration::from_millis(100));
        assert_eq!(cmds.len(), 2, "expected Show + DrawMessage, got {cmds:?}");
        match &cmds[0] {
            WhiteboardCommand::Show(cid) => assert_eq!(cid, "c1"),
            other => panic!("expected Show, got {other:?}"),
        }
        match &cmds[1] {
            WhiteboardCommand::DrawMessage(s) => {
                assert_eq!(s, r#"{"action":"begin","x":0,"y":0}"#);
            }
            other => panic!("expected DrawMessage, got {other:?}"),
        }
    }

    /// Inactive connection: command is dropped without emitting any
    /// bridge command. Critical liveness contract — a stale message
    /// after StopMedia must not reopen the overlay.
    #[tokio::test]
    async fn handle_command_drops_inactive_connection() {
        let (d, rx) = dispatcher();
        d.handle_command(OpaqueConnectionPayload {
            connection_id: "ghost".into(),
            data: br#"{"action":"draw"}"#.to_vec(),
        })
        .await;
        let cmds = drain_with_timeout(&rx, Duration::from_millis(50));
        assert!(cmds.is_empty(), "expected no commands, got {cmds:?}");
    }

    /// Non-UTF-8 payload is silent-dropped (no panic, no command).
    /// Defends against a buggy daemon side that forwards binary frames
    /// as whiteboard commands.
    #[tokio::test]
    async fn handle_command_drops_non_utf8() {
        let (d, rx) = dispatcher();
        d.start_connection(&start_payload("c1")).await;
        d.handle_command(OpaqueConnectionPayload {
            connection_id: "c1".into(),
            data: vec![0xFFu8, 0xFE, 0xFD],
        })
        .await;
        let cmds = drain_with_timeout(&rx, Duration::from_millis(50));
        assert!(
            cmds.is_empty(),
            "expected no commands on bad UTF-8, got {cmds:?}"
        );
    }

    /// `stop_connection` on a known connection emits Hide and removes
    /// it from the active set. A subsequent command is dropped.
    #[tokio::test]
    async fn stop_connection_emits_hide_and_disables_subsequent_commands() {
        let (d, rx) = dispatcher();
        d.start_connection(&start_payload("c1")).await;
        d.stop_connection(&StopMediaPayload {
            connection_id: "c1".into(),
        })
        .await;
        let cmds = drain_with_timeout(&rx, Duration::from_millis(100));
        assert_eq!(cmds.len(), 1, "expected single Hide, got {cmds:?}");
        assert!(matches!(&cmds[0], WhiteboardCommand::Hide(c) if c == "c1"));
        // Subsequent command is dropped silently.
        d.handle_command(OpaqueConnectionPayload {
            connection_id: "c1".into(),
            data: br#"{}"#.to_vec(),
        })
        .await;
        let cmds = drain_with_timeout(&rx, Duration::from_millis(50));
        assert!(cmds.is_empty(), "expected nothing after stop, got {cmds:?}");
    }

    /// `stop_connection` on an unknown id is a silent no-op (no Hide
    /// emission either — we only Hide what we Showed).
    #[tokio::test]
    async fn stop_connection_unknown_is_silent() {
        let (d, rx) = dispatcher();
        d.stop_connection(&StopMediaPayload {
            connection_id: "ghost".into(),
        })
        .await;
        assert!(drain_with_timeout(&rx, Duration::from_millis(50)).is_empty());
    }

    /// `shutdown` emits Hide for every still-active connection and
    /// clears the active set.
    #[tokio::test]
    async fn shutdown_emits_hide_for_each_active_connection() {
        let (d, rx) = dispatcher();
        d.start_connection(&start_payload("c1")).await;
        d.start_connection(&start_payload("c2")).await;
        d.shutdown().await;
        let cmds = drain_with_timeout(&rx, Duration::from_millis(100));
        let hides: Vec<&str> = cmds
            .iter()
            .filter_map(|c| match c {
                WhiteboardCommand::Hide(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hides.len(), 2, "expected 2 Hides, got {cmds:?}");
        assert!(hides.contains(&"c1"));
        assert!(hides.contains(&"c2"));
        // Active set drained — subsequent command drops.
        d.handle_command(OpaqueConnectionPayload {
            connection_id: "c1".into(),
            data: br#"{}"#.to_vec(),
        })
        .await;
        let cmds = drain_with_timeout(&rx, Duration::from_millis(50));
        assert!(cmds.is_empty());
    }
}
