//! Host Control Hub — unified Tauri-side bridge across all server deployment modes.
//!
//! The Aggregator contract works as follows:
//!
//! - **Local** (portable): the embedded server publishes commands to its own ws
//!   endpoint; the embedded Tauri shell is a ws client.
//! - **Aggregator** (ServiceDaemon): the daemon plays *two* roles
//!   simultaneously — it is both a router for worker→Tauri approval traffic
//!   *and* the originator of daemon-self approvals. The WebRTC PeerConnection
//!   lives in the daemon process, so `RequireControl`-driven approvals are
//!   raised by `daemon::pc_manager` directly through `request_approval`. The
//!   aggregator therefore owns business logic, not just routing.
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
use std::time::Duration;

use log::{debug, info, warn};
use tokio::sync::{broadcast, mpsc, oneshot};
use uuid::Uuid;

use desk_signal_facade::model::security_settings::DEFAULT_APPROVAL_TIMEOUT_SECS;

use crate::model::security_approval::SecurityPermissionType;

pub use protocol::{
    ApprovalRequest, ApprovalResponse, CentralSyncState, ClientRole, HostAccessSession,
    HostAccessSnapshot, HostControlMessage, HostFileTransferDirection, HostFileTransferSummary,
    HostRemoteAccessMode, HostRemoteAccessStatus, ServiceOpKind,
};
pub use upstream::UpstreamForwarder;

/// Capacity of internal broadcast channels.
const CMD_BROADCAST_CAPACITY: usize = 256;
const STATE_BROADCAST_CAPACITY: usize = 64;

/// How long `request_approval` waits for at least one approval UI to acknowledge
/// it is mounted and able to talk to the backend before denying. This is a pure
/// readiness probe (loopback-local), not the user-decision timeout: once any UI
/// acks in time the request proceeds to the user-decision wait, which is bounded
/// by the host's configured `approval_timeout` (see [`server_approval_timeout`];
/// unbounded only when configured to "never"). Only applies to Local / Aggregator
/// (daemon-self) requests; Forwarder is exempt.
const APPROVAL_UI_READY_PROBE: Duration = Duration::from_secs(10);

/// Server-side grace added on top of the host-configured `approval_timeout` for
/// the authoritative decision wait. The approval dialog front-end runs the same
/// countdown and denies at the configured value; the server's timer trails by
/// this window so the front-end's explicit deny normally lands first, leaving the
/// server as a fail-closed backstop for when the front-end is gone (window closed
/// or a non-browser controller). This is a best-effort bias — front-end and
/// server timers start from different points — not a hard ordering guarantee.
const APPROVAL_SERVER_GRACE: Duration = Duration::from_secs(3);

/// Translate the host's configured `approval_timeout` (seconds) into the server's
/// authoritative decision wait.
///
/// - `Some(0)` — never time out (no timer arm; the dialog waits indefinitely).
/// - `Some(n>0)` — `n` seconds plus [`APPROVAL_SERVER_GRACE`].
/// - `None` — collapses to [`DEFAULT_APPROVAL_TIMEOUT_SECS`] (plus grace), NOT an
///   unbounded wait. A `None` should already have been normalized upstream, but
///   treating it as the finite default here keeps the server from ever failing
///   open to "wait forever" if some path skipped normalization.
pub(crate) fn server_approval_timeout(configured: Option<u32>) -> Option<Duration> {
    match configured.unwrap_or(DEFAULT_APPROVAL_TIMEOUT_SECS) {
        0 => None,
        n => Some(Duration::from_secs(n as u64) + APPROVAL_SERVER_GRACE),
    }
}

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

/// Identifies one question: this controller, this capability. Two gates that
/// arrive with the same pair are asking the same thing.
type SharedPromptKey = (String, SecurityPermissionType);

/// One in-flight approval awaiting user response.
struct PendingEntry {
    response_tx: oneshot::Sender<ApprovalResponse>,
    from_connection_id: Option<String>,
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
            permission_type: self.permission_type,
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
    /// One in-flight prompt per (connection, capability), with everyone else
    /// asking the same question queued behind it. Without this, two commands
    /// arriving together on a fresh connection each mint their own request id
    /// and the user is shown two identical dialogs for one decision.
    shared_prompts: Mutex<HashMap<SharedPromptKey, Vec<oneshot::Sender<ApprovalResponse>>>>,
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
    host_activity: crate::host_activity::HostActivityRegistry,
    remote_access_gate: crate::daemon::remote_access::RemoteAccessGate,
    remote_access_coordinator:
        std::sync::OnceLock<Arc<crate::daemon::remote_access::RemoteAccessCoordinator>>,
}

/// Owns one shared prompt while it is being asked.
///
/// Settling hands the answer to everyone queued behind it. Dropping without
/// settling — the asking task was cancelled — drops their channels instead,
/// which they read as a denial rather than waiting for an answer that will
/// never come.
struct SharedPromptGuard {
    hub: HostControlHub,
    key: SharedPromptKey,
}

impl SharedPromptGuard {
    fn take(&self) -> Vec<oneshot::Sender<ApprovalResponse>> {
        self.hub
            .inner
            .shared_prompts
            .lock()
            .unwrap()
            .remove(&self.key)
            .unwrap_or_default()
    }

    fn settle(&self, response: ApprovalResponse) {
        for waiter in self.take() {
            let _ = waiter.send(response);
        }
    }
}

impl Drop for SharedPromptGuard {
    fn drop(&mut self) {
        // A no-op after `settle`, which already removed the entry.
        let _ = self.take();
    }
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
        let host_activity = crate::host_activity::HostActivityRegistry::new(cmd_tx.clone());
        let inner = HubInner {
            mode,
            cmd_tx,
            state_tx,
            pending_approvals: Mutex::new(HashMap::new()),
            pending_replay: Mutex::new(HashMap::new()),
            pending_acks: Mutex::new(HashMap::new()),
            pending_routes: Mutex::new(HashMap::new()),
            shared_prompts: Mutex::new(HashMap::new()),
            forwarder_sessions: Mutex::new(HashMap::new()),
            upstream,
            tauri_client_count: AtomicUsize::new(0),
            host_activity,
            remote_access_gate: crate::daemon::remote_access::RemoteAccessGate::startup_locked(),
            remote_access_coordinator: std::sync::OnceLock::new(),
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

    pub fn host_activity(&self) -> crate::host_activity::HostActivityRegistry {
        self.inner.host_activity.clone()
    }

    pub fn remote_access_gate(&self) -> crate::daemon::remote_access::RemoteAccessGate {
        self.inner.remote_access_gate.clone()
    }

    pub fn install_remote_access_coordinator(
        &self,
        coordinator: Arc<crate::daemon::remote_access::RemoteAccessCoordinator>,
    ) -> Result<(), Arc<crate::daemon::remote_access::RemoteAccessCoordinator>> {
        self.inner.remote_access_coordinator.set(coordinator)
    }

    pub fn remote_access_coordinator(
        &self,
    ) -> Option<Arc<crate::daemon::remote_access::RemoteAccessCoordinator>> {
        self.inner.remote_access_coordinator.get().cloned()
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
    /// Aggregator note: the daemon process owns the WebRTC PC and
    /// therefore originates `RequireControl`-driven approvals itself (in addition
    /// to relaying worker-originated requests via `handle_upstream_approval_request`).
    /// The two sources share the same broadcast → Tauri path and are disambiguated
    /// at submit time: routes registered via `register_upstream_request` win the
    /// directional dispatch, otherwise the local oneshot is resolved.
    /// Ask the user, reusing an answer already being asked for.
    ///
    /// A controller that opens a file manager and starts a transfer in the same
    /// breath reaches two gates at once, both on the same capability and the
    /// same connection. They are one question, so the first caller raises the
    /// dialog and the rest wait on its answer instead of stacking dialogs the
    /// user has to dismiss one by one.
    ///
    /// Requests with no originating connection are not shared: without a
    /// connection there is nothing to key them by, and they are rare enough
    /// (host-initiated paths) that the duplicate dialog never arises.
    pub async fn request_approval_shared(
        &self,
        req: ApprovalRequest,
        approval_timeout: Option<Duration>,
    ) -> ApprovalResponse {
        let Some(connection_id) = req.from_connection_id.clone() else {
            return self.request_approval(req, approval_timeout).await;
        };
        let key = (connection_id, req.permission_type);
        let follower = {
            let mut shared = self.inner.shared_prompts.lock().unwrap();
            match shared.get_mut(&key) {
                Some(waiters) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    Some(rx)
                }
                None => {
                    shared.insert(key.clone(), Vec::new());
                    None
                }
            }
        };
        if let Some(rx) = follower {
            // A closed channel means the asking task went away before it could
            // answer, which is not a reason to let the command through.
            return rx.await.unwrap_or_else(|_| ApprovalResponse::deny());
        }

        // From here the entry belongs to this task. The guard hands the answer
        // to everyone waiting, and — should this task be dropped mid-prompt —
        // drops their senders instead, which the arm above reads as a denial.
        let guard = SharedPromptGuard {
            hub: self.clone(),
            key,
        };
        let response = self.request_approval(req, approval_timeout).await;
        guard.settle(response);
        response
    }

    pub async fn request_approval(
        &self,
        req: ApprovalRequest,
        approval_timeout: Option<Duration>,
    ) -> ApprovalResponse {
        self.request_approval_inner(req, APPROVAL_UI_READY_PROBE, approval_timeout)
            .await
    }

    /// Core of [`request_approval`] with an injectable readiness-probe duration so
    /// tests do not have to wait the real [`APPROVAL_UI_READY_PROBE`].
    ///
    /// `approval_timeout` is the authoritative bound on the user-decision wait
    /// (see [`server_approval_timeout`]): `Some(dur)` fires a fail-closed deny on
    /// expiry, `None` waits indefinitely (the configured "never").
    ///
    /// Local / Aggregator (daemon-self) is two-phase:
    ///   * Phase 1 (readiness probe): wait for any approval UI to ack, while also
    ///     racing a possible direct submit and the probe timeout.
    ///   * Phase 2 (user decision): once ready, await the user's response bounded
    ///     by `approval_timeout`. On expiry the entry is claimed atomically against
    ///     a concurrent submit before denying (see below).
    ///
    /// Forwarder is exempt from the probe: the worker is authoritative and the
    /// daemon drives the dialog, so it awaits the upstream-delivered response
    /// directly (registering a local ack would wait for an ack that never comes).
    /// Its `approval_timeout` expiry additionally tells the aggregator to tear
    /// down its routing/replay tables and dialog via `SecurityApprovalResolved`.
    async fn request_approval_inner(
        &self,
        req: ApprovalRequest,
        probe: Duration,
        approval_timeout: Option<Duration>,
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
        let permission_type = req.permission_type;
        let snapshot = ReplaySnapshot {
            req_id: req.req_id.clone(),
            permission_type,
            from_connection_id: req.from_connection_id.clone(),
        };

        self.inner.pending_approvals.lock().unwrap().insert(
            req.req_id.clone(),
            PendingEntry {
                response_tx: tx,
                from_connection_id: req.from_connection_id.clone(),
            },
        );

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

        // Forwarder: no local readiness probe (see method docs). The worker holds
        // the authoritative timer for its own request here. On expiry it claims
        // its local pending, tells the aggregator to tear down the routing/replay
        // tables and Tauri dialog via `SecurityApprovalResolved`, and denies
        // fail-closed. `None` waits indefinitely (configured "never").
        if self.inner.mode == HubMode::Forwarder {
            return match approval_timeout {
                None => rx.await.unwrap_or_else(|_| ApprovalResponse::deny()),
                Some(dur) => {
                    let decided = tokio::select! {
                        // Bias toward the response: if a submit already resolved
                        // `rx`, that arm is polled first and the timer never fires.
                        biased;
                        resp = &mut rx => {
                            Some(resp.unwrap_or_else(|_| ApprovalResponse::deny()))
                        }
                        _ = tokio::time::sleep(dur) => {
                            // Claim to arbitrate against a concurrent submit_approval.
                            if self
                                .inner
                                .pending_approvals
                                .lock()
                                .unwrap()
                                .remove(&req.req_id)
                                .is_some()
                            {
                                // We won: no submit in flight. Ask the aggregator to
                                // clean up its routing/replay/dialog for this req_id.
                                let _ = self.send_command(
                                    HostControlMessage::SecurityApprovalResolved {
                                        req_id: req.req_id.clone(),
                                    },
                                );
                                Some(ApprovalResponse::deny())
                            } else {
                                // submit_approval already claimed the entry; it will
                                // (or already did) send on the oneshot. Fall through
                                // to await it below, rather than reading a possibly
                                // still-empty channel and denying a real approve.
                                None
                            }
                        }
                    };
                    match decided {
                        Some(r) => r,
                        // The submit that beat the timer will deliver here.
                        None => rx.await.unwrap_or_else(|_| ApprovalResponse::deny()),
                    }
                }
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

        // Phase 2: await the user's decision, bounded by the authoritative timeout.
        let response = match approval_timeout {
            None => match rx.await {
                Ok(response) => response,
                Err(_) => ApprovalResponse::deny(),
            },
            Some(dur) => {
                let decided = tokio::select! {
                    // Bias toward the response: if submit_approval already resolved
                    // `rx`, that arm wins and the timer arm is never taken, so a
                    // user's approve is never overridden by an equal-instant timeout.
                    biased;
                    resp = &mut rx => Some(resp.unwrap_or_else(|_| ApprovalResponse::deny())),
                    _ = tokio::time::sleep(dur) => {
                        // Timed out. Claim the pending entry to arbitrate against a
                        // concurrent submit_approval. Removing it means no submit is
                        // in flight: fail closed and emit exactly one Finished.
                        if self
                            .inner
                            .pending_approvals
                            .lock()
                            .unwrap()
                            .remove(&req.req_id)
                            .is_some()
                        {
                            self.inner.pending_replay.lock().unwrap().remove(&req.req_id);
                            self.inner.pending_acks.lock().unwrap().remove(&req.req_id);
                            self.notify_tauri_finished(&req.req_id);
                            Some(ApprovalResponse::deny())
                        } else {
                            // submit_approval already claimed the entry between its
                            // remove and its send; it emits the Finished. Fall
                            // through to await its decision instead of reading a
                            // possibly still-empty channel and denying a real
                            // approve (exactly-once: no second Finished here).
                            None
                        }
                    }
                };
                match decided {
                    Some(r) => r,
                    // The submit that beat the timer will deliver here.
                    None => rx.await.unwrap_or_else(|_| ApprovalResponse::deny()),
                }
            }
        };
        // Normal-exit cleanup. On the timeout-claim path the tables are already
        // cleared above, so these removes are idempotent no-ops there.
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
    ///      no probe is created because the worker owns that readiness path.
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
    ///   `pending_approvals`): resolve the oneshot directly. This is the
    ///   path where the daemon owns the WebRTC PC and originates the approval.
    ///
    /// Returns `true` if the response was successfully dispatched (oneshot
    /// resolved locally, or directional message handed to a registered forwarder
    /// session). Returns `false` if `req_id` is unknown to both routing tables.
    pub fn submit_approval(&self, req_id: &str, response: ApprovalResponse) -> bool {
        if self.inner.mode == HubMode::Aggregator {
            // Worker-originated requests route directionally via pending_routes;
            // never broadcast SecurityApprovalSubmit.
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

    /// Deny every pending approval owned by a host-terminated connection.
    ///
    /// This covers both daemon-local requests and worker-originated requests
    /// routed by an aggregator. Entries are removed before messages are sent so
    /// a concurrent user response cannot approve work after disconnection.
    pub fn cancel_pending_for_connection(&self, connection_id: &str) -> Vec<String> {
        let local_ids = {
            let pending = self.inner.pending_approvals.lock().unwrap();
            pending
                .iter()
                .filter(|(_, entry)| entry.from_connection_id.as_deref() == Some(connection_id))
                .map(|(req_id, _)| req_id.clone())
                .collect::<Vec<_>>()
        };

        let mut cancelled = Vec::new();
        for req_id in local_ids {
            if self.cancel_local_request(&req_id) {
                cancelled.push(req_id);
            }
        }

        if self.inner.mode == HubMode::Aggregator {
            let routed_ids = {
                let replay = self.inner.pending_replay.lock().unwrap();
                replay
                    .iter()
                    .filter(|(_, snapshot)| {
                        snapshot.from_connection_id.as_deref() == Some(connection_id)
                    })
                    .map(|(req_id, _)| req_id.clone())
                    .collect::<Vec<_>>()
            };
            for req_id in routed_ids {
                if self.cancel_routed_request(&req_id) {
                    cancelled.push(req_id);
                }
            }
        }

        cancelled
    }

    /// Fail-closed cleanup used by LockAll. Unlike the connection-specific path,
    /// this also catches requests that have no connection id yet.
    pub fn cancel_all_pending_for_security_lock(&self) -> Vec<String> {
        let local_ids = self
            .inner
            .pending_approvals
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let routed_ids = if self.inner.mode == HubMode::Aggregator {
            self.inner
                .pending_routes
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let mut cancelled = Vec::with_capacity(local_ids.len() + routed_ids.len());
        for req_id in local_ids {
            if self.cancel_local_request(&req_id) {
                cancelled.push(req_id);
            }
        }
        for req_id in routed_ids {
            if self.cancel_routed_request(&req_id) {
                cancelled.push(req_id);
            }
        }
        cancelled
    }

    fn cancel_local_request(&self, req_id: &str) -> bool {
        let entry = self.inner.pending_approvals.lock().unwrap().remove(req_id);
        let Some(entry) = entry else {
            return false;
        };
        let _ = entry.response_tx.send(ApprovalResponse::deny());
        self.inner.pending_replay.lock().unwrap().remove(req_id);
        self.inner.pending_acks.lock().unwrap().remove(req_id);
        if self.inner.mode == HubMode::Forwarder {
            let _ = self.send_command(HostControlMessage::SecurityApprovalResolved {
                req_id: req_id.to_string(),
            });
        } else {
            self.notify_tauri_finished(req_id);
        }
        true
    }

    fn cancel_routed_request(&self, req_id: &str) -> bool {
        let session_id = self.inner.pending_routes.lock().unwrap().remove(req_id);
        let Some(session_id) = session_id else {
            return false;
        };
        self.inner.pending_replay.lock().unwrap().remove(req_id);
        self.inner.pending_acks.lock().unwrap().remove(req_id);
        let _ = self.route_to_forwarder(
            session_id,
            HostControlMessage::SecurityApprovalCancel {
                req_id: req_id.to_string(),
            },
        );
        self.notify_tauri_finished(req_id);
        true
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

    /// Aggregator-only: a worker forwarder reports that it resolved `req_id`
    /// locally (e.g. its authoritative timeout fired), so the aggregator should
    /// tear down the routing/replay tables and close the Tauri dialog.
    ///
    /// Ownership is enforced: only the forwarder session that registered `req_id`
    /// may resolve it. A `SecurityApprovalResolved` naming a req_id owned by a
    /// different session (or none) is ignored — this prevents one forwarder from
    /// cancelling another forwarder's pending approval. Returns `true` when a
    /// matching request was cleaned up.
    pub fn resolve_upstream_request(&self, req_id: &str, session_id: UpstreamSessionId) -> bool {
        {
            let mut routes = self.inner.pending_routes.lock().unwrap();
            match routes.get(req_id) {
                Some(owner) if *owner == session_id => {
                    routes.remove(req_id);
                }
                Some(_) => {
                    warn!(
                        "[Hub/Aggregator] SecurityApprovalResolved req_id={req_id} from \
                         session_id={session_id} that does not own it; ignoring"
                    );
                    return false;
                }
                None => return false,
            }
        }
        self.inner.pending_replay.lock().unwrap().remove(req_id);
        self.notify_tauri_finished(req_id);
        true
    }

    /// Aggregator-only: register a new approval request originated from
    /// `upstream_id`.
    ///
    /// "upstream" here refers strictly to a worker Forwarder session connected
    /// over `/ws/host_upstream`. Daemon-self approvals raised inside the
    /// aggregator process (`daemon::pc_manager` path) do **not** call
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
    /// by `daemon::pc_manager` are issued via `request_approval`
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
            permission_type,
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
    /// drop the forwarder session's outbound mpsc registration, and close each
    /// drained request's Tauri dialog. Called when a forwarder disconnects: the
    /// originating worker is gone, so its authoritative `SecurityApprovalResolved`
    /// may never arrive and the aggregator must broadcast `Finished` itself.
    /// Returns the list of req_ids that were drained.
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
        // Close the dialogs whose originating worker just vanished.
        for req_id in &out {
            self.notify_tauri_finished(req_id);
        }
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
mod tests;
