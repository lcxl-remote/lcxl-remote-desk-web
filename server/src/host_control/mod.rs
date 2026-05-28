//! Host Control Hub — unified Tauri-side bridge across all server deployment modes.
//!
//! The Aggregator contract described below was updated by Arch IV. In short:
//!
//! - **Local** (portable): the embedded server publishes commands to its own ws
//!   endpoint; the embedded Tauri shell is a ws client.
//! - **Aggregator** (ServiceDaemon): under Arch IV the daemon plays *two* roles
//!   simultaneously — it is both a router for worker→Tauri approval traffic
//!   *and* the originator of daemon-self approvals (the WebRTC PeerConnection
//!   was moved into the daemon process by Arch IV PR 2, so `RequireControl`
//!   driven approvals are now raised by `daemon::pc_manager` directly through
//!   `request_approval`). Pre-Arch IV docs that described the aggregator as
//!   "owns no business logic, only routes" are obsolete.
//! - **Forwarder** (SessionWorker): the worker server connects to the daemon as
//!   a ws client and forwards business approvals (FileBrowse / FileTransfer /
//!   Terminal / Whiteboard / FileTransfer dispatcher cache) upstream.
//!
//! Business code talks to a single `HostControlHub` API regardless of mode.
//!
//! ## Aggregator approval-source bookkeeping
//!
//! Two approval sources share the broadcast-to-Tauri path but never mix at
//! submit time. Disambiguation is by which internal table holds state:
//!
//! | Table | Owner | Populated by | Consumed by |
//! |---|---|---|---|
//! | `pending_routes` | worker-originated only | `register_upstream_request` (called from the `/ws/host_upstream` endpoint on `SecurityApprovalRequest` from a Forwarder) | `pop_upstream_for_req` in `submit_approval`, `drain_upstream_pending` on forwarder disconnect, `cancel_all_for_tauri_loss` on last-Tauri-disconnect |
//! | `pending_approvals` | daemon-self only | `request_approval` (called from `daemon::pc_manager` via `check_security_permission`) | `submit_approval` local-oneshot fallback, `deny_all_pending` from `cancel_all_for_tauri_loss` |
//! | `pending_replay` | shared by both sources | both `request_approval` and `register_upstream_request` insert; both submit/drain paths remove | `replay_messages_for_tauri` on Tauri (re)connect |
//!
//! `submit_approval` looks up `pending_routes` first; a hit means the request
//! came from a worker and the response is dispatched directionally to that
//! Forwarder via `route_to_forwarder` (never broadcast). A miss falls through
//! to the local oneshot in `pending_approvals` (daemon-self origin). A double
//! miss is logged at `debug` and ignored.

pub mod bridge;
pub mod endpoint;
pub mod protocol;
pub mod upstream;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How long `request_approval` waits for at least one approval UI to acknowledge
/// it is mounted and able to talk to the backend before denying. This is a pure
/// readiness probe (loopback-local), not a user-decision timeout: if any UI acks
/// in time the request proceeds to an unbounded wait for the user's decision,
/// preserving the "wait forever while a working dialog exists" semantics. Only
/// applies to Local / Aggregator (daemon-self) requests; Forwarder is exempt.
const APPROVAL_UI_READY_PROBE: Duration = Duration::from_secs(10);

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
    /// Local / Aggregator (daemon-self) only: req_id → oneshot fired when an
    /// approval UI acks that it is mounted and able to reach the backend. The
    /// readiness-probe phase of `request_approval` awaits this. Lifecycle is
    /// owned exclusively by `request_approval_inner` (every exit arm cleans its
    /// own entry); `submit_approval` must never touch it, otherwise dropping the
    /// sender mid-probe races the user-result arm of the `select!`.
    pending_acks: Mutex<HashMap<String, oneshot::Sender<()>>>,
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
            pending_acks: Mutex::new(HashMap::new()),
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
    /// when no UI is available (Local / Aggregator with no Tauri shell connected,
    /// or Forwarder with offline upstream). Otherwise awaits the user's response.
    ///
    /// Aggregator note: under Arch IV the daemon process owns the WebRTC PC and
    /// therefore originates `RequireControl`-driven approvals itself (in addition
    /// to relaying worker-originated requests via `handle_upstream_approval_request`).
    /// The two sources share the same broadcast → Tauri path and are disambiguated
    /// at submit time: routes registered via `register_upstream_request` win the
    /// directional dispatch, otherwise the local oneshot is resolved.
    pub async fn request_approval(&self, req: ApprovalRequest) -> ApprovalResponse {
        self.request_approval_inner(req, APPROVAL_UI_READY_PROBE)
            .await
    }

    /// Core of [`request_approval`] with an injectable readiness-probe duration so
    /// tests do not have to wait the real [`APPROVAL_UI_READY_PROBE`].
    ///
    /// Local / Aggregator (daemon-self) is two-phase:
    ///   * Phase 1 (readiness probe): wait for any approval UI to ack, while also
    ///     racing a possible direct submit and the probe timeout.
    ///   * Phase 2 (user decision): once ready, await the user's response with no
    ///     timeout, preserving the "wait forever while a working dialog exists"
    ///     semantics.
    ///
    /// Forwarder is exempt from the probe: the worker is authoritative and the
    /// daemon drives the dialog, so it awaits the upstream-delivered response
    /// directly (registering a local ack would wait for an ack that never comes).
    async fn request_approval_inner(
        &self,
        req: ApprovalRequest,
        probe: Duration,
    ) -> ApprovalResponse {
        // Phase 0: fail-fast when no UI can serve the request.
        match self.inner.mode {
            HubMode::Local | HubMode::Aggregator => {
                if !self.has_tauri_ui() {
                    debug!(
                        "[Hub/{:?}] No Tauri subscriber; denying approval req_id={}",
                        self.inner.mode, req.req_id
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
        }

        let (tx, rx) = oneshot::channel();
        let mut rx = rx;
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

        // Local & Aggregator (daemon-self) hubs cache the request so a Tauri shell
        // reconnecting mid-flight can resume the dialog. Forwarder does not cache
        // (the worker is authoritative). Worker-originated Aggregator requests use
        // the separate `register_upstream_request` path which also populates replay.
        if matches!(self.inner.mode, HubMode::Local | HubMode::Aggregator) {
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

        // Forwarder: no local readiness probe (see method docs).
        if self.inner.mode == HubMode::Forwarder {
            return match rx.await {
                Ok(response) => response,
                Err(_) => ApprovalResponse::deny(),
            };
        }

        // Local / Aggregator — Phase 1: readiness probe.
        let (ack_tx, mut ack_rx) = oneshot::channel();
        self.inner
            .pending_acks
            .lock()
            .unwrap()
            .insert(req.req_id.clone(), ack_tx);

        tokio::select! {
            ack = &mut ack_rx => match ack {
                // At least one UI acked — proceed to phase 2.
                Ok(()) => {
                    self.inner.pending_acks.lock().unwrap().remove(&req.req_id);
                }
                // Ack sender dropped (deny_all_pending / hub teardown) — deny.
                Err(_) => {
                    self.cleanup_local_pending(&req.req_id);
                    return ApprovalResponse::deny();
                }
            },
            resp = &mut rx => {
                // Direct submit inside the probe window. submit_approval only
                // resolves `rx` (never drops the ack sender), so this is the sole
                // ready arm and the user's result wins unambiguously.
                self.inner.pending_acks.lock().unwrap().remove(&req.req_id);
                self.inner.pending_replay.lock().unwrap().remove(&req.req_id);
                return resp.unwrap_or_else(|_| ApprovalResponse::deny());
            }
            _ = tokio::time::sleep(probe) => {
                // No UI reachable in time: clean up, deny, and broadcast Finished
                // so any dialog windows that were created get destroyed.
                debug!(
                    "[Hub/{:?}] No approval UI acked within probe; denying req_id={}",
                    self.inner.mode, req.req_id
                );
                self.cleanup_local_pending(&req.req_id);
                self.notify_tauri_finished(&req.req_id);
                return ApprovalResponse::deny();
            }
        }

        // Phase 2: await the user's decision (no timeout).
        let response = match rx.await {
            Ok(response) => response,
            Err(_) => ApprovalResponse::deny(),
        };
        self.inner
            .pending_replay
            .lock()
            .unwrap()
            .remove(&req.req_id);
        self.inner.pending_acks.lock().unwrap().remove(&req.req_id);
        response
    }

    /// Remove all daemon-self bookkeeping for a req_id (Local / Aggregator).
    fn cleanup_local_pending(&self, req_id: &str) {
        self.inner.pending_approvals.lock().unwrap().remove(req_id);
        self.inner.pending_replay.lock().unwrap().remove(req_id);
        self.inner.pending_acks.lock().unwrap().remove(req_id);
    }

    /// Resolve the readiness probe for `req_id`. Returns whether the request is
    /// known (so the UI can enable its buttons). Layered so the ack is idempotent
    /// and never breaks worker-originated requests:
    ///   1. A probe oneshot is waiting -> fire it (daemon-self, phase 1).
    ///   2. Otherwise the daemon-self request is already past the probe (phase 2)
    ///      or being replayed -> still ready.
    ///   3. Otherwise a worker-originated request (routes/replay) -> ready, but
    ///      no probe is created (worker path is out of scope for P2 fallback).
    ///   4. Truly unknown -> not ready.
    pub fn notify_approval_ack(&self, req_id: &str) -> bool {
        if let Some(tx) = self.inner.pending_acks.lock().unwrap().remove(req_id) {
            let _ = tx.send(());
            return true;
        }
        if self
            .inner
            .pending_approvals
            .lock()
            .unwrap()
            .contains_key(req_id)
        {
            return true;
        }
        let known_route = self
            .inner
            .pending_routes
            .lock()
            .unwrap()
            .contains_key(req_id);
        let known_replay = self
            .inner
            .pending_replay
            .lock()
            .unwrap()
            .contains_key(req_id);
        known_route || known_replay
    }

    /// Resolve an approval. The dispatch depends on hub mode and request origin:
    /// - Local / Forwarder: look up local oneshot and send the response.
    /// - Aggregator with a worker-originated request (`pending_routes` hit):
    ///   send a directional `SecurityApprovalSubmit` to that forwarder's session
    ///   — never broadcast.
    /// - Aggregator with a daemon-self request (no route, but local oneshot in
    ///   `pending_approvals`): resolve the oneshot directly. This is the Arch IV
    ///   path where the daemon owns the WebRTC PC and originates the approval.
    ///
    /// Returns `true` if the response was successfully dispatched (oneshot
    /// resolved locally, or directional message handed to a registered forwarder
    /// session). Returns `false` if `req_id` is unknown to both routing tables.
    pub fn submit_approval(&self, req_id: &str, response: ApprovalResponse) -> bool {
        if self.inner.mode == HubMode::Aggregator {
            // Worker-originated requests: route directionally via pending_routes —
            // never broadcast SecurityApprovalSubmit (plan review #6/#7).
            if let Some(session_id) = self.pop_upstream_for_req(req_id) {
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
            // Fall through to the local-oneshot path below for daemon-self
            // requests originated via `request_approval`.
        }

        // Local / Forwarder, or Aggregator daemon-self: resolve the locally held
        // oneshot.
        let entry = self.inner.pending_approvals.lock().unwrap().remove(req_id);
        match entry {
            Some(PendingEntry { response_tx, .. }) => {
                let _ = response_tx.send(response);
                self.inner.pending_replay.lock().unwrap().remove(req_id);
                if matches!(self.inner.mode, HubMode::Local | HubMode::Aggregator) {
                    self.notify_tauri_finished(req_id);
                }
                true
            }
            None => {
                if self.inner.mode == HubMode::Aggregator {
                    debug!(
                        "[Hub/Aggregator] submit_approval: unknown req_id={req_id} (no route, no local pending)"
                    );
                }
                false
            }
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

    /// Aggregator-only: register a new approval request originated from
    /// `upstream_id`.
    ///
    /// "upstream" here refers strictly to a worker Forwarder session connected
    /// over `/ws/host_upstream`. Daemon-self approvals raised inside the
    /// aggregator process (Arch IV `daemon::pc_manager` path) do **not** call
    /// this API — they go through `request_approval` and store their oneshot
    /// in `pending_approvals` instead. Keeping the two sources in separate
    /// tables (`pending_routes` for upstream, `pending_approvals` for
    /// daemon-self) is what lets `submit_approval` route correctly without
    /// either broadcasting a worker reply or losing a daemon-self resolution.
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
    /// "upstream" here means the worker Forwarder ws session — this API is
    /// **only** for worker-originated approvals. Daemon-self approvals raised
    /// by `daemon::pc_manager` under Arch IV are issued via `request_approval`
    /// directly and never enter this path; the no-Tauri-deny short-circuit for
    /// daemon-self lives in `request_approval` itself.
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
        // Drop any in-flight readiness-probe senders so probes resolve at once.
        // Safe to do here: deny_all_pending denies, so whichever select! arm wins
        // (rx-deny or ack-Err) yields a deny — there is no good result to clobber.
        self.inner.pending_acks.lock().unwrap().clear();
    }

    /// Aggregator-only count: how many pending approvals are currently routed.
    pub fn pending_replay_count(&self) -> usize {
        self.inner.pending_replay.lock().unwrap().len()
    }

    /// Aggregator-only: cancel every in-flight approval because the last Tauri
    /// shell has disconnected. Two pending sources are cleaned up:
    /// 1. Worker-originated (`pending_routes` entries): a directional
    ///    `SecurityApprovalCancel` is delivered to the originating forwarder.
    /// 2. Daemon-self (`pending_approvals` entries owned by this hub): the
    ///    oneshot is resolved with `deny()` so business code unblocks
    ///    immediately rather than hanging on a UI that is no longer reachable.
    ///
    /// The routing / replay tables are cleared in either case. Returns the
    /// concatenated list of req_ids that were cancelled (worker-originated)
    /// or denied (daemon-self).
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
        // Daemon-self pending: drained from pending_approvals and resolved as
        // deny so request_approval() callers wake up promptly. deny_all_pending
        // also clears the replay table for any local oneshots.
        let daemon_self_count = self.inner.pending_approvals.lock().unwrap().len();
        if daemon_self_count > 0 {
            self.deny_all_pending();
        }
        // deny_all_pending clears the replay map for daemon-self entries; we
        // also need to clear any worker-route replay snapshots that
        // pending_routes used.
        self.inner.pending_replay.lock().unwrap().clear();

        let mut cancelled = Vec::with_capacity(routes.len());
        for (req_id, session_id) in routes {
            let msg = HostControlMessage::SecurityApprovalCancel {
                req_id: req_id.clone(),
            };
            self.route_to_forwarder(session_id, msg);
            cancelled.push(req_id);
        }
        if !cancelled.is_empty() || daemon_self_count > 0 {
            warn!(
                "[Hub/Aggregator] Tauri lost — cancelled {} worker-originated and denied {} daemon-self approval(s)",
                cancelled.len(),
                daemon_self_count
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

    // Arch IV: Aggregator now originates daemon-self approvals (the daemon owns
    // the WebRTC PC and runs `check_security_permission` for RequireControl).
    // Without a Tauri shell connected the request denies fast — same shape as
    // the Local-no-subscriber path.
    #[tokio::test]
    async fn aggregator_request_approval_no_tauri_denies_fast() {
        let hub = HostControlHub::new_aggregator();
        let started = std::time::Instant::now();
        let resp = tokio::time::timeout(
            Duration::from_millis(200),
            hub.request_approval(approval_req("r1")),
        )
        .await
        .expect("must not block");
        assert!(!resp.approved);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(hub.pending_replay_count(), 0);
    }

    // Arch IV regression: Aggregator with a Tauri shell present must broadcast
    // the SecurityApprovalRequest and pend until submit_approval resolves the
    // oneshot. This is the exact path RequireControl takes after PR 2 moved the
    // PC into daemon — before the fix it hit the old "router does not request"
    // hard-deny and the Tauri shell never saw a dialog.
    #[tokio::test]
    async fn aggregator_request_approval_pends_until_submit() {
        let hub = HostControlHub::new_aggregator();
        let mut outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r1")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Replay snapshot recorded so a reconnecting Tauri can resume the dialog.
        assert_eq!(hub.pending_replay_count(), 1);
        let replay = hub.replay_messages_for_tauri();
        assert_eq!(replay.len(), 1);
        match &replay[0] {
            HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
            other => panic!("unexpected replay frame: {other:?}"),
        }

        // Tauri saw the broadcast.
        let bcast = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .expect("Request must be broadcast")
            .expect("channel ok");
        match bcast {
            HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
            other => panic!("expected SecurityApprovalRequest, got {other:?}"),
        }

        let solved = hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            },
        );
        assert!(solved, "daemon-self submit must resolve the local oneshot");

        let resp = tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("oneshot must resolve")
            .expect("task ok");
        assert!(resp.approved);
        assert_eq!(hub.pending_replay_count(), 0);

        // Finished frame is broadcast too so the Tauri shell drops always-on-top.
        let finished = tokio::time::timeout(Duration::from_millis(100), outbound_rx.recv())
            .await
            .expect("Finished must be broadcast")
            .expect("channel ok");
        match finished {
            HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
            other => panic!("expected SecurityApprovalFinished, got {other:?}"),
        }
    }

    // Mixed sources: Aggregator handles a daemon-self request and a worker-
    // originated request concurrently. Each submit must reach exactly the right
    // resolver — the daemon-self oneshot for the daemon req, the originating
    // forwarder's mpsc for the worker req. They must NOT cross-contaminate.
    #[tokio::test]
    async fn aggregator_mixed_daemon_self_and_worker_routes_correctly() {
        let hub = HostControlHub::new_aggregator();
        hub.mark_tauri_connected();
        let _outbound_rx = hub.subscribe_outbound();

        // Worker-originated request via upstream registration.
        let (tx_w, mut rx_w) = mpsc::unbounded_channel();
        hub.register_forwarder_session(7, tx_w);
        hub.register_upstream_request(
            "r-worker".to_string(),
            7,
            SecurityPermissionType::Terminal,
            None,
        );

        // Daemon-self request via request_approval.
        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r-daemon")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hub.pending_replay_count(), 2);

        // Submit the worker req — forwarder mpsc must get the directional Submit.
        assert!(hub.submit_approval(
            "r-worker",
            ApprovalResponse {
                approved: true,
                remember: false,
            }
        ));
        match tokio::time::timeout(Duration::from_millis(100), rx_w.recv())
            .await
            .expect("forwarder must receive")
            .expect("mpsc ok")
        {
            HostControlMessage::SecurityApprovalSubmit { req_id, .. } => {
                assert_eq!(req_id, "r-worker")
            }
            other => panic!("unexpected: {other:?}"),
        }
        // Daemon-self task must still be pending — the worker submit must not
        // accidentally resolve it.
        assert!(!task.is_finished());

        // Submit the daemon req — local oneshot resolves.
        assert!(hub.submit_approval(
            "r-daemon",
            ApprovalResponse {
                approved: false,
                remember: true,
            }
        ));
        let resp = tokio::time::timeout(Duration::from_millis(200), task)
            .await
            .expect("oneshot must resolve")
            .expect("task ok");
        assert!(!resp.approved);
        assert!(resp.remember);

        // Forwarder must NOT have received anything else.
        let stray = tokio::time::timeout(Duration::from_millis(50), rx_w.recv()).await;
        assert!(stray.is_err(), "forwarder must not see r-daemon submit");

        assert_eq!(hub.pending_replay_count(), 0);
    }

    // When the last Tauri shell drops, daemon-self pending approvals must be
    // resolved as deny (so request_approval callers unblock) in addition to the
    // existing forwarder-cancel path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn aggregator_tauri_loss_denies_daemon_self_pending() {
        let hub = HostControlHub::new_aggregator();
        hub.mark_tauri_connected();
        let _outbound_rx = hub.subscribe_outbound();

        // One worker req + one daemon-self req in flight.
        let (tx_w, mut rx_w) = mpsc::unbounded_channel();
        hub.register_forwarder_session(3, tx_w);
        hub.register_upstream_request(
            "r-worker".to_string(),
            3,
            SecurityPermissionType::FileTransfer,
            None,
        );
        let h = hub.clone();
        let task = tokio::spawn(async move { h.request_approval(approval_req("r-daemon")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(hub.pending_replay_count(), 2);

        // Tauri lost.
        let cancelled = hub.cancel_all_for_tauri_loss();
        assert_eq!(cancelled, vec!["r-worker".to_string()]);

        // Daemon-self oneshot resolved as deny (does not appear in `cancelled`
        // because that list reports worker-originated cancels by contract).
        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("daemon oneshot must resolve")
            .expect("task ok");
        assert!(!resp.approved);

        // Worker forwarder received the directional Cancel.
        match tokio::time::timeout(Duration::from_millis(100), rx_w.recv())
            .await
            .expect("forwarder must receive")
            .expect("mpsc ok")
        {
            HostControlMessage::SecurityApprovalCancel { req_id } => assert_eq!(req_id, "r-worker"),
            other => panic!("unexpected: {other:?}"),
        }

        assert_eq!(hub.pending_replay_count(), 0);
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

    // P2: helper to wait until the readiness-probe entry for `req_id` exists.
    async fn wait_for_pending_ack(hub: &HostControlHub, req_id: &str) {
        for _ in 0..200 {
            if hub.inner.pending_acks.lock().unwrap().contains_key(req_id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("pending_acks never registered for {req_id}");
    }

    // P2: an ack within the probe window advances to phase 2 (unbounded wait),
    // where a later submit resolves the request normally.
    #[tokio::test]
    async fn local_ack_enters_wait_then_submit_resolves() {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task = tokio::spawn(async move {
            hub_clone
                .request_approval_inner(approval_req("r1"), Duration::from_secs(2))
                .await
        });
        wait_for_pending_ack(&hub, "r1").await;

        assert!(hub.notify_approval_ack("r1"), "ack must hit the probe");
        // Now in phase 2 (no timeout). Submit after a short delay.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(hub.submit_approval(
            "r1",
            ApprovalResponse {
                approved: true,
                remember: false,
            }
        ));
        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(resp.approved);
        assert_eq!(hub.pending_replay_count(), 0);
        assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
    }

    // P2 (codex #1): zero ack within the probe window denies, clears all
    // daemon-self bookkeeping, and broadcasts Finished so any created windows die.
    #[tokio::test]
    async fn local_probe_timeout_denies_and_broadcasts_finished() {
        let hub = HostControlHub::new_local();
        let mut outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task = tokio::spawn(async move {
            hub_clone
                .request_approval_inner(approval_req("r1"), Duration::from_millis(50))
                .await
        });

        // The initial Request is broadcast.
        match tokio::time::timeout(Duration::from_millis(200), outbound_rx.recv())
            .await
            .expect("Request must broadcast")
            .expect("channel ok")
        {
            HostControlMessage::SecurityApprovalRequest { req_id, .. } => assert_eq!(req_id, "r1"),
            other => panic!("expected Request, got {other:?}"),
        }

        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(!resp.approved, "probe timeout must deny");

        // Finished is broadcast so per-monitor windows get destroyed.
        match tokio::time::timeout(Duration::from_millis(200), outbound_rx.recv())
            .await
            .expect("Finished must broadcast")
            .expect("channel ok")
        {
            HostControlMessage::SecurityApprovalFinished { req_id } => assert_eq!(req_id, "r1"),
            other => panic!("expected Finished, got {other:?}"),
        }

        assert_eq!(hub.pending_replay_count(), 0);
        assert!(hub.inner.pending_approvals.lock().unwrap().is_empty());
        assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
    }

    // P2 (codex #2): ack is idempotent. The first ack fires the probe oneshot;
    // a replayed ack after the request has entered phase 2 still reports ready
    // (pending_approvals still holds it).
    #[tokio::test]
    async fn notify_approval_ack_idempotent_in_phase2() {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task = tokio::spawn(async move {
            hub_clone
                .request_approval_inner(approval_req("r1"), Duration::from_secs(2))
                .await
        });
        wait_for_pending_ack(&hub, "r1").await;

        assert!(hub.notify_approval_ack("r1"), "first ack fires the probe");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            hub.notify_approval_ack("r1"),
            "replayed ack in phase 2 must still be ready"
        );

        assert!(hub.submit_approval("r1", ApprovalResponse::deny()));
        let _ = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
    }

    // P2 (codex #3): worker-originated requests are "ready" without creating a
    // probe, so the shared approval page does not break them; the directional
    // route remains intact for submit.
    #[test]
    fn notify_approval_ack_worker_originated_is_ready_without_probe() {
        let hub = HostControlHub::new_aggregator();
        let (tx, _rx) = mpsc::unbounded_channel();
        hub.register_forwarder_session(9, tx);
        hub.register_upstream_request("r-w".to_string(), 9, SecurityPermissionType::Terminal, None);

        assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
        assert!(
            hub.notify_approval_ack("r-w"),
            "worker req must report ready"
        );
        // No probe was created.
        assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
        // Directional route still resolves.
        assert_eq!(hub.pop_upstream_for_req("r-w"), Some(9));
    }

    // P2: a truly unknown req_id is not ready.
    #[test]
    fn notify_approval_ack_unknown_returns_false() {
        let hub = HostControlHub::new_local();
        assert!(!hub.notify_approval_ack("ghost"));
    }

    // P2 (codex #2/four-round): a direct submit inside the probe window must win
    // over the probe deny. submit_approval never touches pending_acks, so the
    // select! `rx` arm is the only ready arm. Looped to shake out select!
    // randomness.
    #[tokio::test]
    async fn direct_submit_during_probe_wins_over_deny() {
        for _ in 0..20 {
            let hub = HostControlHub::new_local();
            let _outbound_rx = hub.subscribe_outbound();
            hub.mark_tauri_connected();

            let hub_clone = hub.clone();
            let task = tokio::spawn(async move {
                hub_clone
                    .request_approval_inner(approval_req("r1"), Duration::from_secs(2))
                    .await
            });
            wait_for_pending_ack(&hub, "r1").await;

            // Direct submit, no ack.
            assert!(hub.submit_approval(
                "r1",
                ApprovalResponse {
                    approved: true,
                    remember: false,
                }
            ));
            let resp = tokio::time::timeout(Duration::from_millis(500), task)
                .await
                .expect("must resolve")
                .unwrap();
            assert!(resp.approved, "direct submit result must win over deny");
            assert!(
                hub.inner.pending_acks.lock().unwrap().is_empty(),
                "pending_acks must not leak"
            );
        }
    }

    // P2 (codex #1, three-round): Forwarder never registers a readiness probe and
    // resolves via the upstream-delivered submit (the worker is authoritative).
    #[tokio::test]
    async fn forwarder_request_registers_no_pending_acks() {
        let upstream = UpstreamForwarder::new_for_test(true);
        let upstream_clone = Arc::clone(&upstream);
        let hub = HostControlHub::new_forwarder(upstream);

        let hub_clone = hub.clone();
        let task =
            tokio::spawn(async move { hub_clone.request_approval(approval_req("r1")).await });
        for _ in 0..50 {
            if hub
                .inner
                .pending_approvals
                .lock()
                .unwrap()
                .contains_key("r1")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            hub.inner.pending_acks.lock().unwrap().is_empty(),
            "Forwarder must not create a readiness probe"
        );

        upstream_clone.test_inject_inbound(HostControlMessage::SecurityApprovalSubmit {
            req_id: "r1".to_string(),
            approved: true,
            remember: false,
        });
        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(resp.approved);
    }

    // P2: deny_all_pending also drops any in-flight readiness probes.
    #[tokio::test]
    async fn deny_all_pending_clears_pending_acks() {
        let hub = HostControlHub::new_local();
        let _outbound_rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();

        let hub_clone = hub.clone();
        let task = tokio::spawn(async move {
            hub_clone
                .request_approval_inner(approval_req("r1"), Duration::from_secs(5))
                .await
        });
        wait_for_pending_ack(&hub, "r1").await;

        hub.deny_all_pending();
        let resp = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("must resolve")
            .unwrap();
        assert!(!resp.approved);
        assert!(hub.inner.pending_acks.lock().unwrap().is_empty());
    }
}
