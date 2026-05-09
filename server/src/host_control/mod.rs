//! Host Control Hub — unified Tauri-side bridge across all server deployment modes.
//!
//! See `agent_works/web/2026-04-30_host-control-hub-unification.md` (planned) for
//! the architectural rationale. In short:
//!
//! - **Local** (portable): the embedded server publishes commands to its own ws
//!   endpoint; the embedded Tauri shell is a ws client.
//! - **Aggregator** (ServiceDaemon): the daemon process owns no business logic
//!   itself but routes between the worker forwarder client and the Tauri client.
//! - **Forwarder** (SessionWorker): the worker server connects to the daemon as
//!   a ws client and forwards all host-control commands upstream.
//!
//! Business code talks to a single `HostControlHub` API regardless of mode.

pub mod bridge;
pub mod endpoint;
pub mod protocol;
pub mod upstream;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use log::{debug, info, warn};
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use crate::model::security_approval::SecurityPermissionType;

pub use protocol::{
    ApprovalRequest, ApprovalResponse, ClientRole, HostControlMessage, ServiceOpKind,
};
pub use upstream::UpstreamForwarder;

/// Capacity of internal broadcast channels.
const CMD_BROADCAST_CAPACITY: usize = 256;
const STATE_BROADCAST_CAPACITY: usize = 64;

/// Identifier for an aggregator-side worker forwarder connection.
pub type UpstreamSessionId = u64;

/// Local subscriber state event payload (host control event from the GUI).
#[derive(Debug, Clone)]
pub enum HostControlEvent {
    /// Private-screen overlay visibility flipped.
    PrivateScreenVisibilityChanged {
        connection_id: String,
        visible: bool,
    },
}

/// Hub deployment mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubMode {
    Local,
    Aggregator,
    Forwarder,
}

/// One in-flight approval awaiting user response. The non-`response_tx` fields
/// are kept for future audit-log / timeout-tracking features.
#[allow(dead_code)]
struct PendingEntry {
    response_tx: oneshot::Sender<ApprovalResponse>,
    permission_type: SecurityPermissionType,
    created_at: Instant,
}

/// Snapshot of an approval request, kept for ws-Ready replay.
#[derive(Debug, Clone)]
struct ReplaySnapshot {
    req_id: String,
    permission_type: SecurityPermissionType,
    from_connection_id: Option<String>,
}

impl ReplaySnapshot {
    fn to_message(&self) -> HostControlMessage {
        HostControlMessage::SecurityApprovalRequest {
            req_id: self.req_id.clone(),
            permission_type: self.permission_type.clone(),
            from_connection_id: self.from_connection_id.clone(),
        }
    }
}

struct HubInner {
    mode: HubMode,
    /// Outbound: server → ws clients.
    cmd_tx: broadcast::Sender<HostControlMessage>,
    /// Inbound: ws clients → server (state events).
    state_tx: broadcast::Sender<HostControlEvent>,
    /// Approvals awaiting local resolution (only used in Local & Forwarder modes —
    /// the Aggregator never holds oneshots itself).
    pending_approvals: Mutex<HashMap<String, PendingEntry>>,
    /// Approval request snapshots, replayed when a Tauri client (re)connects.
    /// Used by Local and Aggregator. Forwarder does not replay (the worker is the
    /// authoritative source — it will re-request if it survives a daemon restart).
    pending_replay: Mutex<HashMap<String, ReplaySnapshot>>,
    /// Aggregator-only: req_id → which upstream forwarder session originated it.
    pending_routes: Mutex<HashMap<String, UpstreamSessionId>>,
    /// Aggregator-only: per-forwarder-session outbound mpsc, used for directional
    /// dispatch (e.g. SecurityApprovalSubmit → exactly the originating worker).
    /// Populated by `endpoint::run_ws_session` on `Ready { role: Forwarder }`.
    forwarder_sessions:
        Mutex<HashMap<UpstreamSessionId, mpsc::UnboundedSender<HostControlMessage>>>,
    /// Forwarder-only: connection to the daemon aggregator.
    upstream: Option<Arc<UpstreamForwarder>>,
    /// Number of Tauri ws clients currently connected (each `mark_tauri_connected`
    /// increments, each `mark_tauri_disconnected` decrements). Used by Local /
    /// Aggregator hubs to fail-fast or trigger Tauri-loss cleanup precisely.
    tauri_client_count: AtomicUsize,
}

/// The unified host control hub.
#[derive(Clone)]
pub struct HostControlHub {
    inner: Arc<HubInner>,
}

impl HostControlHub {
    /// Construct a Local hub (portable mode). Owns its broadcast channels;
    /// the ws endpoint is registered in the same server's HTTP routes.
    pub fn new_local() -> Self {
        Self::new_with_mode(HubMode::Local, None)
    }

    /// Construct an Aggregator hub (ServiceDaemon parent process). Routes between
    /// upstream forwarders (workers) and downstream Tauri clients.
    pub fn new_aggregator() -> Self {
        Self::new_with_mode(HubMode::Aggregator, None)
    }

    /// Construct a Forwarder hub (SessionWorker child process). Forwards business
    /// commands upstream to the daemon's `/ws/host_upstream` endpoint via the
    /// supplied `UpstreamForwarder` (typically backed by a ws client task).
    pub fn new_forwarder(upstream: Arc<UpstreamForwarder>) -> Self {
        Self::new_with_mode(HubMode::Forwarder, Some(upstream))
    }

    fn new_with_mode(mode: HubMode, upstream: Option<Arc<UpstreamForwarder>>) -> Self {
        let (cmd_tx, _) = broadcast::channel(CMD_BROADCAST_CAPACITY);
        let (state_tx, _) = broadcast::channel(STATE_BROADCAST_CAPACITY);
        let inner = HubInner {
            mode,
            cmd_tx,
            state_tx,
            pending_approvals: Mutex::new(HashMap::new()),
            pending_replay: Mutex::new(HashMap::new()),
            pending_routes: Mutex::new(HashMap::new()),
            forwarder_sessions: Mutex::new(HashMap::new()),
            upstream,
            tauri_client_count: AtomicUsize::new(0),
        };
        let hub = Self {
            inner: Arc::new(inner),
        };

        if hub.inner.mode == HubMode::Forwarder {
            hub.spawn_forwarder_inbound_task();
            hub.spawn_forwarder_disconnect_watcher();
        }

        hub
    }

    pub fn mode(&self) -> HubMode {
        self.inner.mode
    }

    /// Subscribe to outgoing host-control commands. Used by the ws endpoint to
    /// forward each command to the connected Tauri shell.
    pub fn subscribe_outbound(&self) -> broadcast::Receiver<HostControlMessage> {
        self.inner.cmd_tx.subscribe()
    }

    /// Subscribe to host-control state events (Tauri → server).
    pub fn subscribe_state(&self) -> broadcast::Receiver<HostControlEvent> {
        self.inner.state_tx.subscribe()
    }

    /// Mark that a Tauri client has connected. Called by the ws endpoint on Ready.
    pub fn mark_tauri_connected(&self) {
        self.inner.tauri_client_count.fetch_add(1, Ordering::AcqRel);
    }

    /// Mark a Tauri client as disconnected. Returns the post-decrement count so
    /// the caller can decide whether to trigger Tauri-loss cleanup.
    pub fn mark_tauri_disconnected(&self) -> usize {
        // Saturating decrement protects against any stray double-disconnect.
        let prev = self
            .inner
            .tauri_client_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
                if v == 0 { None } else { Some(v - 1) }
            })
            .unwrap_or(0);
        prev.saturating_sub(1)
    }

    /// Current number of connected Tauri ws clients.
    pub fn tauri_client_count(&self) -> usize {
        self.inner.tauri_client_count.load(Ordering::Acquire)
    }

    /// Best-effort indicator of whether a Tauri shell can currently consume
    /// commands sent via this hub.
    ///
    /// - **Local / Aggregator**: true when at least one ws subscriber has
    ///   completed `Ready { role: Tauri }` or is mid-handshake (the broadcast
    ///   receiver count is non-zero).
    /// - **Forwarder**: returns true iff the upstream ws is connected. The
    ///   aggregator is responsible for tracking real Tauri presence end-to-end;
    ///   from the worker's perspective an online upstream is the closest signal.
    pub fn has_tauri_ui(&self) -> bool {
        match self.inner.mode {
            HubMode::Local | HubMode::Aggregator => {
                self.inner.tauri_client_count.load(Ordering::Acquire) > 0
            }
            HubMode::Forwarder => self
                .inner
                .upstream
                .as_ref()
                .map(|u| u.is_connected())
                .unwrap_or(false),
        }
    }

    /// Send a command to all subscribed clients (or the upstream forwarder).
    /// Returns Ok(n) where n is the number of subscribers that received the message.
    /// 0 subscribers is **not** an error — it's a normal headless / standalone state.
    pub fn send_command(&self, msg: HostControlMessage) -> Result<usize, SendError> {
        match self.inner.mode {
            HubMode::Local | HubMode::Aggregator => {
                match self.inner.cmd_tx.send(msg) {
                    Ok(n) => Ok(n),
                    Err(_) => Ok(0), // No active receivers; expected in headless / pre-handshake.
                }
            }
            HubMode::Forwarder => {
                if let Some(up) = &self.inner.upstream {
                    if !up.is_connected() {
                        debug!("[Hub/Forwarder] Drop command — upstream offline: {msg:?}");
                        return Ok(0);
                    }
                    up.send(msg);
                    Ok(1)
                } else {
                    Err(SendError::ForwarderMissingUpstream)
                }
            }
        }
    }

    /// Request the user's approval. Returns `ApprovalResponse::deny()` immediately
    /// when no UI is available (Local with no subscribers / Forwarder with offline
    /// upstream). Otherwise awaits the user's response.
    pub async fn request_approval(&self, req: ApprovalRequest) -> ApprovalResponse {
        // Fail-fast when no UI can serve the request.
        match self.inner.mode {
            HubMode::Local => {
                if !self.has_tauri_ui() {
                    debug!(
                        "[Hub/Local] No Tauri subscriber; denying approval req_id={}",
                        req.req_id
                    );
                    return ApprovalResponse::deny();
                }
            }
            HubMode::Forwarder => {
                let connected = self
                    .inner
                    .upstream
                    .as_ref()
                    .map(|u| u.is_connected())
                    .unwrap_or(false);
                if !connected {
                    debug!(
                        "[Hub/Forwarder] Upstream offline; denying approval req_id={}",
                        req.req_id
                    );
                    return ApprovalResponse::deny();
                }
            }
            HubMode::Aggregator => {
                // The aggregator is a router — it does not originate requests.
                warn!(
                    "[Hub/Aggregator] request_approval invoked — denying (aggregator should not request)"
                );
                return ApprovalResponse::deny();
            }
        }

        let (tx, rx) = oneshot::channel();
        let permission_type = req.permission_type.clone();
        let snapshot = ReplaySnapshot {
            req_id: req.req_id.clone(),
            permission_type: permission_type.clone(),
            from_connection_id: req.from_connection_id.clone(),
        };

        {
            let mut pending = self.inner.pending_approvals.lock().unwrap();
            pending.insert(
                req.req_id.clone(),
                PendingEntry {
                    response_tx: tx,
                    permission_type: permission_type.clone(),
                    created_at: Instant::now(),
                },
            );
        }

        // Local hubs cache the request so that a Tauri shell that reconnects mid-flight
        // can resume the dialog. Forwarder does not cache (the worker is authoritative).
        if matches!(self.inner.mode, HubMode::Local) {
            self.inner
                .pending_replay
                .lock()
                .unwrap()
                .insert(req.req_id.clone(), snapshot);
        }

        let outbound = HostControlMessage::SecurityApprovalRequest {
            req_id: req.req_id.clone(),
            permission_type,
            from_connection_id: req.from_connection_id,
        };
        let _ = self.send_command(outbound);

        match rx.await {
            Ok(response) => {
                self.inner
                    .pending_replay
                    .lock()
                    .unwrap()
                    .remove(&req.req_id);
                response
            }
            Err(_) => {
                // Sender was dropped — typically because the hub itself called
                // deny_all_pending. Fall back to deny.
                self.inner
                    .pending_replay
                    .lock()
                    .unwrap()
                    .remove(&req.req_id);
                ApprovalResponse::deny()
            }
        }
    }

    /// Resolve an approval. The dispatch depends on hub mode:
    /// - Local / Forwarder: look up local oneshot and send the response.
    /// - Aggregator: pop the upstream route for `req_id` and send a directional
    ///   `SecurityApprovalSubmit` to that forwarder's session — never broadcast.
    ///
    /// Returns `true` if the response was successfully dispatched (oneshot
    /// resolved locally, or directional message handed to a registered forwarder
    /// session). Returns `false` if `req_id` is unknown or the routed forwarder
    /// has already disconnected.
    pub fn submit_approval(&self, req_id: &str, response: ApprovalResponse) -> bool {
        if self.inner.mode == HubMode::Aggregator {
            // Plan review #6/#7: Aggregator must route directionally via
            // pending_routes — never broadcast SecurityApprovalSubmit.
            let Some(session_id) = self.pop_upstream_for_req(req_id) else {
                debug!("[Hub/Aggregator] submit_approval: unknown req_id={req_id} (no route)");
                return false;
            };
            let msg = HostControlMessage::SecurityApprovalSubmit {
                req_id: req_id.to_string(),
                approved: response.approved,
                remember: response.remember,
            };
            let dispatched = self.route_to_forwarder(session_id, msg);
            if dispatched {
                self.notify_tauri_finished(req_id);
            }
            return dispatched;
        }

        // Local / Forwarder: resolve the locally held oneshot.
        let entry = self.inner.pending_approvals.lock().unwrap().remove(req_id);
        match entry {
            Some(PendingEntry { response_tx, .. }) => {
                let _ = response_tx.send(response);
                self.inner.pending_replay.lock().unwrap().remove(req_id);
                if self.inner.mode == HubMode::Local {
                    self.notify_tauri_finished(req_id);
                }
                true
            }
            None => false,
        }
    }

    /// Local / Aggregator helper: tell every Tauri shell that an approval has
    /// finished so it can release UI state (e.g. always-on-top) keyed on the
    /// request id. No-op on Forwarder (Tauri is upstream of the aggregator).
    fn notify_tauri_finished(&self, req_id: &str) {
        let _ = self.send_command(HostControlMessage::SecurityApprovalFinished {
            req_id: req_id.to_string(),
        });
    }

    /// Aggregator-only: register the outbound mpsc for a freshly-handshaken
    /// forwarder session. The endpoint passes the matching receiver into the ws
    /// sink loop so messages routed via `route_to_forwarder` reach exactly that
    /// connection.
    pub fn register_forwarder_session(
        &self,
        session_id: UpstreamSessionId,
        tx: mpsc::UnboundedSender<HostControlMessage>,
    ) {
        debug_assert_eq!(self.inner.mode, HubMode::Aggregator);
        self.inner
            .forwarder_sessions
            .lock()
            .unwrap()
            .insert(session_id, tx);
    }

    /// Aggregator-only: drop the outbound mpsc for a disconnecting forwarder.
    pub fn unregister_forwarder_session(&self, session_id: UpstreamSessionId) {
        self.inner
            .forwarder_sessions
            .lock()
            .unwrap()
            .remove(&session_id);
    }

    /// Aggregator-only: send a single host-control message to the forwarder
    /// identified by `session_id`. Returns `false` if the session is not (or
    /// no longer) registered, or if the mpsc receiver has been dropped.
    pub fn route_to_forwarder(
        &self,
        session_id: UpstreamSessionId,
        msg: HostControlMessage,
    ) -> bool {
        let sessions = self.inner.forwarder_sessions.lock().unwrap();
        let Some(tx) = sessions.get(&session_id) else {
            debug!("[Hub/Aggregator] route_to_forwarder: session_id={session_id} not registered");
            return false;
        };
        match tx.send(msg) {
            Ok(()) => true,
            Err(_) => {
                debug!(
                    "[Hub/Aggregator] route_to_forwarder: mpsc closed for session_id={session_id}"
                );
                false
            }
        }
    }

    /// Aggregator-only: returns the upstream session id that originated `req_id`,
    /// removing it from the routing table at the same time.
    pub fn pop_upstream_for_req(&self, req_id: &str) -> Option<UpstreamSessionId> {
        let id = self.inner.pending_routes.lock().unwrap().remove(req_id)?;
        self.inner.pending_replay.lock().unwrap().remove(req_id);
        Some(id)
    }

    /// Aggregator-only: register a new approval request originated from `upstream_id`.
    pub fn register_upstream_request(
        &self,
        req_id: String,
        upstream_id: UpstreamSessionId,
        permission_type: SecurityPermissionType,
        from_connection_id: Option<String>,
    ) {
        debug_assert_eq!(self.inner.mode, HubMode::Aggregator);
        self.inner
            .pending_routes
            .lock()
            .unwrap()
            .insert(req_id.clone(), upstream_id);
        self.inner.pending_replay.lock().unwrap().insert(
            req_id.clone(),
            ReplaySnapshot {
                req_id,
                permission_type,
                from_connection_id,
            },
        );
    }

    /// Aggregator-only: process an approval request just received from a worker
    /// forwarder. When at least one Tauri shell is connected, registers the
    /// request and broadcasts it for review. When no Tauri shell is connected,
    /// immediately routes a deny response back to the originating forwarder so
    /// the worker doesn't sit blocked waiting for a UI that will never arrive
    /// (and ultimately get killed by the heartbeat watchdog).
    ///
    /// Returns `true` if the request was queued for Tauri review, `false` if
    /// it was denied immediately.
    pub fn handle_upstream_approval_request(
        &self,
        req_id: String,
        upstream_id: UpstreamSessionId,
        permission_type: SecurityPermissionType,
        from_connection_id: Option<String>,
    ) -> bool {
        debug_assert_eq!(self.inner.mode, HubMode::Aggregator);
        if !self.has_tauri_ui() {
            warn!(
                "[Hub/Aggregator] No Tauri client connected; denying req_id={req_id} immediately"
            );
            self.route_to_forwarder(
                upstream_id,
                HostControlMessage::SecurityApprovalSubmit {
                    req_id,
                    approved: false,
                    remember: false,
                },
            );
            return false;
        }
        self.register_upstream_request(
            req_id.clone(),
            upstream_id,
            permission_type.clone(),
            from_connection_id.clone(),
        );
        let _ = self.send_command(HostControlMessage::SecurityApprovalRequest {
            req_id,
            permission_type,
            from_connection_id,
        });
        true
    }

    /// Aggregator-only: drain all pending approvals belonging to `upstream_id`,
    /// and drop the forwarder session's outbound mpsc registration. Returns the
    /// list of req_ids that were drained, so the caller can notify the Tauri
    /// shell to close those dialogs.
    pub fn drain_upstream_pending(&self, upstream_id: UpstreamSessionId) -> Vec<String> {
        let mut out = Vec::new();
        {
            let mut routes = self.inner.pending_routes.lock().unwrap();
            let mut replay = self.inner.pending_replay.lock().unwrap();
            routes.retain(|req_id, owner| {
                if *owner == upstream_id {
                    replay.remove(req_id);
                    out.push(req_id.clone());
                    false
                } else {
                    true
                }
            });
        }
        self.inner
            .forwarder_sessions
            .lock()
            .unwrap()
            .remove(&upstream_id);
        out
    }

    /// Snapshot of all approval requests that should be replayed to a freshly
    /// connected Tauri client.
    pub fn replay_messages_for_tauri(&self) -> Vec<HostControlMessage> {
        self.inner
            .pending_replay
            .lock()
            .unwrap()
            .values()
            .map(|snap| snap.to_message())
            .collect()
    }

    /// Drain all locally-held pending approvals as denied. Used when the upstream
    /// link drops in Forwarder mode, or when the hub is shutting down in Local mode.
    pub fn deny_all_pending(&self) {
        let mut pending = self.inner.pending_approvals.lock().unwrap();
        if pending.is_empty() {
            return;
        }
        warn!(
            "[Hub/{:?}] Denying {} pending approval(s) (link/UI lost)",
            self.inner.mode,
            pending.len()
        );
        for (_, entry) in pending.drain() {
            let _ = entry.response_tx.send(ApprovalResponse::deny());
        }
        self.inner.pending_replay.lock().unwrap().clear();
    }

    /// Aggregator-only count: how many pending approvals are currently routed.
    pub fn pending_replay_count(&self) -> usize {
        self.inner.pending_replay.lock().unwrap().len()
    }

    /// Aggregator-only: cancel every in-flight approval because the last Tauri
    /// shell has disconnected. For each pending request a directional
    /// `SecurityApprovalCancel` is delivered to its originating forwarder, and
    /// the routing / replay tables are cleared. Returns the list of req_ids that
    /// were cancelled.
    ///
    /// Idempotent: subsequent calls with empty pending tables are no-ops.
    pub fn cancel_all_for_tauri_loss(&self) -> Vec<String> {
        if self.inner.mode != HubMode::Aggregator {
            return Vec::new();
        }
        let routes: Vec<(String, UpstreamSessionId)> = {
            let mut r = self.inner.pending_routes.lock().unwrap();
            let snapshot = r.iter().map(|(k, v)| (k.clone(), *v)).collect::<Vec<_>>();
            r.clear();
            snapshot
        };
        self.inner.pending_replay.lock().unwrap().clear();

        let mut cancelled = Vec::with_capacity(routes.len());
        for (req_id, session_id) in routes {
            let msg = HostControlMessage::SecurityApprovalCancel {
                req_id: req_id.clone(),
            };
            self.route_to_forwarder(session_id, msg);
            cancelled.push(req_id);
        }
        if !cancelled.is_empty() {
            warn!(
                "[Hub/Aggregator] Tauri lost — cancelled {} in-flight approval(s)",
                cancelled.len()
            );
        }
        cancelled
    }

    /// Publish a state event from the GUI to all server-side subscribers.
    pub fn publish_state(&self, event: HostControlEvent) {
        let _ = self.inner.state_tx.send(event);
    }

    /// Forwarder-only: spawn a background task that consumes inbound messages
    /// from the upstream and dispatches local actions (resolve oneshots, etc.).
    ///
    /// Uses `tokio::spawn` (not `actix_web::rt::spawn`) so the task is `Send`
    /// and works under both multi-threaded tokio runtimes (used by tests) and
    /// the actix-rt LocalSet runtime (used in production by the worker).
    fn spawn_forwarder_inbound_task(&self) {
        let Some(upstream) = self.inner.upstream.clone() else {
            return;
        };
        let mut rx = upstream.subscribe_inbound();
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => Self::handle_forwarder_inbound(&inner, msg),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("[Hub/Forwarder] inbound channel lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("[Hub/Forwarder] inbound channel closed");
                        break;
                    }
                }
            }
        });
    }

    /// Forwarder-only: spawn a task that watches the upstream connection-state
    /// and denies every locally-held pending oneshot whenever the upstream link
    /// transitions from connected → disconnected. Critical for plan section
    /// "阶段 6 链路异常兜底" — without this, ws drops would leave business code
    /// blocked on `request_approval` until the next reconnect.
    fn spawn_forwarder_disconnect_watcher(&self) {
        let Some(upstream) = self.inner.upstream.clone() else {
            return;
        };
        let mut state_rx = upstream.subscribe_connection_state();
        // Snapshot `prev` synchronously so a tokio task scheduled after a
        // mark_disconnected() race still sees the pre-disconnect value.
        let mut prev = *state_rx.borrow();
        let hub = self.clone();
        tokio::spawn(async move {
            loop {
                if state_rx.changed().await.is_err() {
                    debug!("[Hub/Forwarder] connection-state watcher stopping");
                    break;
                }
                let cur = *state_rx.borrow_and_update();
                if prev && !cur {
                    info!("[Hub/Forwarder] upstream disconnected — denying pending approvals");
                    hub.deny_all_pending();
                }
                prev = cur;
            }
        });
    }

    fn handle_forwarder_inbound(inner: &HubInner, msg: HostControlMessage) {
        match msg {
            HostControlMessage::SecurityApprovalSubmit {
                req_id,
                approved,
                remember,
            } => {
                let entry = inner.pending_approvals.lock().unwrap().remove(&req_id);
                if let Some(entry) = entry {
                    let _ = entry
                        .response_tx
                        .send(ApprovalResponse { approved, remember });
                } else {
                    debug!("[Hub/Forwarder] SubmitApproval for unknown req_id={req_id}");
                }
            }
            HostControlMessage::SecurityApprovalCancel { req_id } => {
                let entry = inner.pending_approvals.lock().unwrap().remove(&req_id);
                if let Some(entry) = entry {
                    let _ = entry.response_tx.send(ApprovalResponse::deny());
                }
            }
            HostControlMessage::PrivateScreenStateChangedToWorker {
                connection_id,
                visible,
            } => {
                let _ = inner
                    .state_tx
                    .send(HostControlEvent::PrivateScreenVisibilityChanged {
                        connection_id,
                        visible,
                    });
            }
            other => {
                debug!("[Hub/Forwarder] Ignoring upstream-originated msg: {other:?}");
            }
        }
    }
}

#[derive(Debug)]
pub enum SendError {
    ForwarderMissingUpstream,
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForwarderMissingUpstream => {
                f.write_str("forwarder hub has no upstream configured")
            }
        }
    }
}

impl std::error::Error for SendError {}

/// Generate a fresh approval request id.
pub fn new_req_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn approval_req(req_id: &str) -> ApprovalRequest {
        ApprovalRequest {
            req_id: req_id.to_string(),
            permission_type: SecurityPermissionType::RemoteControl,
            from_connection_id: Some("conn-1".to_string()),
        }
    }

    // U-3: Local mode without ws subscribers denies immediately.
    #[tokio::test]
    async fn u3_local_no_subscriber_denies_immediately() {
        let hub = HostControlHub::new_local();
        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(
            Duration::from_millis(200),
            hub.request_approval(approval_req("r1")),
        )
        .await
        .expect("must not block");
        assert!(!resp.approved);
        assert!(!resp.remember);
        assert!(started.elapsed() < Duration::from_millis(100));
        // No replay entry created.
        assert_eq!(hub.pending_replay_count(), 0);
    }

    // U-3b: Forwarder mode with offline upstream denies immediately.
    #[tokio::test]
    async fn u3b_forwarder_offline_denies_immediately() {
        let upstream = UpstreamForwarder::new_for_test(false);
        let hub = HostControlHub::new_forwarder(upstream);
        let resp = tokio::time::timeout(
            Duration::from_millis(200),
            hub.request_approval(approval_req("r1")),
        )
        .await
        .expect("must not block");
        assert!(!resp.approved);
    }

    // U-3c: Local mode with at least one subscriber pends until submit.
    #[tokio::test]
    async fn u3c_local_with_subscriber_pends_until_submit() {
        let hub = HostControlHub::new_local();
        // Pretend Tauri is connected.
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r1")).await });

        // Give the task time to enter pending state.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hub.pending_replay_count(), 1);

        let solved = hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            },
        );
        assert!(solved, "submit should find the pending entry");

        let resp = tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("oneshot must resolve")
            .expect("task ok");
        assert!(resp.approved);
        assert!(!resp.remember);
        assert_eq!(hub.pending_replay_count(), 0);
    }

    // Regression: Local submit_approval must broadcast a SecurityApprovalFinished
    // so the Tauri shell can release always-on-top once the dialog closes.
    #[tokio::test]
    async fn local_submit_broadcasts_finished_to_tauri() {
        let hub = HostControlHub::new_local();
        let mut outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r1")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Drain the original Request so the next recv() observes Finished cleanly.
        match tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .expect("Request must be broadcast")
            .expect("channel ok")
        {
            HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
            other => panic!("expected SecurityApprovalRequest, got {other:?}"),
        }

        assert!(hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            }
        ));
        let resp = tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("oneshot must resolve")
            .unwrap();
        assert!(resp.approved);

        let finished = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .expect("Finished must be broadcast")
            .expect("channel ok");
        match finished {
            HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
            other => panic!("expected SecurityApprovalFinished, got {other:?}"),
        }
    }

    // Local submit_approval for an unknown req_id must NOT broadcast Finished —
    // otherwise a duplicate user click could spuriously release always-on-top
    // while another dialog is still up.
    #[tokio::test]
    async fn local_unknown_submit_does_not_broadcast_finished() {
        let hub = HostControlHub::new_local();
        let mut outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        assert!(!hub.submit_approval("ghost", ApprovalResponse::deny()));
        let bcast = tokio::time::timeout(Duration::from_millis(50), outbound_rx.recv()).await;
        assert!(
            bcast.is_err(),
            "no message expected when no pending entry matched"
        );
    }

    // Aggregator submit_approval also notifies Tauri so the shell can drop
    // always-on-top symmetrically with the Local path.
    #[tokio::test]
    async fn aggregator_submit_broadcasts_finished_to_tauri() {
        let hub = HostControlHub::new_aggregator();
        let mut outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let (tx, mut rx_fwd) = mpsc::unbounded_channel();
        hub.register_forwarder_session(1, tx);

        // Simulate the upstream worker registering an in-flight approval.
        hub.register_upstream_request(
            "r1".to_string(),
            1,
            SecurityPermissionType::RemoteControl,
            None,
        );

        assert!(hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            }
        ));

        // Forwarder gets the directional Submit.
        match tokio::time::timeout(Duration::from_millis(100), rx_fwd.recv())
            .await
            .expect("forwarder must receive submit")
            .expect("mpsc ok")
        {
            HostControlMessage::SecurityApprovalSubmit { req_id, .. } => assert_eq!(req_id, "r1"),
            other => panic!("expected SecurityApprovalSubmit, got {other:?}"),
        }

        // Tauri broadcast carries Finished.
        match tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .expect("Finished must be broadcast")
            .expect("channel ok")
        {
            HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
            other => panic!("expected SecurityApprovalFinished, got {other:?}"),
        }
    }

    // U-4: Submit with mismatched req_id is no-op; existing pending unaffected.
    #[tokio::test]
    async fn u4_submit_unknown_req_id_is_noop() {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r1")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Wrong id — should not resolve r1.
        let solved = hub.submit_approval("r-other", ApprovalResponse::deny());
        assert!(!solved);

        // Correct id resolves the original.
        let solved = hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: true,
            },
        );
        assert!(solved);

        let resp = task.await.unwrap();
        assert!(resp.approved && resp.remember);
    }

    // U-7: 100 concurrent approvals resolved in shuffled order — no deadlock, no loss.
    #[tokio::test]
    async fn u7_concurrent_approvals_no_deadlock() {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let mut tasks = Vec::new();
        for i in 0..100 {
            let id = format!("r{i}");
            let hub_clone = hub.clone();
            tasks.push(tokio::spawn(async move {
                let id_inner = id.clone();
                let resp = hub_clone.request_approval(approval_req(&id_inner)).await;
                (id, resp)
            }));
        }

        // Wait until all entered pending state.
        for _ in 0..50 {
            if hub.pending_replay_count() == 100 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(hub.pending_replay_count(), 100);

        // Submit in shuffled order.
        let mut order: Vec<usize> = (0..100).collect();
        order.swap(0, 99);
        order.swap(20, 50);
        order.swap(33, 66);
        for i in order {
            let approved = i % 2 == 0;
            hub.submit_approval(
                &format!("r{i}"),
                ApprovalResponse {
                    approved,
                    remember: false,
                },
            );
        }

        for t in tasks {
            let (id, resp) = t.await.unwrap();
            let i: usize = id[1..].parse().unwrap();
            assert_eq!(resp.approved, i.is_multiple_of(2));
        }
        assert_eq!(hub.pending_replay_count(), 0);
    }

    // U-8: state broadcast reaches multiple subscribers.
    #[tokio::test]
    async fn u8_state_broadcast_multi_subscriber() {
        let hub = HostControlHub::new_local();
        let mut rx_a = hub.subscribe_state();
        let mut rx_b = hub.subscribe_state();
        hub.publish_state(HostControlEvent::PrivateScreenVisibilityChanged {
            connection_id: "c1".to_string(),
            visible: true,
        });
        let a = tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        let b = tokio::time::timeout(Duration::from_millis(100), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        for ev in [a, b] {
            match ev {
                HostControlEvent::PrivateScreenVisibilityChanged {
                    connection_id,
                    visible,
                } => {
                    assert_eq!(connection_id, "c1");
                    assert!(visible);
                }
            }
        }
    }

    // U-9: send_command returns Ok(0) when nobody is listening.
    #[test]
    fn u9_send_command_zero_subscribers_is_ok() {
        let hub = HostControlHub::new_local();
        let n = hub
            .send_command(HostControlMessage::PrivateScreenShow {
                connection_id: "c1".to_string(),
            })
            .expect("send_command should not error on no subscribers");
        assert_eq!(n, 0);
    }

    // U-10: Forwarder send_command with online upstream forwards to upstream queue.
    #[tokio::test]
    async fn u10_forwarder_send_command_forwards_to_upstream() {
        let upstream = UpstreamForwarder::new_for_test(true);
        let mut outbound_rx = upstream.test_outbound_rx();
        let hub = HostControlHub::new_forwarder(upstream);

        let cmd = HostControlMessage::PrivateScreenShow {
            connection_id: "c1".to_string(),
        };
        let n = hub.send_command(cmd.clone()).unwrap();
        assert_eq!(n, 1);

        let received = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .unwrap()
            .expect("upstream must receive");
        assert_eq!(received, cmd);
    }

    // U-11: Forwarder send_command with offline upstream is silent (no panic, returns 0).
    #[tokio::test]
    async fn u11_forwarder_offline_send_command_silent() {
        let upstream = UpstreamForwarder::new_for_test(false);
        let hub = HostControlHub::new_forwarder(upstream);
        let n = hub
            .send_command(HostControlMessage::PrivateScreenShow {
                connection_id: "c1".to_string(),
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    // U-12: Forwarder receiving SubmitApproval from upstream resolves the local oneshot.
    #[tokio::test]
    async fn u12_forwarder_receives_submit_resolves_pending() {
        let upstream = UpstreamForwarder::new_for_test(true);
        let upstream_clone = Arc::clone(&upstream);
        let hub = HostControlHub::new_forwarder(upstream);

        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r1")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        upstream_clone.test_inject_inbound(HostControlMessage::SecurityApprovalSubmit {
            req_id: "r1".to_string(),
            approved: true,
            remember: true,
        });

        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(resp.approved && resp.remember);
    }

    // U-13: Forwarder receiving Cancel resolves with deny.
    #[tokio::test]
    async fn u13_forwarder_receives_cancel_resolves_deny() {
        let upstream = UpstreamForwarder::new_for_test(true);
        let upstream_clone = Arc::clone(&upstream);
        let hub = HostControlHub::new_forwarder(upstream);

        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r1")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        upstream_clone.test_inject_inbound(HostControlMessage::SecurityApprovalCancel {
            req_id: "r1".to_string(),
        });

        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(!resp.approved);
    }

    // U-14: Aggregator routing — upstream registers + drains correctly.
    #[test]
    fn u14_aggregator_pending_routes_lifecycle() {
        let hub = HostControlHub::new_aggregator();
        hub.register_upstream_request(
            "r1".to_string(),
            42,
            SecurityPermissionType::RemoteControl,
            None,
        );
        hub.register_upstream_request(
            "r2".to_string(),
            42,
            SecurityPermissionType::PrivateScreen,
            Some("c2".to_string()),
        );
        hub.register_upstream_request(
            "r3".to_string(),
            999,
            SecurityPermissionType::Whiteboard,
            None,
        );

        // Replay snapshot present for all 3.
        assert_eq!(hub.pending_replay_count(), 3);
        let snaps = hub.replay_messages_for_tauri();
        assert_eq!(snaps.len(), 3);

        // pop_upstream_for_req removes the entry.
        assert_eq!(hub.pop_upstream_for_req("r1"), Some(42));
        assert_eq!(hub.pop_upstream_for_req("r1"), None);
        assert_eq!(hub.pending_replay_count(), 2);

        // drain_upstream_pending strips remaining 42-owned entries.
        let drained = hub.drain_upstream_pending(42);
        assert_eq!(drained, vec!["r2".to_string()]);
        assert_eq!(hub.pending_replay_count(), 1);

        // r3 still owned by 999.
        let drained = hub.drain_upstream_pending(999);
        assert_eq!(drained, vec!["r3".to_string()]);
        assert_eq!(hub.pending_replay_count(), 0);
    }

    // U-14c: Aggregator replay snapshot reflects pending requests.
    #[test]
    fn u14c_aggregator_replay_snapshot() {
        let hub = HostControlHub::new_aggregator();
        hub.register_upstream_request("r1".to_string(), 7, SecurityPermissionType::Terminal, None);
        let msgs = hub.replay_messages_for_tauri();
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            HostControlMessage::SecurityApprovalRequest {
                req_id,
                permission_type,
                ..
            } => {
                assert_eq!(req_id, "r1");
                assert!(matches!(permission_type, SecurityPermissionType::Terminal));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // Aggregator request_approval is a no-op deny (it's a router, not a request source).
    #[tokio::test]
    async fn aggregator_request_approval_denies() {
        let hub = HostControlHub::new_aggregator();
        let resp = hub.request_approval(approval_req("r1")).await;
        assert!(!resp.approved);
    }

    // U-14b: Aggregator submit_approval routes the response directionally to
    // the originating forwarder session via its registered mpsc — never the
    // outbound broadcast (a second forwarder session must not see the message).
    #[tokio::test]
    async fn u14b_aggregator_submit_directional_only() {
        let hub = HostControlHub::new_aggregator();

        // Two forwarder sessions registered with their own mpsc receivers.
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        hub.register_forwarder_session(1, tx_a);
        hub.register_forwarder_session(2, tx_b);

        // Track outbound broadcast — submit must not appear here.
        let mut outbound_rx = hub.subscribe_outbound();

        hub.register_upstream_request(
            "r1".to_string(),
            1,
            SecurityPermissionType::RemoteControl,
            None,
        );
        hub.register_upstream_request("r2".to_string(), 2, SecurityPermissionType::Terminal, None);

        let dispatched = hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            },
        );
        assert!(dispatched, "directional submit must succeed");

        // Forwarder #1 receives the SubmitApproval.
        let got = tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
            .await
            .expect("session 1 must receive")
            .expect("mpsc closed");
        match got {
            HostControlMessage::SecurityApprovalSubmit {
                req_id,
                approved,
                remember,
            } => {
                assert_eq!(req_id, "r1");
                assert!(approved);
                assert!(!remember);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Forwarder #2 must NOT have received it.
        let other = tokio::time::timeout(Duration::from_millis(50), rx_b.recv()).await;
        assert!(other.is_err(), "session 2 must not receive r1's submit");

        // Outbound broadcast must NOT carry SubmitApproval; it carries the
        // Tauri-bound Finished notification instead so the shell can release
        // its dialog UI affordances.
        let bcast = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .expect("Finished broadcast expected")
            .expect("channel ok");
        match bcast {
            HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
            other @ HostControlMessage::SecurityApprovalSubmit { .. } => {
                panic!("Submit must not appear on broadcast: {other:?}")
            }
            other => panic!("unexpected broadcast frame: {other:?}"),
        }

        // Replay/route entries for r1 are removed.
        assert_eq!(hub.pending_replay_count(), 1);
    }

    // U-14d: Aggregator submit for an unknown req_id returns false.
    #[tokio::test]
    async fn u14d_aggregator_submit_unknown_returns_false() {
        let hub = HostControlHub::new_aggregator();
        let (tx, _rx) = mpsc::unbounded_channel();
        hub.register_forwarder_session(7, tx);

        let dispatched = hub.submit_approval("does-not-exist", ApprovalResponse::deny());
        assert!(!dispatched);
    }

    // Aggregator immediately denies an upstream approval request when no Tauri
    // shell is connected — prevents the worker from blocking until the heartbeat
    // watchdog kills it.
    #[tokio::test]
    async fn aggregator_handle_upstream_request_denies_without_tauri() {
        let hub = HostControlHub::new_aggregator();
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.register_forwarder_session(1, tx);

        // No mark_tauri_connected — UI is offline.
        let accepted = hub.handle_upstream_approval_request(
            "r1".to_string(),
            1,
            SecurityPermissionType::RemoteControl,
            None,
        );
        assert!(!accepted, "must report denied");
        assert_eq!(
            hub.pending_replay_count(),
            0,
            "denied request must not be registered for replay"
        );

        let msg = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("forwarder must receive a deny submit")
            .expect("mpsc closed");
        match msg {
            HostControlMessage::SecurityApprovalSubmit {
                req_id,
                approved,
                remember,
            } => {
                assert_eq!(req_id, "r1");
                assert!(!approved);
                assert!(!remember);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // Aggregator registers the request and broadcasts it when a Tauri shell is
    // connected. No deny is routed back to the forwarder.
    #[tokio::test]
    async fn aggregator_handle_upstream_request_broadcasts_when_tauri_present() {
        let hub = HostControlHub::new_aggregator();
        let mut outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let (tx, mut rx) = mpsc::unbounded_channel();
        hub.register_forwarder_session(1, tx);

        let accepted = hub.handle_upstream_approval_request(
            "r1".to_string(),
            1,
            SecurityPermissionType::RemoteControl,
            None,
        );
        assert!(accepted);
        assert_eq!(hub.pending_replay_count(), 1);

        let bcast = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .expect("broadcast must fire")
            .expect("channel closed");
        match bcast {
            HostControlMessage::SecurityApprovalRequest { req_id, .. } => {
                assert_eq!(req_id, "r1");
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Forwarder must NOT receive an immediate deny.
        let nothing = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(nothing.is_err(), "forwarder must not get a deny submit");
    }

    // Aggregator drain_upstream_pending also removes the forwarder session entry.
    #[tokio::test]
    async fn aggregator_drain_unregisters_session() {
        let hub = HostControlHub::new_aggregator();
        let (tx, _rx) = mpsc::unbounded_channel();
        hub.register_forwarder_session(42, tx);
        hub.register_upstream_request(
            "r1".to_string(),
            42,
            SecurityPermissionType::RemoteControl,
            None,
        );

        let drained = hub.drain_upstream_pending(42);
        assert_eq!(drained, vec!["r1".to_string()]);

        // After drain, route_to_forwarder fails for the same session_id.
        let routed = hub.route_to_forwarder(
            42,
            HostControlMessage::SecurityApprovalCancel {
                req_id: "r1".to_string(),
            },
        );
        assert!(!routed, "drained session must be unregistered");
    }

    // route_to_forwarder fails silently when the receiver was already dropped.
    #[tokio::test]
    async fn route_to_forwarder_handles_closed_receiver() {
        let hub = HostControlHub::new_aggregator();
        let (tx, rx) = mpsc::unbounded_channel();
        hub.register_forwarder_session(5, tx);
        drop(rx); // simulate ws task gone

        let routed = hub.route_to_forwarder(
            5,
            HostControlMessage::SecurityApprovalCancel {
                req_id: "r-x".to_string(),
            },
        );
        assert!(!routed);
    }

    // mark_tauri_disconnected returns the post-decrement count and saturates at
    // zero so a stray double-disconnect never wraps around.
    #[test]
    fn tauri_client_count_saturates_at_zero() {
        let hub = HostControlHub::new_local();
        assert_eq!(hub.tauri_client_count(), 0);
        hub.mark_tauri_connected();
        hub.mark_tauri_connected();
        assert_eq!(hub.tauri_client_count(), 2);
        assert_eq!(hub.mark_tauri_disconnected(), 1);
        assert_eq!(hub.mark_tauri_disconnected(), 0);
        // Saturating: an extra decrement must not underflow.
        assert_eq!(hub.mark_tauri_disconnected(), 0);
        assert_eq!(hub.tauri_client_count(), 0);
    }

    // Plan §6 兜底: Forwarder upstream lost — every in-flight approval is
    // resolved as deny without business code observing a hang.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forwarder_upstream_disconnect_denies_pending() {
        let upstream = UpstreamForwarder::new_for_test(true);
        let upstream_clone = Arc::clone(&upstream);
        let hub = HostControlHub::new_forwarder(upstream);

        let h1 = hub.clone();
        let h2 = hub.clone();
        let t1 = tokio::spawn(async move { h1.request_approval(approval_req("a")).await });
        let t2 = tokio::spawn(async move { h2.request_approval(approval_req("b")).await });
        // Wait until both requests parked in pending_approvals.
        for _ in 0..50 {
            if hub.inner.pending_approvals.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(hub.inner.pending_approvals.lock().unwrap().len(), 2);

        upstream_clone.mark_disconnected();

        let r1 = tokio::time::timeout(Duration::from_millis(2000), t1)
            .await
            .expect("must resolve")
            .unwrap();
        let r2 = tokio::time::timeout(Duration::from_millis(2000), t2)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(!r1.approved && !r2.approved);
        assert!(hub.inner.pending_approvals.lock().unwrap().is_empty());
    }

    // Plan §6 兜底: Aggregator's cancel_all_for_tauri_loss routes a
    // SecurityApprovalCancel to each owning forwarder and clears the tables.
    #[tokio::test]
    async fn aggregator_cancel_all_for_tauri_loss_routes_directionally() {
        let hub = HostControlHub::new_aggregator();
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        hub.register_forwarder_session(1, tx_a);
        hub.register_forwarder_session(2, tx_b);

        hub.register_upstream_request(
            "r1".to_string(),
            1,
            SecurityPermissionType::RemoteControl,
            None,
        );
        hub.register_upstream_request("r2".to_string(), 1, SecurityPermissionType::Terminal, None);
        hub.register_upstream_request(
            "r3".to_string(),
            2,
            SecurityPermissionType::Whiteboard,
            None,
        );

        let mut cancelled = hub.cancel_all_for_tauri_loss();
        cancelled.sort();
        assert_eq!(
            cancelled,
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()]
        );
        assert_eq!(hub.pending_replay_count(), 0);

        // Forwarder 1 receives Cancel for r1 and r2 (in some order).
        let mut got_a = Vec::new();
        for _ in 0..2 {
            let m = tokio::time::timeout(Duration::from_millis(100), rx_a.recv())
                .await
                .expect("session 1 must receive")
                .expect("mpsc closed");
            match m {
                HostControlMessage::SecurityApprovalCancel { req_id } => got_a.push(req_id),
                other => panic!("unexpected: {other:?}"),
            }
        }
        got_a.sort();
        assert_eq!(got_a, vec!["r1".to_string(), "r2".to_string()]);

        // Forwarder 2 receives Cancel for r3 only.
        let m = tokio::time::timeout(Duration::from_millis(100), rx_b.recv())
            .await
            .expect("session 2 must receive")
            .expect("mpsc closed");
        match m {
            HostControlMessage::SecurityApprovalCancel { req_id } => assert_eq!(req_id, "r3"),
            other => panic!("unexpected: {other:?}"),
        }

        // Idempotent: a second call has nothing to do.
        assert!(hub.cancel_all_for_tauri_loss().is_empty());
    }

    // cancel_all_for_tauri_loss is a no-op on Local/Forwarder hubs.
    #[test]
    fn cancel_all_for_tauri_loss_only_aggregator() {
        let hub = HostControlHub::new_local();
        assert!(hub.cancel_all_for_tauri_loss().is_empty());
    }

    // deny_all_pending resolves every outstanding oneshot with deny.
    #[tokio::test]
    async fn deny_all_pending_resolves_everything() {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let h1 = hub.clone();
        let h2 = hub.clone();
        let t1 = tokio::spawn(async move { h1.request_approval(approval_req("a")).await });
        let t2 = tokio::spawn(async move { h2.request_approval(approval_req("b")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        hub.deny_all_pending();
        let r1 = tokio::time::timeout(Duration::from_millis(200), t1)
            .await
            .unwrap()
            .unwrap();
        let r2 = tokio::time::timeout(Duration::from_millis(200), t2)
            .await
            .unwrap()
            .unwrap();
        assert!(!r1.approved && !r2.approved);
        assert_eq!(hub.pending_replay_count(), 0);
    }
}
