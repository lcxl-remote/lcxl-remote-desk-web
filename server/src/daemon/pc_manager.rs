//! # Daemon-side WebRTC PeerConnection manager
//!
//! Owner of the [`webrtc::peer_connection::RTCPeerConnection`] lifecycle.
//! The daemon owns the PC so WebRTC negotiation happens once per browser
//! session and survives every worker swap. Worker replacement becomes
//! invisible to the browser apart from a ~1 s frame freeze waiting for the
//! next IDR from the new encoder.
//!
//! Were the worker process to hold the PC instead, every UAC /
//! lock-screen / OS-session-switch (any event that respawns the worker)
//! would tear down the PC and force the browser through full SDP
//! renegotiation + ICE restart — a path that becomes unstable under
//! SYSTEM-token + Winlogon desktop combinations and shows up as "video
//! garbled / ICE checking → failed" during UAC.
//!
//! The [`PcRegistry`] holds per-`SignalingType` handlers for the five
//! WebRTC SDP/ICE messages the daemon owns
//! (`RequestRemote` / `Offer` / `Answer` / `Canid` / `CloseControl`),
//! feeds the worker's media transport into the per-PC tracks it holds,
//! and registers the DataChannel handlers on top.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{
    OfferModel, RequestRemoteModel, SignalingModel, SignalingState, SignalingType,
};
use desk_utils::error::{CustomDeskError, DeskErrorCode};
use tokio::sync::{RwLock, broadcast, mpsc};
use webrtc::api::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9,
};
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::peer_connection::{RTCPeerConnection, peer_connection_state::RTCPeerConnectionState};
use webrtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::payload_feedbacks::receiver_estimated_maximum_bitrate::ReceiverEstimatedMaximumBitrate;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::daemon::bitrate_controller::{
    AdaptiveBitrateShared, AdaptiveBitrateState, CapDirective,
};
use crate::daemon::codec_negotiation;
use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use crate::host_control::HostControlHub;
use crate::model::data_channel::SignalRequestControlData;
use crate::model::security_approval::{
    SecurityPermissionType, check_security_permission, effective_permission,
};
use crate::model::settings::{Settings, SharedSettings};
use crate::service::signaling::{should_short_circuit_clipboard, should_short_circuit_control};
use desk_capture_engine::audio_encoder::audio_encoder_factory::list_audio_encoder;
use desk_capture_engine::model::video_encoder::{VideoEncoderType, VideoEncoderTypeHelper};
use desk_capture_engine::video_encoder::video_encoder_factory::list_video_encoder;
use desk_ipc_protocol::message::{
    ClipboardPayload, CursorDataPayload, FileTransferPayload, FileTransferSendErrorKind,
    FileTransferSendFailedPayload, ForceKeyframePayload, MediaCapabilities, MediaCodec, MediaFrame,
    MediaFrameKind, ServiceToWorker, StartMediaPayload, StopMediaPayload,
    UpdateMediaSettingsPayload,
};
use desk_signal_facade::model::signal::InitSignalingData;
use std::time::{Duration, Instant};
use webrtc::media::Sample;

/// Bounded capacity of the per-connection file-transfer writer queue.
///
/// Sized just above the SCTP-internal high-watermark so the daemon's
/// drain task `await`s on a full queue (= the worker's file lane is
/// being drained faster than the browser's DC accepts). The
/// `await`-on-full propagates back through the file pipe to the
/// worker's `emit_*` helpers, which ultimately blocks
/// `serve_download` before reading the next disk chunk — the central
/// piece of the end-to-end backpressure chain re-established for
/// daemon mode.
///
/// Choosing 16 rather than something larger keeps the per-PC memory
/// footprint bounded at ~960 KB (16 × 60 KB chunk) regardless of
/// active downloads.
const FILE_TRANSFER_WRITER_QUEUE_CAP: usize = 16;

/// Rolling per-window accumulator for the daemon-side file-transfer
/// writer task. Each window flushes one `[ft-metrics-daemon]` INFO
/// line cross-referenceable with the worker's `[ft-metrics]` line via
/// `connection_id`. The fields surface the two suspected daemon-side
/// hot spots:
///
/// - `dc_send_ns` — time spent inside `webrtc-rs` `dc.send().await`,
///   the SCTP-encoding hot path and the prime suspect when the daemon
///   pegs a CPU core during a download.
/// - `buffered_*_bytes` — SCTP transmit buffer occupancy at the start
///   of each `dc.send`; if these stay high we are pipelining behind
///   the browser's SCTP receiver and `serve_download` is correctly
///   parked by the file-lane backpressure chain. If they stay near
///   zero while throughput is low, the bottleneck is upstream of the
///   DataChannel (worker / IPC / disk).
/// - `recv_idle_ns` — gap between the previous `dc.send` completing
///   and the next payload arriving on `mpsc::Receiver`. A large idle
///   gap when throughput is low points at upstream starvation, not
///   the SCTP send path.
///
/// Kept as a plain struct alongside [`FILE_TRANSFER_WRITER_QUEUE_CAP`]
/// (rather than under `worker::`) so the daemon module owns it and
/// the unit tests can construct it without pulling in dispatcher
/// state.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DaemonFtWindow {
    pub frames: u32,
    pub bytes: u64,
    pub text_frames: u32,
    pub recv_idle_ns: u64,
    pub dc_send_ns: u64,
    pub buffered_max_bytes: u64,
    pub buffered_sum_bytes: u64,
    pub buffered_samples: u32,
}

impl DaemonFtWindow {
    pub(crate) fn record(
        &mut self,
        bytes: u64,
        is_text: bool,
        recv_idle: Duration,
        dc_send: Duration,
        buffered_before_send: u64,
    ) {
        self.frames = self.frames.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        if is_text {
            self.text_frames = self.text_frames.saturating_add(1);
        }
        self.recv_idle_ns =
            self.recv_idle_ns
                .saturating_add(crate::worker::file_transfer_dispatcher::duration_ns(
                    recv_idle,
                ));
        self.dc_send_ns =
            self.dc_send_ns
                .saturating_add(crate::worker::file_transfer_dispatcher::duration_ns(
                    dc_send,
                ));
        if buffered_before_send > self.buffered_max_bytes {
            self.buffered_max_bytes = buffered_before_send;
        }
        self.buffered_sum_bytes = self.buffered_sum_bytes.saturating_add(buffered_before_send);
        self.buffered_samples = self.buffered_samples.saturating_add(1);
    }

    pub(crate) fn is_full(&self) -> bool {
        self.frames >= crate::worker::file_transfer_dispatcher::FT_METRICS_WINDOW_CHUNKS
    }

    pub(crate) fn flush_line(&self, connection_id: &str) -> Option<String> {
        if self.frames == 0 {
            return None;
        }
        let mbps =
            crate::worker::file_transfer_dispatcher::throughput_mbps(self.bytes, self.dc_send_ns);
        let buffered_avg = if self.buffered_samples > 0 {
            self.buffered_sum_bytes / (self.buffered_samples as u64)
        } else {
            0
        };
        let send_ms = (self.dc_send_ns as f64) / 1_000_000.0;
        let idle_ms = (self.recv_idle_ns as f64) / 1_000_000.0;
        Some(format!(
            "[ft-metrics-daemon] cid={cid} frames={f} text={t} bytes={b} \
             dc_send={sm:.2}ms recv_idle={im:.2}ms buffered_max={bm} \
             buffered_avg={ba} dc_throughput={mbps:.2}MB/s",
            cid = connection_id,
            f = self.frames,
            t = self.text_frames,
            b = self.bytes,
            sm = send_ms,
            im = idle_ms,
            bm = self.buffered_max_bytes,
            ba = buffered_avg,
        ))
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

mod ice;

#[cfg(test)]
use ice::resolve_ice_timeouts;
pub use ice::{
    DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS, DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS,
    build_peer_connection, filter_ice_servers, own_turn_endpoints,
};
// =====================================================================
// Per-connection PC context + registry
// =====================================================================

/// All daemon-side state for one browser connection. Each browser
/// gets exactly one of these; multi-browser concurrency = many
/// `PeerConnectionContext`s sharing the same daemon process.
///
/// `pc` + `signaling_state` are populated on `RequestRemote` / `Offer`,
/// along with (when the offer includes media) `video_track` /
/// `audio_track`, which are fed samples from worker `MediaFrame`s. The
/// `on_data_channel` handler routes browser DC traffic over IPC to the
/// worker (mouse / keyboard / clipboard / file / whiteboard) and stashes
/// the cursor-sync DC in `cursor_data_channel` for worker-side cursor
/// updates to be pushed back to.
pub struct PeerConnectionContext {
    pub connection_id: String,
    pub pc: Arc<RTCPeerConnection>,
    pub signaling_state: Arc<RwLock<SignalingState>>,
    /// Set on the first `Offer` whose SDP carries `m=video`. Driven from
    /// worker-side `MediaFrame`s (`MediaFrameKind::VideoI`/`VideoP`).
    pub video_track: Option<Arc<TrackLocalStaticSample>>,
    /// Set on the first `Offer` whose SDP carries `m=audio`. Same
    /// fill timing as `video_track`.
    pub audio_track: Option<Arc<TrackLocalStaticSample>>,
    /// Set when the browser opens the `cursor_sync_event` DataChannel.
    /// The daemon `on_data_channel` handler writes to this slot, and
    /// worker-side `WorkerToService::CursorData` is routed to
    /// `dc.send(...)` here.
    pub cursor_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    /// Set when the browser opens the `clipboard_event` DataChannel.
    /// Worker-side `WorkerToService::ClipboardRead` is routed to
    /// `dc.send_text(...)` here. Browser→host clipboard writes flow
    /// through the standard router (DC `on_message` →
    /// `ServiceToWorker::ClipboardWrite`).
    pub clipboard_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    /// Set when the browser opens the `file_transfer_event`
    /// DataChannel. Worker-side download chunks + control replies
    /// (received over the **file lane** — see
    /// `desk-ipc-protocol::dual_transport`) to `dc.send_text(...)` /
    /// `dc.send(...)` here. Browser→worker chunks and control
    /// messages do **not** flow through `ServiceToWorker` anymore;
    /// they ride the file lane via
    /// [`crate::daemon::worker_manager::WorkerManager::send_file_to_worker`].
    pub file_transfer_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    /// Sender into the per-connection file-transfer writer task spawned
    /// in [`PcRegistry::create_for_request_remote`]. The task drains
    /// payloads serially and calls `dc.send` / `dc.send_text` on the
    /// matching `file_transfer_data_channel`.
    ///
    /// Bounded at [`FILE_TRANSFER_WRITER_QUEUE_CAP`] = 16 so a slow
    /// browser DC head-of-line blocks the file-lane drain rather than
    /// silently buffering unbounded chunks (the regression that drove
    /// fix-2026-05-05): the daemon-side file-lane drain task awaits
    /// on this `Sender::send().await` when full, which in turn parks
    /// the next worker→daemon file-lane payload, which in turn parks
    /// `dispatcher.emit_binary().await` inside the worker's
    /// `serve_download` loop. End-to-end SCTP backpressure restored.
    /// The task exits naturally when this sender is dropped, which
    /// happens when the registry releases the last `Arc` reference to
    /// this `PeerConnectionContext` (i.e. after
    /// `cleanup_pc → registry.remove`).
    pub file_transfer_writer_tx: mpsc::Sender<FileTransferPayload>,
    /// Pause flag set by [`PcRegistry::pause_all_media`] before a
    /// worker swap. While set, [`write_video_frame`] drops samples so
    /// `webrtc-rs` does not push frames the new encoder hasn't anchored
    /// yet. The first `MediaFrameKind::VideoI` after the pause clears
    /// the flag in-line, giving the browser a clean IDR-aligned
    /// resync. Audio falls under the same flag — the brief silence is
    /// preferable to playing audio against a frozen video frame.
    pub media_paused: Arc<AtomicBool>,
    /// Cached payload from the most recent `handle_offer` for
    /// this connection. After a worker swap [`PcRegistry::resume_active_media`]
    /// re-issues this (plus a `ForceKeyframe`) so the new worker
    /// re-arms its per-`connection_id` encoder without a fresh SDP
    /// round-trip. `None` means the offer hasn't been exchanged yet
    /// (PC up but no media negotiated) — resume is a no-op for those.
    pub cached_start_media: Arc<RwLock<Option<StartMediaPayload>>>,
    /// Per-connection adaptive bitrate-cap state. Shared between the
    /// RTCP feedback task (REMB decisions) and the settings router
    /// (enable/disable edges); see `daemon::bitrate_controller` for
    /// the locking contract. Created enabled; the first `Offer`
    /// applies the browser's `desk_settings.adaptive_bitrate`
    /// preference.
    pub adaptive_bitrate: Arc<crate::daemon::bitrate_controller::AdaptiveBitrateShared>,
}

impl PeerConnectionContext {
    /// Record this connection's latest `StartMediaPayload` and report
    /// whether this was the *first* offer to do so.
    ///
    /// `handle_offer` uses the result to gate worker `StartMedia` to the
    /// first negotiation: a later renegotiation (an ICE-restart re-offer)
    /// overwrites the cached payload — so a future worker-swap
    /// [`PcRegistry::resume_active_media`] re-issues the most recent one —
    /// but returns `false`, telling the caller to skip re-issuing
    /// `StartMedia`. Re-issuing on every offer would make the worker
    /// rebuild its per-connection input handlers and log a duplicate-start
    /// warning (`MediaProducer::start_media` ignores the duplicate).
    ///
    /// The check-and-set is atomic on `cached_start_media`. Concurrent
    /// offers for one connection are additionally serialized by the caller
    /// holding the `PeerConnectionContext` write lock across this call, so
    /// exactly one of them observes `true`.
    pub async fn record_start_media_was_first(&self, payload: StartMediaPayload) -> bool {
        let mut slot = self.cached_start_media.write().await;
        let was_first = slot.is_none();
        *slot = Some(payload);
        was_first
    }
}

/// How a signaling connection was admitted, recorded when its `RequestRemote`
/// is authorized and consulted by the router's first door. Independent of the
/// PC's lifecycle so it survives a `CloseControl` PC teardown (see
/// [`PcRegistry::admissions`]).
#[derive(Debug, Clone)]
pub enum Admission {
    /// An owner / full session — no capability ceiling.
    OwnerFull,
    /// A redeemed-grant or legacy-support session, capped by this ceiling.
    Capped(SecuritySettings),
}

/// Daemon-wide registry of active per-browser
/// `PeerConnectionContext`s, indexed by `connection_id`. Equivalent
/// to the `DeskSession::rtc_peer_connection_map` the worker process
/// used to hold, but lives in the daemon process so it survives every
/// worker swap.
///
/// The registry also holds an optional [`WorkerManager`] handle used
/// by [`spawn_file_transfer_writer_task`] to push
/// [`ServiceToWorker::FileTransferSendFailed`] back to the worker when
/// `dc.send` fails. Stored as `OnceCell<WorkerManager>` (set once by
/// the daemon entry point right after `WorkerManager::new`) rather
/// than threaded through every [`Self::create_for_request_remote`]
/// call: `WorkerManager` holds the same registry as a clonable
/// `PcRegistry`, so passing it by argument would re-introduce the
/// constructor-time cycle that the runtime-injection design was
/// chosen to break. Tests that never set the handle keep the legacy
/// "log + drop" behaviour.
/// One entry of the grant-session reverse index: the generation the grant was
/// minted at plus every live connection sharing it. All connections of one grant
/// share the same generation (the central stamps it per RequestRemote); a directed
/// teardown closes them together.
#[derive(Debug, Default)]
struct GrantSessionEntry {
    generation: i64,
    connections: HashSet<String>,
}

#[derive(Clone, Default)]
pub struct PcRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<RwLock<PeerConnectionContext>>>>>,
    worker_mgr: Arc<tokio::sync::OnceCell<WorkerManager>>,
    host_activity: Arc<tokio::sync::OnceCell<crate::host_activity::HostActivityRegistry>>,
    /// Counts in-flight `RequestRemote` handlers that have not yet
    /// registered a [`PeerConnectionContext`]. Used by
    /// [`crate::daemon::pc_manager::cleanup_pc`] to suppress N→0
    /// virtual-display detach while a new browser is mid-`ensure_attached`
    /// but hasn't called [`Self::create_for_request_remote`] yet. The
    /// counter is bumped via [`Self::enter_pending`] which returns a
    /// RAII guard that decrements on drop (panics / early returns are
    /// covered).
    pending_requests: Arc<AtomicUsize>,
    /// External `host:port` endpoints of this node's own bundled TURN server,
    /// frozen at daemon startup from the live `TurnApiState` (empty when no
    /// embedded TURN runs). [`filter_ice_servers`] drops relay candidates that
    /// point back at these so the node never relays through itself. Shared via
    /// `Arc` so registry clones stay cheap and consistent.
    own_turn_endpoints: Arc<HashSet<String>>,
    /// Reverse index `grant_session_id -> {generation, connection_ids}` for every
    /// connection admitted under a redeemed grant (its `RequestRemoteAuthz` stamp
    /// carried a `grant_session_id`). Lets the daemon target a whole logical grant
    /// session for directed teardown / revocation — closing every connection that
    /// shares one grant in a single sweep — instead of the coarse restricted-set.
    /// The recorded generation lets a dial-code regeneration close every grant
    /// minted at a superseded generation (see [`close_grants_up_to_generation`]).
    /// Owner / unrestricted connections carry no grant and never appear here.
    /// Populated by [`Self::index_grant_connection`] in [`handle_request_remote`],
    /// pruned by [`Self::unindex_grant_connection`] on every [`cleanup_pc`]
    /// teardown. Shared via `Arc` so registry clones stay consistent.
    grant_sessions: Arc<RwLock<HashMap<String, GrantSessionEntry>>>,
    /// Signaling-connection admission classes, keyed by the server-authoritative
    /// `from_connection_id`. Recorded when a connection's `RequestRemote` is
    /// authorized (owner → [`Admission::OwnerFull`]; redeemed grant / legacy
    /// support → [`Admission::Capped`] with the ceiling) and — crucially — kept for
    /// the whole **signaling** connection, i.e. **not** cleared when the PC is torn
    /// down by `CloseControl` / [`cleanup_pc`], only by
    /// [`Self::clear_admission`] on the real `ConnectionRemoved` (or a grant
    /// revoke). This outlives the PC so the router's first door still classifies a
    /// capped connection as capped after it drops its PC — closing the
    /// post-teardown escalation where a capped client sends `CloseControl` then
    /// reuses the same connection id for owner-plane frames. Shared via `Arc` so
    /// registry clones stay consistent.
    admissions: Arc<RwLock<HashMap<String, Admission>>>,
    /// Connection ids that are **terminal** WS connections (a distinct connection
    /// per open terminal, admitted via `StartTerminal` rather than `RequestRemote`
    /// and holding no PC). Tracked so a teardown — the connection's own
    /// `CloseTerminal`, or a grant-directed revocation sweeping [`cleanup_pc`] — can
    /// kill the worker-side shell and clear the connection's ceiling / admission,
    /// which the PC-focused cleanup path would otherwise skip (no PC to close, no
    /// media to stop). Shared via `Arc` so registry clones stay consistent.
    terminal_connections: Arc<RwLock<HashSet<String>>>,
    /// Host-terminated signaling connection ids. A tombstone is installed before
    /// teardown so a concurrent half-built request cannot publish a new PC after
    /// the host has declared the connection dead.
    tombstones: Arc<RwLock<HashMap<String, Instant>>>,
    /// Test-only phantom PC counter added to `len()`. See
    /// [`Self::set_test_len_extra`] — it lets the signaling router unit
    /// tests simulate multi-PC topologies without building real
    /// `PeerConnectionContext` instances.
    #[cfg(test)]
    test_len_extra: Arc<AtomicUsize>,
}

/// Errors produced by [`PcRegistry`] handlers. Worker-side equivalents
/// in `service::signaling` use the broader `DeskError`; the registry
/// re-uses it so callers don't have to bridge two error types.
type RegistryResult<T> = Result<T, DeskError>;

/// RAII guard returned by [`PcRegistry::enter_pending`]. While held
/// the registry's `pending_requests` counter stays incremented;
/// `Drop` decrements it. Guarantees the counter survives panics and
/// early returns inside the `RequestRemote` handler.
pub struct PendingRequestGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PcRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the node's own bundled-TURN endpoints (see
    /// [`PcRegistry::own_turn_endpoints`]). Builder-style so existing
    /// `PcRegistry::new()` call sites stay unchanged; the daemon entry point
    /// chains this once at startup with the set derived from the live
    /// `TurnApiState`.
    pub fn with_own_turn_endpoints(mut self, own_turn_endpoints: Arc<HashSet<String>>) -> Self {
        self.own_turn_endpoints = own_turn_endpoints;
        self
    }

    /// Install the daemon's [`WorkerManager`] so the file-transfer
    /// writer task can push [`ServiceToWorker::FileTransferSendFailed`]
    /// back to the worker on `dc.send` failure. Idempotent — calling
    /// twice is a programmer bug (worker_manager is meant to be
    /// initialised once at daemon startup) and is silently ignored
    /// rather than panicking so a future re-entry from crash recovery
    /// stays safe.
    pub fn set_worker_manager(&self, worker_mgr: WorkerManager) {
        if self.worker_mgr.set(worker_mgr).is_err() {
            log::debug!(
                "[pc_manager] PcRegistry::set_worker_manager called more than once; ignoring"
            );
        }
    }

    pub fn set_host_activity(&self, registry: crate::host_activity::HostActivityRegistry) {
        if self.host_activity.set(registry).is_err() {
            log::debug!("[pc_manager] host activity registry already installed; ignoring");
        }
    }

    fn host_activity(&self) -> Option<crate::host_activity::HostActivityRegistry> {
        self.host_activity.get().cloned()
    }

    pub(crate) fn clear_worker_activity(&self) {
        if let Some(activity) = self.host_activity() {
            activity.clear_worker_owned();
        }
    }

    /// Returns the registered [`WorkerManager`] handle if one was
    /// installed via [`Self::set_worker_manager`]. Tests that never
    /// register one observe `None`, which short-circuits the
    /// reverse-feedback path back to its "log + drop" behaviour.
    pub fn worker_manager(&self) -> Option<WorkerManager> {
        self.worker_mgr.get().cloned()
    }

    pub async fn contains(&self, connection_id: &str) -> bool {
        self.inner.read().await.contains_key(connection_id)
    }

    pub async fn get(&self, connection_id: &str) -> Option<Arc<RwLock<PeerConnectionContext>>> {
        self.inner.read().await.get(connection_id).cloned()
    }

    pub async fn remove(&self, connection_id: &str) -> Option<Arc<RwLock<PeerConnectionContext>>> {
        self.inner.write().await.remove(connection_id)
    }

    pub async fn connection_ids(&self) -> Vec<String> {
        self.inner.read().await.keys().cloned().collect()
    }

    pub async fn all_connection_ids(&self) -> Vec<String> {
        let mut ids = std::collections::BTreeSet::new();
        ids.extend(self.inner.read().await.keys().cloned());
        ids.extend(self.admissions.read().await.keys().cloned());
        ids.extend(self.terminal_connections.read().await.iter().cloned());
        if let Some(activity) = self.host_activity() {
            ids.extend(
                activity
                    .snapshot()
                    .sessions
                    .into_iter()
                    .map(|session| session.connection_id),
            );
        }
        ids.into_iter().collect()
    }

    pub async fn tombstone_connection(&self, connection_id: &str) {
        const TOMBSTONE_TTL: Duration = Duration::from_secs(5 * 60);
        self.tombstones
            .write()
            .await
            .insert(connection_id.to_string(), Instant::now() + TOMBSTONE_TTL);
    }

    pub async fn is_tombstoned(&self, connection_id: &str) -> bool {
        let now = Instant::now();
        let mut tombstones = self.tombstones.write().await;
        tombstones.retain(|_, expires_at| *expires_at > now);
        tombstones.contains_key(connection_id)
    }

    /// Record that `connection_id` was admitted under grant `grant_session_id`.
    /// Idempotent; multiple connections (main / file-transfer / reconnect) of one
    /// grant accumulate under the same key so a directed teardown reaches them all.
    pub(crate) async fn index_grant_connection(
        &self,
        grant_session_id: &str,
        generation: i64,
        connection_id: &str,
    ) {
        let mut map = self.grant_sessions.write().await;
        let entry = map.entry(grant_session_id.to_string()).or_default();
        // All connections of one grant share its generation; record it on first
        // insert (later inserts carry the same value).
        entry.generation = generation;
        entry.connections.insert(connection_id.to_string());
    }

    /// Drop `connection_id` from whatever grant session held it, removing the
    /// grant key entirely once its last connection departs. Called by
    /// [`cleanup_pc`] on every teardown path and by the terminal connection's own
    /// `CloseTerminal` cleanup; a no-op for connections that carry no grant.
    pub(crate) async fn unindex_grant_connection(&self, connection_id: &str) {
        let mut map = self.grant_sessions.write().await;
        map.retain(|_, entry| {
            entry.connections.remove(connection_id);
            !entry.connections.is_empty()
        });
    }

    /// Snapshot the connection ids currently indexed under `grant_session_id`
    /// (empty when the grant is unknown). Used by [`close_grant_session`] to sweep
    /// every connection of a revoked grant.
    pub async fn connections_for_grant(&self, grant_session_id: &str) -> Vec<String> {
        self.grant_sessions
            .read()
            .await
            .get(grant_session_id)
            .map(|entry| entry.connections.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot the grant-session ids whose recorded generation is at or below
    /// `revoked_generation`. Used by [`close_grants_up_to_generation`] to direct-
    /// close every session minted before a dial-code regeneration. Owner sessions
    /// (never indexed) are unaffected.
    async fn grants_up_to_generation(&self, revoked_generation: i64) -> Vec<String> {
        self.grant_sessions
            .read()
            .await
            .iter()
            .filter(|(_, entry)| entry.generation <= revoked_generation)
            .map(|(gsid, _)| gsid.clone())
            .collect()
    }

    /// Record how `connection_id` was admitted (owner vs. capped). Called from
    /// [`handle_request_remote`] when the connection's `RequestRemote` is
    /// authorized. Kept for the whole signaling connection — see
    /// [`Self::admissions`].
    pub async fn record_admission(&self, connection_id: &str, admission: Admission) {
        self.admissions
            .write()
            .await
            .insert(connection_id.to_string(), admission);
    }

    /// The admission class of `connection_id`, if its `RequestRemote` was
    /// authorized on this instance. `None` for a connection that never did an
    /// authorized `RequestRemote` (e.g. a central/owner management-only connection
    /// whose privileged frames are gated by their own source/authz gates).
    pub async fn admission(&self, connection_id: &str) -> Option<Admission> {
        self.admissions.read().await.get(connection_id).cloned()
    }

    /// Drop `connection_id`'s admission record. Called only when the signaling
    /// connection truly ends (`ConnectionRemoved`) or its grant is revoked — never
    /// on a `CloseControl` PC teardown, so a capped connection stays classified as
    /// capped for the life of its signaling connection.
    pub async fn clear_admission(&self, connection_id: &str) {
        self.admissions.write().await.remove(connection_id);
    }

    /// Mark `connection_id` as a terminal WS connection (see
    /// [`Self::terminal_connections`]). Called from `handle_start_terminal_inbound`
    /// once the terminal's admission is recorded.
    pub async fn mark_terminal_connection(&self, connection_id: &str) {
        self.terminal_connections
            .write()
            .await
            .insert(connection_id.to_string());
    }

    /// Whether `connection_id` is a tracked terminal WS connection. Used by
    /// [`cleanup_pc`] to run terminal-specific teardown (kill the shell, clear the
    /// ceiling) that the PC-focused path would otherwise skip.
    pub async fn is_terminal_connection(&self, connection_id: &str) -> bool {
        self.terminal_connections
            .read()
            .await
            .contains(connection_id)
    }

    /// Drop `connection_id` from the terminal set. Called when the terminal is torn
    /// down (its own `CloseTerminal` or a grant-directed sweep).
    pub async fn unmark_terminal_connection(&self, connection_id: &str) {
        self.terminal_connections
            .write()
            .await
            .remove(connection_id);
    }

    /// Whether any registered `PeerConnectionContext` currently has
    /// `signaling_state.accept_control == true`. Used by the daemon's
    /// `update_exclusive_after_control_change` helper to decide
    /// whether the worker should keep the exclusive layer active —
    /// the gate is "any holder", not "all holders", so a second
    /// browser releasing while a first still holds control keeps the
    /// physical displays detached.
    pub async fn any_with_accept_control(&self) -> bool {
        // Snapshot the connection list first so we do not hold the
        // outer read lock while awaiting each per-connection lock.
        let pcs: Vec<Arc<RwLock<PeerConnectionContext>>> = {
            let inner = self.inner.read().await;
            inner.values().cloned().collect()
        };
        for pc in pcs {
            let ctx = pc.read().await;
            if ctx.signaling_state.read().await.accept_control {
                return true;
            }
        }
        false
    }

    pub async fn len(&self) -> usize {
        let real = self.inner.read().await.len();
        #[cfg(test)]
        {
            real + self
                .test_len_extra
                .load(std::sync::atomic::Ordering::Relaxed)
        }
        #[cfg(not(test))]
        {
            real
        }
    }

    /// Test-only knob: simulate additional registered PCs without having
    /// to build real `PeerConnectionContext` instances (which depend on
    /// a fully constructed `RTCPeerConnection`). The router's
    /// `auto_request_rejected_when_multiple_pcs` test uses this to bump
    /// `len()` past 1 without dragging the entire WebRTC stack into the
    /// signaling unit-test fixture.
    #[cfg(test)]
    pub fn set_test_len_extra(&self, extra: usize) {
        self.test_len_extra
            .store(extra, std::sync::atomic::Ordering::Relaxed);
    }

    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Mark the start of a new `RequestRemote` handler that has not yet
    /// inserted into the registry. The returned [`PendingRequestGuard`]
    /// decrements the counter on `Drop`, so RAII covers normal returns,
    /// `?` early-exits, and panics. The counter is read by
    /// [`cleanup_pc`] to gate N→0 virtual-display detach — while at
    /// least one pending request is in flight the supervisor stays
    /// attached even if the live PC count momentarily hits zero.
    pub fn enter_pending(&self) -> PendingRequestGuard {
        self.pending_requests.fetch_add(1, Ordering::SeqCst);
        PendingRequestGuard {
            counter: Arc::clone(&self.pending_requests),
        }
    }

    /// Snapshot of in-flight `RequestRemote` handlers (those holding a
    /// live [`PendingRequestGuard`]).
    pub fn pending_requests(&self) -> usize {
        self.pending_requests.load(Ordering::SeqCst)
    }

    /// Build a new `PeerConnectionContext` for the given browser
    /// `connection_id`. Refuses on duplicate (caller should treat
    /// that as a protocol error from the browser).
    ///
    /// Build steps mirror `service::signaling::DeskSession::init_ptc_peer_connection`:
    ///
    /// 1. `filter_ice_servers` per local traversal / startup mode.
    /// 2. `build_peer_connection` with the daemon defaults.
    /// 3. Insert empty-state `PeerConnectionContext` into the map.
    ///
    /// Init reply (codecs / device list) is intentionally NOT sent
    /// here — that requires `MediaCapabilities` from the worker, which
    /// the caller folds into the Init reply once the worker reports them.
    pub async fn create_for_request_remote(
        &self,
        connection_id: &str,
        request_remote: &RequestRemoteModel,
        local_settings: &Settings,
    ) -> RegistryResult<Arc<RwLock<PeerConnectionContext>>> {
        if self.is_tombstoned(connection_id).await {
            return Err(DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::INVALID_STATE,
                "Connection was terminated by the host",
            )));
        }
        if self.contains(connection_id).await {
            return Err(DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("Peer connection already exists for {connection_id}"),
            )));
        }

        let filtered = filter_ice_servers(
            &request_remote.ice_servers,
            &local_settings.turn_client.traversal_mode,
            &self.own_turn_endpoints,
        );

        let pc = build_peer_connection(filtered.iter().map(Into::into).collect(), local_settings)
            .await?;

        // Per-connection file-transfer writer task. The DC slot lives
        // outside the context Arc so the task can read it directly
        // without a lock dance through the registry on every chunk;
        // when the context drops, the sender drops, the task observes
        // `None` from `recv` and exits.
        let file_transfer_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>> =
            Arc::new(RwLock::new(None));
        let (file_transfer_writer_tx, file_transfer_writer_rx) =
            mpsc::channel::<FileTransferPayload>(FILE_TRANSFER_WRITER_QUEUE_CAP);
        spawn_file_transfer_writer_task(
            connection_id.to_string(),
            file_transfer_writer_rx,
            Arc::clone(&file_transfer_data_channel),
            self.worker_manager(),
        );

        let ctx = Arc::new(RwLock::new(PeerConnectionContext {
            connection_id: connection_id.to_string(),
            pc: Arc::new(pc),
            signaling_state: Arc::new(RwLock::new(SignalingState::default())),
            video_track: None,
            audio_track: None,
            cursor_data_channel: Arc::new(RwLock::new(None)),
            clipboard_data_channel: Arc::new(RwLock::new(None)),
            file_transfer_data_channel,
            file_transfer_writer_tx,
            media_paused: Arc::new(AtomicBool::new(false)),
            cached_start_media: Arc::new(RwLock::new(None)),
            adaptive_bitrate: Arc::new(
                crate::daemon::bitrate_controller::AdaptiveBitrateShared::new(true),
            ),
        }));

        // Keep the tombstone read guard through publication. A force-disconnect
        // takes the write side before cleanup, so either this insert happens
        // first and cleanup removes it, or publication observes the tombstone.
        let tombstones = self.tombstones.read().await;
        if tombstones
            .get(connection_id)
            .is_some_and(|expires_at| *expires_at > Instant::now())
        {
            return Err(DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::INVALID_STATE,
                "Connection was terminated by the host",
            )));
        }
        self.inner
            .write()
            .await
            .insert(connection_id.to_string(), Arc::clone(&ctx));
        drop(tombstones);

        Ok(ctx)
    }

    /// Mark every active PC as paused before a worker swap.
    /// Subsequent `write_video_frame` calls drop frames per PC until the
    /// first `MediaFrameKind::VideoI` after the swap clears the flag in
    /// place. Counterpart to [`Self::resume_active_media`] which re-issues
    /// the cached `StartMediaPayload` to the freshly spawned worker.
    pub async fn pause_all_media(&self) {
        let map = self.inner.read().await;
        for (id, ctx) in map.iter() {
            let ctx = ctx.read().await;
            ctx.media_paused.store(true, Ordering::Relaxed);
            log::debug!("[pc_manager] paused media for {id} (worker swap)");
        }
    }

    /// Re-issue the cached `StartMediaPayload` + a `ForceKeyframe`
    /// to the worker for every PC that already negotiated an offer.
    /// Called by `signaling_proxy` once the new worker reports
    /// `Capabilities` after a desktop / crash swap. PCs without a cached
    /// offer (request_remote arrived but offer didn't yet) are skipped —
    /// the standard `handle_offer` path still owns first-time StartMedia.
    pub async fn resume_active_media(
        &self,
        worker_mgr: &WorkerManager,
        virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    ) {
        let snapshot: Vec<(String, Option<StartMediaPayload>, Option<Admission>)> = {
            let map = self.inner.read().await;
            let admissions = self.admissions.read().await;
            let mut out = Vec::with_capacity(map.len());
            for (id, ctx) in map.iter() {
                let cached = ctx.read().await.cached_start_media.read().await.clone();
                out.push((id.clone(), cached, admissions.get(id).cloned()));
            }
            out
        };
        for (id, payload, admission) in snapshot {
            // Re-register the capability ceiling with the freshly spawned worker
            // before any media / terminal / file frame for this connection. A worker
            // swap (desktop change / UAC / lock screen / crash recovery) starts a
            // worker with an empty `ConnectionCeilingStore`, so without this the
            // worker-side `meet(ceiling, global)` gates for terminal / file-browse /
            // private-screen would fall back to global-only and a capped grant
            // session would silently escalate to owner-level access. Done for every
            // capped connection — including ones with no cached offer, since those
            // still accept worker-bound capability frames. Fail-closed: if the
            // ceiling cannot reach the new worker we tear the connection down rather
            // than resume it uncapped (mirrors the admit path at `handle_request_remote`).
            if let Some(Admission::Capped(ceiling)) = admission.as_ref() {
                if let Err(e) = worker_mgr
                    .send_to_worker(ServiceToWorker::SetConnectionCeiling(
                        desk_ipc_protocol::message::SetConnectionCeilingPayload {
                            connection_id: id.clone(),
                            ceiling: Some(ceiling.clone()),
                        },
                    ))
                    .await
                {
                    log::warn!(
                        "[pc_manager] resume: ceiling re-registration for capped session {id} \
                         failed ({e}); tearing down to avoid running uncapped"
                    );
                    cleanup_pc(
                        self,
                        worker_mgr,
                        virtual_display,
                        &id,
                        "resume: ceiling re-registration failed",
                    )
                    .await;
                    continue;
                }
            }
            let payload = match payload {
                Some(p) => p,
                None => {
                    log::debug!(
                        "[pc_manager] resume: no cached StartMedia for {id} (offer not exchanged \
                         yet) — skipping"
                    );
                    continue;
                }
            };
            log::info!("[pc_manager] resume: re-issuing StartMedia + ForceKeyframe for {id}");
            if let Err(e) = worker_mgr
                .send_to_worker(ServiceToWorker::StartMedia(payload))
                .await
            {
                log::warn!("[pc_manager] resume StartMedia for {id} failed: {e}");
                continue;
            }
            if let Err(e) = worker_mgr
                .send_to_worker(ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                    connection_id: id.clone(),
                }))
                .await
            {
                log::warn!("[pc_manager] resume ForceKeyframe for {id} failed: {e}");
            }
        }
    }

    /// Per-connection encoder reset: tells the worker to drop the existing
    /// encoder pipeline for `connection_id` and start a fresh one using the
    /// cached `StartMediaPayload`, then forces an IDR so the daemon can
    /// resume `write_sample` once the new keyframe arrives.
    ///
    /// Called by `signaling_proxy` when the worker reports
    /// `WorkerToService::Error { code: ERROR_CODE_MEDIA_TRANSPORT_STUCK,
    /// connection_id: Some(..) }` — the I-frame send timed out and the
    /// only safe recovery is a clean restart of that connection's encoder
    /// pipeline. PCs without a cached offer (the error fired before the
    /// first StartMedia ever landed) are a no-op other than the StopMedia
    /// to clear any half-built worker state.
    pub async fn reset_media_for(&self, connection_id: &str, worker_mgr: &WorkerManager) {
        let cached = match self.get(connection_id).await {
            Some(ctx) => ctx.read().await.cached_start_media.read().await.clone(),
            None => {
                log::debug!(
                    "[pc_manager] reset_media_for: unknown connection {connection_id}; ignoring"
                );
                return;
            }
        };

        // Pause this PC's media ingestion until the new IDR clears the flag
        // — same pattern as `pause_all_media` but scoped to one connection.
        if let Some(ctx) = self.get(connection_id).await {
            ctx.read().await.media_paused.store(true, Ordering::Relaxed);
        }

        log::info!(
            "[pc_manager] reset_media_for {connection_id}: issuing StopMedia + StartMedia + \
             ForceKeyframe"
        );
        if let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::StopMedia(StopMediaPayload {
                connection_id: connection_id.to_string(),
            }))
            .await
        {
            log::warn!("[pc_manager] reset_media_for {connection_id}: StopMedia failed: {e}");
            // Continue anyway — StartMedia is the actual recovery action.
        }

        let payload = match cached {
            Some(p) => p,
            None => {
                log::warn!(
                    "[pc_manager] reset_media_for {connection_id}: no cached StartMedia (offer \
                     never landed); leaving connection paused — caller must redo handle_offer"
                );
                return;
            }
        };

        if let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::StartMedia(payload))
            .await
        {
            log::warn!("[pc_manager] reset_media_for {connection_id}: StartMedia failed: {e}");
            return;
        }
        if let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                connection_id: connection_id.to_string(),
            }))
            .await
        {
            log::warn!("[pc_manager] reset_media_for {connection_id}: ForceKeyframe failed: {e}");
        }
    }

    /// Fan out an `UpdateMediaSettings` to every PC that has already
    /// negotiated a `StartMedia` (i.e. has a `cached_start_media`
    /// snapshot). Called by `signaling_router` when the browser sends
    /// `SignalingType::UpdateDeskSettings` so encoder fps / quality
    /// changes flow into the live worker pipeline rather than waiting
    /// for the next StopMedia / StartMedia cycle.
    ///
    /// PCs without a cached payload (offer hasn't landed) are skipped
    /// — `handle_offer` will pick up the new daemon-wide settings on
    /// its first StartMedia anyway. All-`None` payloads short-circuit
    /// without iterating to keep the path quiet for unrelated
    /// `UpdateDeskSettings` messages.
    pub async fn broadcast_media_settings_update(
        &self,
        worker_mgr: &WorkerManager,
        fps: Option<u32>,
        bitrate_kbps: Option<u32>,
        quality: Option<u32>,
        enable_dirty_rect: Option<bool>,
    ) {
        if fps.is_none()
            && bitrate_kbps.is_none()
            && quality.is_none()
            && enable_dirty_rect.is_none()
        {
            return;
        }
        let connection_ids: Vec<String> = {
            let map = self.inner.read().await;
            let mut ids = Vec::with_capacity(map.len());
            for (id, ctx) in map.iter() {
                let ctx = ctx.read().await;
                if ctx.cached_start_media.read().await.is_some() {
                    ids.push(id.clone());
                }
            }
            ids
        };
        for id in connection_ids {
            let payload = UpdateMediaSettingsPayload {
                connection_id: id.clone(),
                fps,
                bitrate_kbps,
                quality,
                enable_dirty_rect,
            };
            if let Err(e) = worker_mgr
                .send_to_worker(ServiceToWorker::UpdateMediaSettings(payload))
                .await
            {
                log::warn!("[pc_manager] broadcast_media_settings_update {id}: send failed: {e}");
            }
        }
    }
}

mod data_channel;

pub use data_channel::register_data_channel_router;
#[cfg(test)]
use data_channel::{DcRoute, classify_dc_label, route_is_permitted, route_to_service_msg};
// =====================================================================
// RTCP reader → ForceKeyframe / bitrate-cap IPC
// =====================================================================

/// Ships a bitrate-cap directive to the worker as
/// `UpdateMediaSettings { bitrate_kbps: Some(_) }` and commits the
/// controller state **only on send success** (two-phase commit — see
/// `daemon::bitrate_controller`). Must be called while holding the
/// connection's `AdaptiveBitrateShared::state` lock so directives
/// reach the FIFO event pipe in decision order; the borrow on `state`
/// enforces that structurally.
pub(crate) async fn send_cap_directive(
    worker_mgr: &WorkerManager,
    connection_id: &str,
    directive: CapDirective,
    state: &mut AdaptiveBitrateState,
) {
    let payload = UpdateMediaSettingsPayload {
        connection_id: connection_id.to_string(),
        fps: None,
        bitrate_kbps: Some(directive.wire_kbps()),
        quality: None,
        enable_dirty_rect: None,
    };
    match worker_mgr
        .send_to_worker(ServiceToWorker::UpdateMediaSettings(payload))
        .await
    {
        Ok(()) => {
            log::debug!("[BitrateCap] {connection_id}: sent {directive:?}");
            state.commit(directive, std::time::Instant::now());
        }
        Err(e) => {
            // No commit: the next REMB re-decides from the unchanged
            // state instead of being suppressed by hysteresis. A send
            // failure means the worker pipe is down (worker swap /
            // shutdown); a restarted worker rebuilds encoders at their
            // initial ceiling, so a lost Clear self-heals.
            log::warn!("[BitrateCap] {connection_id}: failed to send {directive:?}: {e}");
        }
    }
}

/// Spawn a task that reads RTCP feedback off `rtp_sender`:
///
/// - **PLI / FIR** (RFC 4585 §6.3.1 / RFC 5104 §4.3.1.1) — the browser
///   asking for a fresh IDR — are translated into
///   `ServiceToWorker::ForceKeyframe` IPC messages addressed to
///   `connection_id`; the worker's `MediaProducer` flags the next
///   encode pass.
/// - **REMB** (receiver-estimated maximum bitrate, `goog-remb`) feeds
///   the per-connection adaptive bitrate-cap controller; emitted
///   directives ride `UpdateMediaSettings.bitrate_kbps`. Decision and
///   send happen under the state lock (see `send_cap_directive`).
///
/// Exits when `read_rtcp` returns `Err` — that happens on PC close /
/// CloseControl, which is the natural lifetime of the task. A noisy
/// transient read error logs at warn level and continues, because the
/// rtp_sender survives single bad reads (e.g. malformed RTCP packet
/// from a buggy proxy).
fn spawn_rtcp_feedback_task(
    rtp_sender: Arc<RTCRtpSender>,
    connection_id: String,
    worker_mgr: WorkerManager,
    adaptive_bitrate: Arc<AdaptiveBitrateShared>,
) {
    tokio::spawn(async move {
        log::info!("[RtcpReader] {connection_id}: starting");
        loop {
            match rtp_sender.read_rtcp().await {
                Ok((packets, _attrs)) => {
                    for pkt in packets {
                        let any = pkt.as_any();
                        if any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>() {
                            log::debug!(
                                "[RtcpReader] {connection_id}: PLI/FIR received → ForceKeyframe \
                                 IPC"
                            );
                            let msg = ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                                connection_id: connection_id.clone(),
                            });
                            if let Err(e) = worker_mgr.send_to_worker(msg).await {
                                log::warn!(
                                    "[RtcpReader] {connection_id}: ForceKeyframe IPC failed: {e}"
                                );
                            }
                        } else if let Some(remb) =
                            any.downcast_ref::<ReceiverEstimatedMaximumBitrate>()
                        {
                            log::trace!(
                                "[RtcpReader] {connection_id}: REMB estimate {:.0} bps",
                                remb.bitrate
                            );
                            let mut state = adaptive_bitrate.state.lock().await;
                            if let Some(directive) =
                                state.decide_on_remb(std::time::Instant::now(), remb.bitrate as f64)
                            {
                                send_cap_directive(
                                    &worker_mgr,
                                    &connection_id,
                                    directive,
                                    &mut state,
                                )
                                .await;
                            }
                        }
                    }
                }
                Err(e) => {
                    // read_rtcp returns Err on PC close — the only sane
                    // exit. Log at info (not warn) so a normal close
                    // doesn't fill the logs; the message identifies it
                    // as the natural lifetime ending.
                    log::info!("[RtcpReader] {connection_id}: exiting (read_rtcp closed): {e}");
                    break;
                }
            }
        }
    });
}

// =====================================================================
// SignalingType handlers
// =====================================================================

/// Outbound Sender used to ship a serialised SignalingModel back to
/// the signaling server (and thence to the browser). Identical to
/// `signaling_proxy`'s `outbound_tx` — pulled out as a type alias so
/// the handler signatures stay readable.
pub type OutboundSink = broadcast::Sender<String>;

/// Push a successful response back to the signaling server. Errors
/// are logged but not returned because a proxy connection drop is
/// recovery-by-reconnect, not a per-handler failure.
fn send_response<T: serde::Serialize + ?Sized>(
    outbound: &OutboundSink,
    request_id: &str,
    signaling_type: SignalingType,
    to_connection_id: &str,
    data: Option<&T>,
) -> Result<(), DeskError> {
    let model = SignalingModel::success_response(
        request_id,
        signaling_type,
        None,
        Some(to_connection_id.to_string()),
        data,
    )?;
    let text = serde_json::to_string(&model).map_err(|e| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("Failed to encode signaling reply: {e}"),
        ))
    })?;
    if let Err(e) = outbound.send(text) {
        log::warn!("[pc_manager] outbound channel send failed: {e}");
    }
    Ok(())
}

/// Forward locally-gathered ICE candidates back to the browser via the
/// signaling channel. Each host / srflx / relay candidate emitted by
/// libwebrtc is wrapped in a
/// `SignalingType::Canid` message — without this the browser only ever
/// learns about the daemon's transport addresses through peer-reflexive
/// discovery, which only works for single-m-line PCs (DataChannel-only
/// file transfer) and consistently times out for video+audio+DC PCs in
/// 30 s of `checking`. Trickle ICE friendly: each candidate ships
/// independently as a fresh `new_request`.
fn register_local_ice_candidate_forwarder(
    pc: Arc<RTCPeerConnection>,
    outbound: OutboundSink,
    from_connection_id: String,
) {
    pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let outbound = outbound.clone();
        let from_connection_id = from_connection_id.clone();
        Box::pin(async move {
            // None signals end-of-candidates; nothing to ship in that case.
            let Some(candidate) = c else {
                log::debug!(
                    "[pc_manager] ICE gathering complete for {from_connection_id} \
                     (end-of-candidates)"
                );
                return;
            };
            let init = match candidate.to_json() {
                Ok(j) => j,
                Err(e) => {
                    log::warn!(
                        "[pc_manager] candidate.to_json failed for {from_connection_id}: {e}"
                    );
                    return;
                }
            };
            let model = match SignalingModel::new_request(
                SignalingType::Canid,
                Some(from_connection_id.clone()),
                Some(&init),
            ) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!(
                        "[pc_manager] candidate model build failed for {from_connection_id}: {e}"
                    );
                    return;
                }
            };
            match serde_json::to_string(&model) {
                Ok(text) => {
                    log::info!(
                        "[pc_manager] forwarding local ICE candidate for {from_connection_id}: \
                         {}",
                        init.candidate
                    );
                    if let Err(e) = outbound.send(text) {
                        log::warn!(
                            "[pc_manager] outbound send (Canid) failed for \
                             {from_connection_id}: {e}"
                        );
                    }
                }
                Err(e) => {
                    log::warn!(
                        "[pc_manager] candidate JSON encode failed for {from_connection_id}: {e}"
                    );
                }
            }
        })
    }));
}

/// Daemon side of `SignalingType::RequestRemote`. Creates the PC and
/// emits the matching `Init` reply. Mirrors the worker's
/// `init_ptc_peer_connection` minus the preapproved restoration (PC
/// lives in the daemon and never has to be rehydrated across worker
/// swaps) and minus the device-list enumeration (supplied instead by
/// the worker's `Capabilities` message).
#[allow(
    clippy::too_many_arguments,
    reason = "Daemon-side RequestRemote handler aggregates state from the \
              entire RouterContext; bundling into a struct would force a \
              tighter Arc/RwLock surface than the call sites need."
)]
pub async fn handle_request_remote(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    settings: &Settings,
    user_name: &str,
    has_tauri: bool,
    capabilities: Option<&MediaCapabilities>,
    worker_mgr: Option<&WorkerManager>,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    model: &SignalingModel,
    // The validated capability ceiling: unwrapped from the `RequestRemoteAuthz`
    // stamp for a redeemed grant (a temporary-support session is one such grant),
    // or `None` for an owner / unrestricted connection. Stored on the connection's
    // `SignalingState` and registered with the worker so the `meet(ceiling,
    // global)` gates enforce it.
    access_ceiling: Option<SecuritySettings>,
    // The grant logical-session id this connection belongs to (`None` when there
    // is no grant). Indexes the connection for grant-directed teardown.
    grant_session_id: Option<String>,
    // The device generation this grant was minted at (stamped by the central).
    // Recorded with the grant so a dial-code regeneration can direct-close every
    // session at a superseded generation. Ignored when there is no grant.
    grant_generation: i64,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let request_remote = model.get_data::<RequestRemoteModel>()?;

    // Register the validated ceiling with the worker's per-connection ceiling map
    // ahead of any worker-bound frame for this connection, so the worker-side
    // `meet(ceiling, global)` gates enforce it from the first file-list / terminal
    // / media request (the never-drop event pipe keeps this FIFO-ordered before
    // them). Only grant-restricted connections carry a ceiling. Fail-closed: if the
    // registration cannot be delivered we abort the whole `RequestRemote` — done
    // *before* creating the PC so a rejected grant leaves no registered connection
    // — rather than let a capped grant session run with no worker-side cap (a
    // delivered media/terminal frame with no ceiling would fall back to global-only
    // gating and over-permit). Owner/unrestricted connections (`ceiling == None`)
    // skip this and leave the worker map empty.
    if let Some(ceiling) = access_ceiling.as_ref() {
        let mgr = worker_mgr.ok_or_else(|| {
            DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "cannot admit grant session {from_connection_id}: no worker to receive its capability ceiling"
                ),
            ))
        })?;
        mgr.send_to_worker(ServiceToWorker::SetConnectionCeiling(
            desk_ipc_protocol::message::SetConnectionCeilingPayload {
                connection_id: from_connection_id.to_string(),
                ceiling: Some(ceiling.clone()),
            },
        ))
        .await
        .map_err(|e| {
            DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "cannot admit grant session {from_connection_id}: ceiling registration failed to reach worker: {e}"
                ),
            ))
        })?;
    }

    let ctx = registry
        .create_for_request_remote(from_connection_id, &request_remote, settings)
        .await?;

    // Record the admission class for the router's first door, keyed by the
    // server-authoritative connection id. Kept for the whole signaling connection
    // (survives a later `CloseControl` PC teardown) so a capped connection can
    // never be reclassified as an unadmitted owner-plane sender.
    registry
        .record_admission(
            from_connection_id,
            match access_ceiling.as_ref() {
                Some(c) => Admission::Capped(c.clone()),
                None => Admission::OwnerFull,
            },
        )
        .await;

    // Stamp the capability ceiling and grant id onto the connection before the ICE
    // / DataChannel handlers below and before the Init reply, so the worker-side
    // `meet(ceiling, global)` gates and grant-directed teardown observe them from
    // the connection's very first frame.
    {
        let ctx_guard = ctx.read().await;
        let mut st = ctx_guard.signaling_state.write().await;
        st.purpose = request_remote.purpose;
        st.access_ceiling = access_ceiling;
        st.grant_session_id = grant_session_id.clone();
    }
    if let Some(gsid) = grant_session_id.as_deref() {
        // Index the connection under its grant so a directed revocation / teardown
        // can reach every connection that shares the grant in one sweep.
        registry
            .index_grant_connection(gsid, grant_generation, from_connection_id)
            .await;
    }

    // Forward locally-gathered ICE candidates back to the browser. Must
    // happen before the Offer arrives (and definitely before
    // `set_local_description` triggers gathering) so that no host / srflx
    // candidate is silently dropped during the handshake window.
    {
        let ctx_guard = ctx.read().await;
        register_local_ice_candidate_forwarder(
            Arc::clone(&ctx_guard.pc),
            outbound.clone(),
            from_connection_id.to_string(),
        );
    }

    // Install the daemon-side `on_data_channel` router on the
    // freshly-created PC. Done before the Offer arrives so any
    // DataChannel the browser opens during SDP setup has its handlers
    // attached on first onopen / onmessage. `worker_mgr` is `Option`
    // so unit-test paths that only exercise SDP / ICE handlers do not
    // have to construct a WorkerManager.
    if let Some(mgr) = worker_mgr {
        let ctx_guard = ctx.read().await;
        register_data_channel_router(
            Arc::clone(&ctx_guard.pc),
            from_connection_id.to_string(),
            Arc::clone(&ctx_guard.signaling_state),
            Arc::clone(&ctx_guard.cursor_data_channel),
            Arc::clone(&ctx_guard.clipboard_data_channel),
            Arc::clone(&ctx_guard.file_transfer_data_channel),
            mgr.clone(),
        );
        // Cleanup hook: when ICE detects the browser is gone (Failed) or
        // the PC is explicitly closed, drop the registry entry and tell
        // the worker to release its per-connection encoder + DXGI /
        // WASAPI capture. Without this the worker keeps DuplicateOutput
        // held and the next remote-desktop attempt hits 0x80070057 from
        // a second concurrent DuplicateOutput on the same monitor.
        register_peer_connection_state_cleanup(
            Arc::clone(&ctx_guard.pc),
            registry.clone(),
            mgr.clone(),
            virtual_display.cloned(),
            from_connection_id.to_string(),
        );
    }

    // Populate the Init reply from the worker's
    // `WorkerToService::Capabilities` snapshot when available; fall
    // back to capture-engine's static factory enumerations for the
    // codec lists when the worker hasn't reported yet (first-Init
    // race window). The fallback path leaves device lists empty
    // because device enumeration requires a live capture stack on
    // the worker's desktop — the daemon (running as SYSTEM in
    // ServiceDaemon mode) cannot produce a meaningful list itself.
    let (
        audio_encoder_list,
        video_encoder_list,
        audio_device_list,
        video_device_list,
        is_admin_value,
    ) = if let Some(caps) = capabilities {
        // Prefer the verbatim encoder identifiers reported by the worker
        // so the UI sees the X264 (libx264) vs H264 (OpenH264)
        // distinction; collapsing them through `media_codec_to_str` would
        // produce two indistinguishable "H264" entries. Fall back to the
        // codec-derived list only when the worker predates this field
        // (empty default on the wire).
        let video_encoder_list = if caps.video_encoders.is_empty() {
            caps.video_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>()
        } else {
            caps.video_encoders.clone()
        };
        let audio_encoder_list = if caps.audio_encoders.is_empty() {
            caps.audio_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>()
        } else {
            caps.audio_encoders.clone()
        };
        (
            audio_encoder_list,
            video_encoder_list,
            caps.audio_device_list.clone(),
            caps.video_device_list.clone(),
            caps.is_admin,
        )
    } else {
        (
            list_audio_encoder(),
            list_video_encoder(),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
            desk_utils::permission::is_admin(),
        )
    };
    // Adaptive-resolution metadata. The browser hook uses
    // `virtual_display_active` to decide whether to start its
    // ResizeObserver loop at all, `virtual_display_device_name` to
    // confirm the captured monitor is in fact the IDD (otherwise
    // resizing the browser would silently change the virtual display
    // resolution while WGC keeps capturing a physical screen), and
    // `adaptive_resolution` to drive the trailing-edge debounce /
    // min-delta thresholds without needing a separate REST round-trip.
    // `virtual_display_current_refresh_hz` is informational — the auto
    // path always sends `refresh_hz: 0` and the daemon substitutes the
    // cached refresh on the way out.
    let (virtual_display_active, virtual_display_current_refresh_hz, virtual_display_device_name) =
        match virtual_display {
            Some(s) => (
                s.is_active().await,
                s.last_refresh_hz(),
                s.attached_display_name().await,
            ),
            None => (false, 0, None),
        };
    let adaptive_resolution = desk_signal_facade::model::signal::AdaptiveResolutionParams {
        debounce_ms: settings.virtual_display.adaptive_debounce_ms,
        min_delta_px: settings.virtual_display.adaptive_min_delta_px,
    };
    let init_data = InitSignalingData {
        ice_servers: vec![],
        user_name: user_name.to_string(),
        audio_device_list,
        audio_encoder_list,
        video_device_list,
        video_encoder_list,
        desk_settings: settings.desk.clone(),
        has_tauri,
        is_admin: is_admin_value,
        virtual_display_active,
        virtual_display_current_refresh_hz,
        virtual_display_device_name,
        adaptive_resolution,
        // The daemon/server process runs on the host, so the compile-time OS
        // is the host's OS. The browser uses this to tailor host-targeted UI.
        operation_system: desk_signal_facade::model::os::OperationSystemEnum::default(),
    };
    log::info!(
        "[pc_manager] Sending Init reply for {from_connection_id} \
         (capabilities={})",
        if capabilities.is_some() {
            "from-worker"
        } else {
            "fallback"
        }
    );
    send_response(
        outbound,
        &model.request_id,
        SignalingType::Init,
        from_connection_id,
        Some(&init_data),
    )
}

/// Inverse of the worker-side codec mapping. Used by the Init reply
/// path so the daemon's `audio_encoder_list` / `video_encoder_list`
/// payloads carry the same string identifiers the legacy worker did.
fn media_codec_to_str(c: &MediaCodec) -> Option<String> {
    match c {
        MediaCodec::H264 => Some("H264".to_string()),
        MediaCodec::Vp8 => Some("VP8".to_string()),
        MediaCodec::Vp9 => Some("VP9".to_string()),
        MediaCodec::Av1 => Some("AV1".to_string()),
        MediaCodec::Opus => Some("OPUS".to_string()),
    }
}

/// Map the offer's `desk_settings.video_encoder` string to the IPC
/// `MediaCodec`. Used by `handle_offer` to compose `StartMediaPayload`.
/// Map the browser-supplied `DeskSettings.video_device_name` to the
/// `StartMediaPayload.video_device` Option carried over IPC. Empty
/// string means "no display selected yet" — the daemon passes `None`
/// so the worker's `payload_overrides` leaves its base
/// `video_device_name` untouched (which the capture-engine then
/// hard-errors on; never falls back to a default monitor). Any
/// non-empty value is propagated verbatim.
pub(crate) fn video_device_for_payload(video_device_name: &str) -> Option<String> {
    if video_device_name.is_empty() {
        None
    } else {
        Some(video_device_name.to_string())
    }
}

fn video_encoder_to_media_codec(t: VideoEncoderType) -> MediaCodec {
    match t {
        VideoEncoderType::H264 | VideoEncoderType::X264 => MediaCodec::H264,
        VideoEncoderType::VP8 => MediaCodec::Vp8,
        VideoEncoderType::VP9 => MediaCodec::Vp9,
        VideoEncoderType::AV1 => MediaCodec::Av1,
    }
}

/// Daemon side of `SignalingType::Offer`. Adds video / audio tracks
/// (when the offer SDP carries the matching m-lines) before running
/// the SDP exchange so the answer comes back with proper media
/// directions; the tracks are then fed from the worker.
pub async fn handle_offer(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    worker_mgr: &WorkerManager,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let offer = model.get_data::<OfferModel>()?;

    let ctx = registry.get(from_connection_id).await.ok_or_else(|| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("No PC for {from_connection_id} (offer arrived before RequestRemote?)"),
        ))
    })?;

    let mut ctx_guard = ctx.write().await;

    {
        let mut s = ctx_guard.signaling_state.write().await;
        s.wayland_control_mode = offer.desk_settings.wayland_control_mode.clone();
    }

    {
        // Apply the browser's adaptive-bitrate preference for this
        // connection before the RTCP reader spawns, so the first REMB
        // decision already sees the right flag. On renegotiation a
        // disable edge with an active cap ships the Clear right here.
        let adaptive = Arc::clone(&ctx_guard.adaptive_bitrate);
        let mut state = adaptive.state.lock().await;
        if let Some(directive) =
            state.set_enabled_and_decide_clear(offer.desk_settings.adaptive_bitrate)
        {
            send_cap_directive(worker_mgr, from_connection_id, directive, &mut state).await;
        }
    }

    let sdp_str = &offer.offer.sdp;
    let has_video = sdp_str.contains("m=video");
    let has_audio = sdp_str.contains("m=audio");
    log::info!(
        "[pc_manager] Offer from {from_connection_id}: has_video={has_video}, has_audio={has_audio}"
    );
    // F3 (observe-only): record the remote SDP's advertised
    // `a=max-message-size` and assert chunk_size + binary-header fits.
    // webrtc-rs 0.17.1 does not expose the negotiated value on
    // `RTCSctpTransport::get_capabilities()` (it currently hard-codes
    // `0`), so we parse the SDP text directly. The check is informational
    // only — a violation logs at `error!` but does NOT block the offer;
    // the actual `dc.send` will surface the failure via
    // F1/F2 (`FileTransferSendErrorKind::PacketTooLarge`) anyway, but
    // having the warning at SDP time means we catch it before the first
    // byte of file data hits the channel.
    log_sdp_max_message_size(from_connection_id, sdp_str);

    // Negotiate the single video codec the host will encode for this
    // connection: intersect the codecs the client advertised it can decode
    // (the offer's `m=video` rtpmap) with the codecs the host can encode,
    // honouring `desk_settings.video_encoder` as a preference hint. This
    // replaces the legacy "trust the client-asserted codec verbatim" path
    // so a client never receives a codec it cannot decode (black screen).
    // Falls back to the configured default only when no codec is shared,
    // which is effectively impossible since VP8 is a universal baseline.
    let preferred_codec = offer
        .desk_settings
        .get_video_encoder_type()
        .ok()
        .map(video_encoder_to_media_codec);
    let negotiated_video_codec = if has_video {
        let client_codecs = codec_negotiation::parse_offer_video_codecs(sdp_str);
        let server_codecs = codec_negotiation::server_encodable_video_codecs();
        match codec_negotiation::negotiate_video_codec(
            &client_codecs,
            &server_codecs,
            preferred_codec,
        ) {
            Some(codec) => {
                log::info!(
                    "[pc_manager] Negotiated video codec {codec:?} for {from_connection_id} \
                     (client={client_codecs:?}, preferred={preferred_codec:?})"
                );
                codec
            }
            None => {
                let fallback = preferred_codec.unwrap_or(MediaCodec::H264);
                log::warn!(
                    "[pc_manager] No video codec shared with {from_connection_id} \
                     (client={client_codecs:?}, server={server_codecs:?}); falling back to \
                     {fallback:?} — the client may be unable to decode"
                );
                fallback
            }
        }
    } else {
        preferred_codec.unwrap_or(MediaCodec::H264)
    };

    if has_video && ctx_guard.video_track.is_none() {
        let video_mime_type = match negotiated_video_codec {
            MediaCodec::H264 => MIME_TYPE_H264,
            MediaCodec::Vp8 => MIME_TYPE_VP8,
            MediaCodec::Vp9 => MIME_TYPE_VP9,
            MediaCodec::Av1 => MIME_TYPE_AV1,
            // Opus is audio-only; the negotiation never yields it for video.
            MediaCodec::Opus => MIME_TYPE_H264,
        };
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: video_mime_type.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        let rtp_sender = ctx_guard
            .pc
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        ctx_guard.video_track = Some(video_track);
        // Spawn the RTCP reader. PLI / FIR from the browser become
        // ForceKeyframe IPC; REMB estimates feed the per-connection
        // adaptive bitrate-cap controller. Reader exits when the
        // rtp_sender is closed (PC drop / CloseControl), see
        // `spawn_rtcp_feedback_task`.
        spawn_rtcp_feedback_task(
            rtp_sender,
            from_connection_id.to_string(),
            worker_mgr.clone(),
            Arc::clone(&ctx_guard.adaptive_bitrate),
        );
    }

    if has_audio && ctx_guard.audio_track.is_none() {
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        let _rtp_sender = ctx_guard
            .pc
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        ctx_guard.audio_track = Some(audio_track);
    }

    ctx_guard.pc.set_remote_description(offer.offer).await?;
    let answer = ctx_guard.pc.create_answer(None).await?;
    ctx_guard.pc.set_local_description(answer).await?;

    if let Some(local_desc) = ctx_guard.pc.local_description().await {
        log::info!("[pc_manager] Sending Answer for {from_connection_id}");
        send_response(
            outbound,
            &model.request_id,
            SignalingType::Answer,
            from_connection_id,
            Some(&local_desc),
        )?;
    }

    // Now that the SDP exchange has populated tracks, tell the worker
    // to start its per-`connection_id` encoder. Without this the daemon
    // would have a video_track that nobody ever feeds. The codec is the
    // one negotiated above (client-decodable ∩ host-encodable) so the
    // worker's encoder and the daemon's track always agree. Audio codec
    // is currently fixed to OPUS.
    let video_codec = negotiated_video_codec;
    // v4 capture-selection fix: thread the browser-chosen GDI device
    // name through to the worker so capture binds to the right
    // monitor. See [`video_device_for_payload`] for the empty-string
    // semantics (legal-but-unselected fresh install case).
    let video_device = video_device_for_payload(&offer.desk_settings.video_device_name);
    let start_media_payload = StartMediaPayload {
        connection_id: from_connection_id.to_string(),
        video_codec,
        audio_codec: MediaCodec::Opus,
        video_device,
        audio_device: None,
        fps: offer.desk_settings.video_fps,
        bitrate_kbps: 0,
        quality: offer.desk_settings.video_quality,
        // Track presence in the offer drives whether the worker spawns
        // each capture pipeline. The browser file-management page
        // negotiates a DataChannel-only PC (no `m=video`, no `m=audio`)
        // and must not trigger DXGI / WASAPI capture — see the worker
        // `start_media` doc comment for the rationale.
        start_video: has_video,
        start_audio: has_audio,
        // Per-connection backend choice — propagating it lets a
        // second browser pick a different backend (e.g. one DXGI +
        // one GDI) without colliding on the first connection's
        // DuplicateOutput. The worker falls back to its own settings
        // when this is `None`.
        image_capture: offer.desk_settings.image_capture.clone(),
        // Thread the Advanced-tab dirty-rect kill-switch from the
        // browser offer through to the worker on the *first*
        // StartMedia. Without this the worker's `merged_settings`
        // would always pick up its base-settings default (`true`),
        // regardless of what the browser actually negotiated, and the
        // toggle would only take effect after a subsequent live
        // `UpdateDeskSettings` round-trip.
        enable_dirty_rect: Some(offer.desk_settings.enable_dirty_rect),
    };
    // Record the payload + decide first-vs-renegotiation while still
    // holding `ctx_guard`, so two concurrent offers for the same
    // connection (an in-flight initial offer racing a frontend
    // ICE-restart re-offer) cannot both observe an empty cache and
    // double-issue StartMedia. Publishing here also keeps the cache
    // ahead of any worker-swap `resume_active_media` that races the swap
    // (it reads the cache under `ctx.read()`).
    let is_first_offer = ctx_guard
        .record_start_media_was_first(start_media_payload.clone())
        .await;
    drop(ctx_guard);
    if has_video && let Some(activity) = registry.host_activity() {
        activity.mark_video_negotiated(from_connection_id);
    }
    // Only the first offer starts the worker's per-connection capture +
    // encode pipeline. A renegotiation (ICE-restart re-offer) finished
    // the SDP exchange above but must not re-issue StartMedia.
    if is_first_offer
        && let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::StartMedia(start_media_payload))
            .await
    {
        log::warn!(
            "[pc_manager] Failed to issue StartMedia to worker for {from_connection_id}: {e} \
             (PC is up but no media will flow until worker comes online)"
        );
    }
    Ok(())
}

/// Daemon side of `SignalingType::Canid` (ICE candidate). Mirrors the
/// worker's mDNS rewrite path for `*.local` hosts.
pub async fn handle_canid(registry: &PcRegistry, model: &SignalingModel) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let ctx = registry.get(from_connection_id).await.ok_or_else(|| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("No PC for {from_connection_id} (Canid before RequestRemote?)"),
        ))
    })?;
    let mut candidate_init = match model.get_data_with_type::<RTCIceCandidateInit>()? {
        Some(c) => c,
        None => return Ok(()),
    };
    log::info!(
        "[pc_manager] ICE candidate for {from_connection_id}: candidate=\"{}\" sdp_mid={:?} \
         sdp_mline_index={:?} ufrag={:?}",
        candidate_init.candidate,
        candidate_init.sdp_mid,
        candidate_init.sdp_mline_index,
        candidate_init.username_fragment,
    );
    if candidate_init.candidate.contains(".local") {
        let mut parts = candidate_init
            .candidate
            .split_whitespace()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        if parts.len() >= 6 {
            let host = parts[4].clone();
            if host.ends_with(".local")
                && let Some(ip) = crate::service::signaling::resolve_mdns_host(&host).await
            {
                log::info!("[pc_manager] Resolved mDNS {host} -> {ip}");
                parts[4] = ip.to_string();
                candidate_init.candidate = parts.join(" ");
            }
        }
    }
    let ctx = ctx.read().await;
    if let Err(e) = ctx.pc.add_ice_candidate(candidate_init).await {
        log::warn!("[pc_manager] add_ice_candidate failed: {e}");
    }
    Ok(())
}

// =====================================================================
// MediaFrame ingestion
// =====================================================================

/// Write one decoded `MediaFrame` to the appropriate per-`connection_id`
/// `TrackLocalStaticSample`. Called from the daemon-side media-pipe
/// receiver task spawned by `worker_manager::run_pipe_server`.
///
/// All errors are intentionally swallowed:
///
/// - **Unknown `connection_id`** — a race against `CloseControl` /
///   browser drop. Logged at trace level so high-rate noise during
///   normal teardown does not flood the operator.
/// - **No `video_track` yet (Audio frame, or video before the first
///   `Offer` arrived)** — same race window; debug-logged and skipped.
/// - **`write_sample` failure** — surfaced as a warning. The sample is
///   dropped; the next IDR will resync. We do not propagate the error
///   because the caller is a long-running receiver loop and there is
///   nothing useful to do at that level besides keep reading frames.
///
/// Video and audio frames are shaped through the same entry point,
/// differing only in which per-connection track they target.
pub async fn write_video_frame(registry: &PcRegistry, frame: MediaFrame) {
    let ctx = match registry.get(&frame.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping frame for unknown connection {}",
                frame.connection_id
            );
            return;
        }
    };

    // Hold the read guard only as long as we need the track Arc + the
    // pause flag; clone them out before awaiting on `write_sample` so
    // the daemon's offer / canid handlers (which take the write lock)
    // are not blocked while the codec write completes.
    let (track_opt, paused) = {
        let g = ctx.read().await;
        let t = match frame.kind {
            MediaFrameKind::VideoI | MediaFrameKind::VideoP => g.video_track.clone(),
            MediaFrameKind::Audio => g.audio_track.clone(),
        };
        (t, g.media_paused.clone())
    };

    // While a worker swap is in progress every frame except the
    // first IDR is dropped. Writing P frames or audio against the
    // browser's existing reference would either decode wrong (P) or
    // play sound against a frozen video frame (audio). The first
    // VideoI clears the flag in place — single store per swap, no
    // central coordinator needed because the same task that observes
    // `paused == true` is the one that flips it back. The flag-flip
    // happens BEFORE the track-presence check so the resume contract
    // (an IDR always re-arms the PC) holds even in the unusual case
    // where the offer hasn't reinstalled the track yet.
    if paused.load(Ordering::Relaxed) {
        match frame.kind {
            MediaFrameKind::VideoI => {
                paused.store(false, Ordering::Relaxed);
                log::info!(
                    "[pc_manager] {} resumed media (first IDR after worker swap)",
                    frame.connection_id
                );
                // fall through to write_sample
            }
            MediaFrameKind::VideoP | MediaFrameKind::Audio => {
                log::trace!(
                    "[pc_manager] dropping {:?} for {} during worker swap (waiting for IDR)",
                    frame.kind,
                    frame.connection_id
                );
                return;
            }
        }
    }

    let track = match track_opt {
        Some(t) => t,
        None => {
            log::debug!(
                "[pc_manager] dropping {:?} frame for {} — no matching track on PC yet \
                 (offer not exchanged?)",
                frame.kind,
                frame.connection_id
            );
            return;
        }
    };

    let sample = Sample {
        data: bytes::Bytes::from(frame.payload),
        duration: Duration::from_nanos(frame.duration_ns),
        ..Default::default()
    };
    if let Err(e) = track.write_sample(&sample).await {
        log::warn!(
            "[pc_manager] write_sample failed for {} ({:?}): {e}",
            frame.connection_id,
            frame.kind
        );
    }
}

/// Write a worker-emitted cursor-sync payload to the matching
/// connection's `cursor_sync_event` DataChannel. The daemon performs the
/// `channel.send_text(json)` based on a `WorkerToService::CursorData` IPC
/// the worker pushes from its capture loop.
///
/// All "channel-not-open" / "connection-unknown" paths are silent:
///
/// - Unknown `connection_id` — race against `CloseControl`; trace-log.
/// - No cursor DataChannel registered yet — browser hasn't opened the
///   `cursor_sync_event` channel for this connection (e.g. control
///   not granted, browser still negotiating). Debug-log + drop.
/// - Channel registered but not in `Open` state — the WebRTC
///   handshake hasn't completed for that DC; debug-log + drop.
/// - Send failed — log warn and continue; the next cursor update will
///   resync the browser without operator intervention.
pub async fn write_cursor_data(registry: &PcRegistry, payload: CursorDataPayload) {
    let ctx = match registry.get(&payload.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping cursor data for unknown connection {}",
                payload.connection_id
            );
            return;
        }
    };
    let dc_opt = {
        let ctx = ctx.read().await;
        ctx.cursor_data_channel.read().await.clone()
    };
    let dc = match dc_opt {
        Some(d) => d,
        None => {
            log::debug!(
                "[pc_manager] dropping cursor data for {} — no cursor_sync DataChannel \
                 registered yet (browser hasn't opened it)",
                payload.connection_id
            );
            return;
        }
    };
    if dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
        log::debug!(
            "[pc_manager] dropping cursor data for {} — DC state is {:?}, not Open",
            payload.connection_id,
            dc.ready_state()
        );
        return;
    }
    // Worker ships JSON bytes (see CursorSyncData serialisation in
    // model::data_channel); the daemon hands them through unchanged.
    // We use `send_text` rather than `send` so the browser receives a
    // text frame matching the legacy wire shape exactly.
    let s = match std::str::from_utf8(&payload.data) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[pc_manager] cursor data for {} not UTF-8: {e}; dropping",
                payload.connection_id
            );
            return;
        }
    };
    if let Err(e) = dc.send_text(s.to_string()).await {
        log::warn!(
            "[pc_manager] failed to send cursor data for {}: {e}",
            payload.connection_id
        );
    }
}

/// Write a worker-emitted clipboard payload (text or chunked image —
/// already JSON-encoded as `ClipboardEventData`) to the matching
/// connection's `clipboard_event` DataChannel. The daemon writes the
/// JSON unchanged so the browser sees the same wire shape the worker's
/// `service::clipboard_event::handle_clipboard_event` polling task
/// used to emit.
///
/// Permission gating is applied here (not the worker): the worker
/// emits unconditionally for every active connection so it does not
/// have to track per-connection accept state, and the daemon drops the
/// IPC if `accept_control && accept_clipboard_sync` is not set on
/// the matching `SignalingState`. This keeps the trust boundary on the
/// daemon side, same as the browser→worker direction in
/// `register_data_channel_router`.
///
/// Silent-drop branches:
///
/// - Unknown `connection_id` — race against `CloseControl`; trace-log.
/// - Permission not granted — `accept_clipboard_sync` is false; debug-log.
/// - No clipboard DataChannel registered yet — browser hasn't opened
///   the `clipboard_event` channel; debug-log.
/// - Channel registered but not in `Open` state — debug-log.
/// - Non-UTF-8 bytes — warn + drop (worker should always serialise
///   `ClipboardEventData` as JSON; this defends against a
///   mismatched-version worker).
/// - Send failed — warn-log; the next clipboard change will resync
///   the browser without operator intervention.
pub async fn write_clipboard_data(registry: &PcRegistry, payload: ClipboardPayload) {
    let ctx = match registry.get(&payload.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping clipboard data for unknown connection {}",
                payload.connection_id
            );
            return;
        }
    };
    let (dc_opt, accepted) = {
        let ctx = ctx.read().await;
        let dc = ctx.clipboard_data_channel.read().await.clone();
        let s = ctx.signaling_state.read().await;
        (dc, s.accept_control && s.accept_clipboard_sync)
    };
    if !accepted {
        log::debug!(
            "[pc_manager] dropping clipboard data for {} — permission not granted",
            payload.connection_id
        );
        return;
    }
    let dc = match dc_opt {
        Some(d) => d,
        None => {
            log::debug!(
                "[pc_manager] dropping clipboard data for {} — no clipboard DataChannel \
                 registered yet (browser hasn't opened it)",
                payload.connection_id
            );
            return;
        }
    };
    if dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
        log::debug!(
            "[pc_manager] dropping clipboard data for {} — DC state is {:?}, not Open",
            payload.connection_id,
            dc.ready_state()
        );
        return;
    }
    let s = match std::str::from_utf8(&payload.data) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[pc_manager] clipboard data for {} not UTF-8: {e}; dropping",
                payload.connection_id
            );
            return;
        }
    };
    if let Err(e) = dc.send_text(s.to_string()).await {
        log::warn!(
            "[pc_manager] failed to send clipboard data for {}: {e}",
            payload.connection_id
        );
    }
}

/// Route a worker-emitted file-transfer payload onto the matching
/// connection's per-connection writer task (queued via
/// `PeerConnectionContext::file_transfer_writer_tx`). The actual
/// `dc.send` / `dc.send_text` runs inside the spawned task — see
/// [`spawn_file_transfer_writer_task`] for the write logic and the
/// silent-drop policy.
///
/// Decoupling the write from this dispatch hop is what keeps the
/// daemon's main IPC loop in
/// `signaling_proxy::run_signaling_proxy` from head-of-line blocking
/// behind a slow / stalled DataChannel: a 989 MB transfer that fills
/// SCTP send buffers no longer delays unrelated typed-IPC traffic
/// (`ManagerFileListResponse`, `Heartbeat`, ...). The dispatch itself
/// is `O(1)` — registry lookup + non-blocking
/// `UnboundedSender::send`.
///
/// Permission gate: file transfer is on its own access category
/// (`security.allow_file_transfer`) — independent from
/// `accept_control` / `accept_clipboard_sync`. The browser
/// file-management UI opens a fresh PC that never requests remote
/// control, so the daemon must forward write-back unconditionally.
/// The actual permission check lives in the worker dispatcher
/// (`worker::file_transfer_dispatcher::permission_for`), which runs
/// `check_security_permission(allow_file_transfer, FileTransfer)`
/// before processing the inbound command. If the worker is satisfied,
/// any reply it emits is by definition authorised — re-checking here
/// against the unrelated `accept_control` flag would silently drop
/// every download (regression fixed 2026-05-05).
///
/// Silent-drop branches at this layer:
///
/// - Unknown `connection_id` — race against `CloseControl`; trace.
/// - Writer task gone (sender disconnected) — debug. Happens during
///   teardown when the context has dropped but a stale payload was
///   already in the daemon's `worker_rx` queue.
pub async fn write_file_transfer_data(registry: &PcRegistry, payload: FileTransferPayload) {
    let ctx_arc = match registry.get(&payload.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping file transfer data for unknown connection {}",
                payload.connection_id
            );
            return;
        }
    };
    // Clone the writer + connection_id under the read guard, then DROP
    // the guard before awaiting `Sender::send`. Holding the read guard
    // across the bounded send would block every other reader of this
    // PeerConnectionContext (clipboard / signaling state / ...) for the
    // entire SCTP-backpressure pause — the daemon's main IPC drain
    // would also park, defeating the lane-separation guarantee.
    let (writer_tx, conn_id) = {
        let ctx = ctx_arc.read().await;
        (
            ctx.file_transfer_writer_tx.clone(),
            ctx.connection_id.clone(),
        )
    };
    if let Err(e) = writer_tx.send(payload).await {
        log::debug!("[pc_manager] file transfer writer task gone for {conn_id}: {e}");
    }
}

/// Spawn the per-connection file-transfer writer. Drains
/// `rx` serially and routes each payload to the matching DataChannel
/// stored in `dc_slot`.
///
/// Lifetime is tied to the sender end inside
/// `PeerConnectionContext::file_transfer_writer_tx`: when that
/// context drops (registry release in `cleanup_pc`), all senders are
/// gone and `rx.recv()` returns `None`, exiting the task.
///
/// When `worker_mgr` is `Some`, a failed `dc.send` is reported back to
/// the worker via [`ServiceToWorker::FileTransferSendFailed`] so the
/// worker dispatcher can abort the matching in-flight transfer and
/// emit a `TransferError` to the browser. The error is also classified
/// ([`FileTransferSendErrorKind`]) so the worker (and the daemon log)
/// can distinguish a configuration bug (`PacketTooLarge`) from normal
/// teardown (`TransportClosed`). When `worker_mgr` is `None`
/// (test-only callers), the failure is logged and dropped so tests
/// don't need to wire a real `WorkerManager`.
///
/// Silent-drop branches inside the task:
///
/// - No file-transfer DC registered — debug (browser hasn't opened it
///   yet, or PC was torn down before the DC frame arrived).
/// - DC not in `Open` state — debug.
/// - send_text on non-UTF-8 bytes — warn + drop. Defends against a
///   buggy worker that sets `is_text=true` on raw chunk bytes.
fn spawn_file_transfer_writer_task(
    connection_id: String,
    mut rx: mpsc::Receiver<FileTransferPayload>,
    dc_slot: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    worker_mgr: Option<WorkerManager>,
) {
    // `tokio::spawn` (not `actix_web::rt::spawn`) is intentional:
    // the task only awaits `mpsc::recv` and `webrtc-rs` futures, both
    // of which are `Send` and need no `LocalSet`. Using
    // `actix_web::rt::spawn` (which is `spawn_local`) would force
    // every `#[tokio::test]` that calls `create_for_request_remote`
    // to wrap itself in a `LocalSet` for the constructor to succeed.
    tokio::spawn(async move {
        let mut window = DaemonFtWindow::default();
        // `last_send_done` anchors the `recv_idle` measurement: time
        // between completing one `dc.send` and pulling the next
        // payload off the bounded queue. A persistently large idle
        // gap during a slow transfer is the smoking gun for an
        // upstream stall (worker / IPC / disk); a near-zero gap with
        // long `dc_send` points the finger at SCTP / webrtc-rs.
        // Initialised to `Instant::now()` so the very first sample's
        // idle gap measures from task start, not from a previous send
        // that never happened.
        let mut last_send_done = std::time::Instant::now();
        while let Some(payload) = rx.recv().await {
            let recv_idle = last_send_done.elapsed();
            let dc_opt = dc_slot.read().await.clone();
            let dc = match dc_opt {
                Some(d) => d,
                None => {
                    log::debug!(
                        "[pc_manager] dropping file transfer data for {connection_id} — no \
                         file_transfer DataChannel registered yet"
                    );
                    continue;
                }
            };
            if dc.ready_state()
                != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            {
                log::debug!(
                    "[pc_manager] dropping file transfer data for {connection_id} — DC state \
                     is {:?}, not Open",
                    dc.ready_state()
                );
                continue;
            }
            // Sample the SCTP transmit buffer occupancy BEFORE the
            // send so the window's `buffered_max` / `buffered_avg`
            // reflect what we hand off to webrtc-rs (post-send the
            // number can momentarily drop as bytes get flushed onto
            // the wire, which would mask sustained occupancy).
            let buffered_before = dc.buffered_amount().await as u64;
            let payload_len = payload.data.len() as u64;
            let is_text = payload.is_text;
            let payload_transfer_id = payload.transfer_id.clone();
            let send_start = std::time::Instant::now();
            let result = if is_text {
                let s = match std::str::from_utf8(&payload.data) {
                    Ok(s) => s.to_string(),
                    Err(e) => {
                        log::warn!(
                            "[pc_manager] file transfer text for {connection_id} not UTF-8: \
                             {e}; dropping"
                        );
                        continue;
                    }
                };
                dc.send_text(s).await
            } else {
                dc.send(&bytes::Bytes::from(payload.data)).await
            };
            let dc_send_elapsed = send_start.elapsed();
            last_send_done = std::time::Instant::now();
            if let Err(e) = result {
                let kind = classify_dc_send_error(&e);
                match kind {
                    FileTransferSendErrorKind::PacketTooLarge => {
                        // Configuration bug: the chosen chunk_size +
                        // binary-header exceeds the remote SDP's
                        // a=max-message-size. The whole transfer is
                        // doomed (every subsequent chunk trips the same
                        // check) so this is logged at error! and the
                        // worker is told to abort.
                        log::error!(
                            "[pc_manager] {connection_id}: SCTP packet too large \
                             (chunk_size + header > remote max_message_size): {e}"
                        );
                    }
                    FileTransferSendErrorKind::TransportClosed => {
                        // Normal teardown / peer disconnect; the
                        // cleanup_pc path is already on its way.
                        log::debug!("[pc_manager] {connection_id}: DC closed mid-transfer: {e}");
                    }
                    FileTransferSendErrorKind::Other => {
                        log::warn!(
                            "[pc_manager] {connection_id}: file transfer dc.send failed: {e}"
                        );
                    }
                }
                if let Some(mgr) = worker_mgr.as_ref() {
                    let notify =
                        ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
                            connection_id: connection_id.clone(),
                            transfer_id: payload_transfer_id,
                            chunk_index: None,
                            kind,
                            error: e.to_string(),
                        });
                    if let Err(send_err) = mgr.send_to_worker(notify).await {
                        log::debug!(
                            "[pc_manager] {connection_id}: could not deliver \
                             FileTransferSendFailed to worker: {send_err}"
                        );
                    }
                }
                // Still account for the failed send in the window so
                // the next flush surfaces the failure latency.
            }
            window.record(
                payload_len,
                is_text,
                recv_idle,
                dc_send_elapsed,
                buffered_before,
            );
            if window.is_full() {
                if let Some(line) = window.flush_line(&connection_id) {
                    log::info!("{line}");
                }
                window.reset();
            }
        }
        // Trailing flush so the last partial window does not vanish
        // when the sender drops on PC teardown.
        if let Some(line) = window.flush_line(&connection_id) {
            log::info!("{line}");
        }
        log::debug!(
            "[pc_manager] file transfer writer task exited for {connection_id} (sender dropped)"
        );
    });
}

/// Categorise a `webrtc::Error` from `dc.send` / `dc.send_text` into
/// the variants the worker reacts to. The webrtc-rs error chain is
/// `webrtc::Error::Sctp(webrtc_sctp::Error::ErrOutboundPacketTooLarge)`
/// for the "chunk too large" case; rather than reaching into the
/// nested error type (the `Sctp` arm is private to webrtc-rs and
/// could be refactored), match on the rendered `Display` substring.
/// The substring `"OutboundPacketTooLarge"` is stable across
/// webrtc-rs 0.17.x and uniquely identifies the SCTP wire-level
/// rejection that the 256 KiB chunk-size regression hit in
/// 2026-05-11.
fn classify_dc_send_error(err: &webrtc::Error) -> FileTransferSendErrorKind {
    let rendered = err.to_string();
    if rendered.contains("OutboundPacketTooLarge") {
        FileTransferSendErrorKind::PacketTooLarge
    } else if rendered.contains("closed")
        || rendered.contains("Closed")
        || rendered.contains("StreamClosed")
        || rendered.contains("ConnectionClosed")
    {
        FileTransferSendErrorKind::TransportClosed
    } else {
        FileTransferSendErrorKind::Other
    }
}

/// Parse `a=max-message-size:N` out of a remote SDP. Returns `None`
/// when the attribute is absent (some browsers / older versions skip
/// it, in which case the SCTP RFC 8841 default of 65536 applies — but
/// we don't synthesise that here; the caller logs the gap).
///
/// The attribute can appear on the session level or under the
/// `m=application` (DataChannel) media section; either case wins. We
/// match the literal `a=max-message-size:` prefix because there is no
/// other SDP attribute that shares the prefix, and we deliberately
/// don't bring in a full SDP parser for one line — keeping the
/// dependency surface minimal.
fn parse_sdp_max_message_size(sdp: &str) -> Option<u64> {
    const PREFIX: &str = "a=max-message-size:";
    sdp.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed.strip_prefix(PREFIX).and_then(|rest| {
            rest.split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
        })
    })
}

/// Log the offer's `a=max-message-size` advertise and assert that our
/// chosen file-transfer chunk size fits inside it (chunk + 40-byte
/// binary header).
///
/// `info!` on success: useful when correlating production logs
/// against a chunk-size regression — knowing the actual negotiated
/// value retroactively explains a `PacketTooLarge` from
/// [`classify_dc_send_error`].
///
/// `error!` on violation: the SCTP send will reject the very first
/// binary chunk with `ErrOutboundPacketTooLarge`. Surfacing it at
/// offer time gives operators a chance to roll back the chunk-size
/// change before the next download starts, instead of finding out
/// only when the first file fails.
///
/// `warn!` when the attribute is absent: per RFC 8841 §6 the default
/// is 65536 bytes (64 KiB), which is **smaller** than our 240 KiB +
/// 40 B header. A peer that doesn't advertise the attribute is on
/// some old WebRTC stack that probably also doesn't lift the default,
/// so we proactively warn.
fn log_sdp_max_message_size(connection_id: &str, sdp: &str) {
    // Constants from the worker dispatcher reach across the
    // daemon ↔ worker boundary because chunk_size is currently a
    // compile-time constant on the worker side. A future negotiated
    // chunk_size (deferred F3 follow-up) would consult this value to
    // pick the maximum; for now we just check our static choice fits.
    use crate::model::file_transfer::BINARY_HEADER_SIZE;
    use crate::worker::file_transfer_dispatcher::FILE_TRANSFER_CHUNK_SIZE_TX;
    let required = (FILE_TRANSFER_CHUNK_SIZE_TX + BINARY_HEADER_SIZE) as u64;
    match parse_sdp_max_message_size(sdp) {
        Some(advertised) => {
            if advertised < required {
                log::error!(
                    "[pc_manager] {connection_id}: remote SDP advertises \
                     max-message-size={advertised} but our chunk \
                     (FILE_TRANSFER_CHUNK_SIZE_TX={FILE_TRANSFER_CHUNK_SIZE_TX} + \
                     BINARY_HEADER_SIZE={BINARY_HEADER_SIZE} = {required} B) won't fit; \
                     downloads will fail with ErrOutboundPacketTooLarge — \
                     lower FILE_TRANSFER_CHUNK_SIZE_TX or use a browser that advertises a \
                     larger ceiling"
                );
            } else {
                log::info!(
                    "[pc_manager] {connection_id}: remote SDP max-message-size={advertised} \
                     (chunk+header={required})"
                );
            }
        }
        None => {
            log::warn!(
                "[pc_manager] {connection_id}: remote SDP has no a=max-message-size; \
                 falling back to RFC 8841 default 65536 which is below our chunk+header \
                 ({required} B). Downloads to this peer may fail with \
                 ErrOutboundPacketTooLarge"
            );
        }
    }
}

/// Centralised teardown for one browser-side PC. Removes the registry
/// entry (so subsequent ICE / DC events for that connection short-circuit),
/// closes the underlying [`RTCPeerConnection`] (idempotent — safe even if
/// already closed by webrtc-rs internals), and ships `StopMedia` to the
/// worker so its per-connection encoder + DXGI duplication / WASAPI capture
/// release immediately. Used by:
///
/// 1. [`handle_close_control`] — explicit browser CloseControl.
/// 2. The on_peer_connection_state_change hook installed in
///    [`register_peer_connection_state_cleanup`] — fires when ICE
///    detects the browser is gone (Failed/Closed/Disconnected).
///
/// All errors swallowed: a dead worker / already-closed PC are normal
/// teardown paths, not failure modes the caller can recover from.
pub(crate) async fn cleanup_pc(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: &str,
    reason: &str,
) {
    let removed = registry.remove(connection_id).await;
    if let Some(activity) = registry.host_activity() {
        activity.remove_connection(connection_id);
    }
    // Prune the grant reverse-index on every teardown path (idempotent for
    // connections that carry no grant) so a directed teardown can never reach a
    // stale connection id.
    registry.unindex_grant_connection(connection_id).await;
    if let Some(ctx) = &removed {
        let ctx = ctx.read().await;
        if let Err(e) = ctx.pc.close().await {
            log::warn!("[pc_manager] PC close failed for {connection_id}: {e}");
        }
        log::info!("[pc_manager] Closed PC for {connection_id} (reason: {reason})");
    } else {
        log::debug!("[pc_manager] cleanup_pc({connection_id}, {reason}): registry already empty");
    }

    if let Err(e) = worker_mgr
        .send_to_worker(ServiceToWorker::StopMedia(
            desk_ipc_protocol::message::StopMediaPayload {
                connection_id: connection_id.to_string(),
            },
        ))
        .await
    {
        log::debug!("[pc_manager] StopMedia for {connection_id} could not reach worker: {e}");
    }

    // Terminal WS connections hold no PC and no media, so the steps above are a
    // no-op for them. A directed teardown (grant revoke / dial-code regeneration)
    // sweeping this path must still physically end the terminal: kill the worker
    // shell and clear the connection's ceiling + admission so nothing survives the
    // revocation. Idempotent with the terminal's own `CloseTerminal` cleanup.
    if registry.is_terminal_connection(connection_id).await {
        if let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::CloseTerminalRequest(
                desk_ipc_protocol::message::CloseTerminalPayload {
                    connection_id: connection_id.to_string(),
                },
            ))
            .await
        {
            log::debug!(
                "[pc_manager] CloseTerminalRequest for {connection_id} could not reach worker: {e}"
            );
        }
        if let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::SetConnectionCeiling(
                desk_ipc_protocol::message::SetConnectionCeilingPayload {
                    connection_id: connection_id.to_string(),
                    ceiling: None,
                },
            ))
            .await
        {
            log::debug!(
                "[pc_manager] ceiling clear for terminal {connection_id} could not reach worker: {e}"
            );
        }
        registry.clear_admission(connection_id).await;
        registry.unmark_terminal_connection(connection_id).await;
        log::info!("[pc_manager] terminal connection {connection_id} torn down (reason: {reason})");
    }

    // Codex P1 #1: re-derive the exclusive-mode desired flag on
    // every actual removal — not just the N → 0 case. If the
    // departing PC was the sole `accept_control=true` holder but
    // other view-only PCs remain (registry.len() stays > 0), the
    // old code never recomputed and the supervisor stayed pinned
    // at `desired=true` with no control holder → physical displays
    // left detached. `recompute_desired` is a no-op when no router
    // closure is installed (e.g. tests / in-process mode), and
    // costs only a read lock + one closure call otherwise, so it is
    // safe to run unconditionally on `removed.is_some()`.
    if let Some(supervisor) = virtual_display
        && removed.is_some()
    {
        supervisor.recompute_desired().await;
    }

    // N -> 0 virtual display detach. Three gates, all required:
    //   (1) `removed.is_some()` — only the call that actually pulled
    //       a live PC out triggers detach. Stale `ConnectionRemoved`
    //       fan-outs that arrive after the PC was already cleaned up
    //       (or never existed) MUST NOT trigger a detach, since a
    //       new `RequestRemote` may be mid-`ensure_attached` with no
    //       PC registered yet.
    //   (2) `registry.len() == 0` — no other live browser session
    //       still using the IDD.
    //   (3) `registry.pending_requests() == 0` — no other browser
    //       currently inside the `RequestRemote` handler holding a
    //       `PendingRequestGuard`. Without this gate, a fast browser
    //       open/close racing with a slow new connection's
    //       `ensure_attached` would tear down the IDD while the
    //       new connection is still bringing it up.
    if let Some(supervisor) = virtual_display
        && removed.is_some()
        && registry.len().await == 0
        && registry.pending_requests() == 0
    {
        log::info!("[pc_manager] last PC removed, no pending requests; detaching virtual display");
        if let Err(e) = supervisor.apply(false).await {
            log::warn!("[pc_manager] N->0 virtual display detach failed: {e}");
        }
    }
}

/// Host-initiated teardown has stronger semantics than browser `CloseControl`:
/// it tombstones the signaling id and clears the whole admission footprint.
pub async fn force_disconnect_connection(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: &str,
    reason: &str,
) -> bool {
    let existed = registry
        .all_connection_ids()
        .await
        .iter()
        .any(|candidate| candidate == connection_id);
    registry.tombstone_connection(connection_id).await;
    cleanup_pc(registry, worker_mgr, virtual_display, connection_id, reason).await;
    if let Err(error) = worker_mgr
        .send_to_worker(ServiceToWorker::SetConnectionCeiling(
            desk_ipc_protocol::message::SetConnectionCeilingPayload {
                connection_id: connection_id.to_string(),
                ceiling: None,
            },
        ))
        .await
    {
        log::debug!(
            "[pc_manager] force-disconnect ceiling clear for {connection_id} could not reach worker: {error}"
        );
    }
    registry.clear_admission(connection_id).await;
    registry.unindex_grant_connection(connection_id).await;
    registry.unmark_terminal_connection(connection_id).await;
    existed
}

/// Tear down every connection admitted under grant `grant_session_id` — a
/// grant-directed revocation. Called when a grant is revoked or its logical
/// session ends (e.g. the manager broadcasts a directed teardown after a device
/// dial-code regeneration), so every connection sharing the grant ends
/// physically, not just at the signaling layer. Snapshots the grant's connection
/// ids first (each `cleanup_pc` prunes the reverse-index), then closes each PC. A
/// no-op when the grant has no live connection.
pub async fn close_grant_session(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    grant_session_id: &str,
    reason: &str,
) {
    let ids = registry.connections_for_grant(grant_session_id).await;
    for id in ids {
        cleanup_pc(registry, worker_mgr, virtual_display, &id, reason).await;
    }
}

/// Tear down every grant session whose recorded generation is at or below
/// `revoked_generation` — the directed teardown the manager triggers after a device
/// dial-code regeneration (each superseded grant is closed via
/// [`close_grant_session`], so all of its connections end together). Owner sessions
/// carry no grant and are never indexed, so they are untouched. A no-op when no
/// held grant is at or below the revoked generation.
///
/// Matches on generation alone, not device: this daemon serves a single device (one
/// desk-server = one `client_id`), so every grant it holds targets that one device
/// and the `RevokeAccessGrant` frame is delivered only to this host. If a daemon ever
/// hosted grants for more than one target device, this would need the frame's
/// `target_device` as a second filter dimension (stored per grant) to avoid closing
/// an unrelated device's grant that happens to share a generation number.
pub async fn close_grants_up_to_generation(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    revoked_generation: i64,
    reason: &str,
) {
    let gsids = registry.grants_up_to_generation(revoked_generation).await;
    for gsid in gsids {
        close_grant_session(registry, worker_mgr, virtual_display, &gsid, reason).await;
    }
}

/// Wire the daemon-side cleanup path onto `pc.on_peer_connection_state_change`
/// so a browser disconnect / network drop / explicit close releases the
/// worker's encoder + capture resources promptly.
///
/// Without this hook the worker keeps the per-connection encoder running and
/// the per-output DXGI duplication held; the next browser to connect then
/// hits `DuplicateOutput → 0x80070057 (E_INVALIDARG)` because Windows only
/// allows one duplication per (process, output) pair. Replaces the
/// `peer_state_change_sender → DeskSessionMessage::WebRTCDropped` chain
/// that used to live in `service::signaling::DeskSession::init_ptc_peer_connection`.
///
/// Only `Failed` and `Closed` trigger cleanup. `Disconnected` is transient
/// (a momentary network blip can recover) and webrtc-rs will follow it
/// with `Failed` after its internal disconnected-timeout if the peer
/// stays gone, so reacting to `Disconnected` would tear down working
/// connections during normal jitter.
fn register_peer_connection_state_cleanup(
    pc: Arc<RTCPeerConnection>,
    registry: PcRegistry,
    worker_mgr: WorkerManager,
    virtual_display: Option<Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: String,
) {
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let registry = registry.clone();
        let worker_mgr = worker_mgr.clone();
        let virtual_display = virtual_display.clone();
        let connection_id = connection_id.clone();
        Box::pin(async move {
            match state {
                RTCPeerConnectionState::Connected => {
                    if let Some(activity) = registry.host_activity() {
                        activity.set_pc_connected(&connection_id, true);
                    }
                }
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    log::info!(
                        "[pc_manager] PC for {connection_id} reached terminal state {state:?}; \
                         tearing down daemon-side context + StopMedia to worker"
                    );
                    cleanup_pc(
                        &registry,
                        &worker_mgr,
                        virtual_display.as_ref(),
                        &connection_id,
                        "pc_state_terminal",
                    )
                    .await;
                }
                _ => {}
            }
        })
    }));
}

/// Daemon side of `SignalingType::CloseControl`. Removes the
/// per-connection context, closes the PC, and tells the worker to
/// drop its per-`connection_id` encoder via
/// `ServiceToWorker::StopMedia`. The StopMedia is best-effort — a
/// dead worker will surface an error from `send_to_worker` which we
/// log but don't propagate; the PC is already closed at that point
/// so the daemon-side state is consistent regardless.
pub async fn handle_close_control(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    cleanup_pc(
        registry,
        worker_mgr,
        virtual_display,
        from_connection_id,
        "close_control",
    )
    .await;
    Ok(())
}

/// Daemon side of `SignalingType::ConnectionRemoved`. Sent by the
/// signaling server when a `Browser`-type peer leaves its connection
/// map (typically because the browser tab closed and the WS
/// disconnected). The signal arrives milliseconds after the browser
/// goes away, well before webrtc-rs would notice through ICE consent
/// freshness — so this is the primary cleanup path for the
/// "user closed the tab" case. The matching ICE
/// `disconnected → failed` timeouts (see [`build_peer_connection`]
/// callers) only run when the signaling channel is gone too.
///
/// Idempotent: if no PC exists for `from_connection_id` the call is
/// a logged no-op (e.g. the browser never finished SDP, or another
/// cleanup path already fired). The departed peer's id rides in
/// `from_connection_id`; the data payload is intentionally empty.
pub async fn handle_connection_removed(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    cleanup_pc(
        registry,
        worker_mgr,
        virtual_display,
        from_connection_id,
        "peer_signaling_closed",
    )
    .await;
    // The signaling connection is truly ending (not just a `CloseControl` PC
    // teardown), so drop its admission record. `cleanup_pc` above — shared with the
    // `CloseControl` path — deliberately leaves the admission intact.
    registry.clear_admission(from_connection_id).await;
    Ok(())
}

/// Daemon side of `SignalingType::RequireControl`. Mirrors the
/// worker-side `DeskSession::handle_request_control` but runs against
/// the daemon-held PC. The browser sends this to either
/// (a) request control + clipboard grants (`accept = true`) or (b)
/// release them (`accept = false`); the daemon dispatches to the
/// host-control hub for user approval (subject to settings allow /
/// remember bits), updates the per-connection [`SignalingState`], and
/// emits the matching reply back through the outbound sink:
///
/// - `accept = true` && approved → `AcceptControl`
/// - `accept = true` && denied → `DenyControl` (state stays false)
/// - `accept = false` (release) → `CloseControl` (state goes false)
///
/// The daemon `on_data_channel` router gates each forwarded
/// browser-input event on the resulting `accept_control` /
/// `accept_clipboard_sync` flags, so the worker only ever sees IPC
/// payloads the user has authorised.
pub async fn handle_require_control(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    settings: &SharedSettings,
    host_control_hub: &Arc<HostControlHub>,
    model: &SignalingModel,
) -> Result<ControlOutcome, DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let ctx = registry.get(from_connection_id).await.ok_or_else(|| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!(
                "No PC for {from_connection_id} (RequireControl arrived before RequestRemote?)"
            ),
        ))
    })?;

    let control_data = model.get_data::<SignalRequestControlData>()?;
    log::info!(
        "[pc_manager] {from_connection_id} RequireControl: {:?}",
        control_data
    );

    // Snapshot the pre-decision state for the short-circuit helpers
    // (re-grant of an already-accepted permission must not re-prompt
    // the user). Read lock dropped before the approval await so the
    // signaling-state write below can take exclusive access cleanly.
    let (currently_has_control, currently_has_clipboard) = {
        let ctx = ctx.read().await;
        let s = ctx.signaling_state.read().await;
        (s.accept_control, s.accept_clipboard_sync)
    };

    // Releasing control (accept = false) is never a privileged action and must
    // never prompt the host. The browser sends RequireControl{accept=false} when
    // the user clicks "cancel control"; routing that through the approval path
    // would pop a spurious authorization dialog on the host just as the
    // controller is walking away (and, with allow_remote_control = None, block on
    // the UI-readiness probe). Short-circuit straight to the release reply.
    if !control_data.accept {
        {
            let ctx = ctx.read().await;
            let mut s = ctx.signaling_state.write().await;
            s.accept_control = false;
            s.accept_clipboard_sync = false;
        }
        log::info!("[pc_manager] {from_connection_id}: release (CloseControl)");
        send_response::<()>(
            outbound,
            &model.request_id,
            SignalingType::CloseControl,
            from_connection_id,
            None,
        )?;
        return Ok(ControlOutcome {
            connection_id: from_connection_id.to_string(),
            accept_control: false,
            changed: currently_has_control,
        });
    }

    // From here on the browser is requesting a grant (accept = true). The
    // effective permission is the connection's capability ceiling met with the
    // host global, so a redeemed-grant session can only be tightened relative to
    // the owner's global; an owner session carries no ceiling and uses the global
    // verbatim.
    let access_ceiling = ctx
        .read()
        .await
        .signaling_state
        .read()
        .await
        .access_ceiling
        .clone();
    let allow_control = effective_permission(
        access_ceiling.as_ref(),
        settings.read().await.security.allow_remote_control,
        |c| c.allow_remote_control,
    );
    let allow_clipboard = effective_permission(
        access_ceiling.as_ref(),
        settings.read().await.security.allow_clipboard_sync,
        |c| c.allow_clipboard_sync,
    );

    let control_approved =
        if should_short_circuit_control(control_data.accept, currently_has_control) {
            log::info!(
                "[pc_manager] {from_connection_id}: short-circuit RemoteControl (already accepted)"
            );
            true
        } else {
            check_security_permission(
                settings,
                host_control_hub,
                allow_control,
                SecurityPermissionType::RemoteControl,
                Some(from_connection_id.to_string()),
                // Capped grant / code-session: honor the prompt but never widen the
                // owner's global allow_* from a borrowed session's "remember".
                access_ceiling.is_some(),
            )
            .await
        };

    if !control_approved {
        log::warn!("[pc_manager] {from_connection_id}: RemoteControl denied");
        {
            let ctx = ctx.read().await;
            let mut s = ctx.signaling_state.write().await;
            s.accept_control = false;
            s.accept_clipboard_sync = false;
        }
        send_response::<()>(
            outbound,
            &model.request_id,
            SignalingType::DenyControl,
            from_connection_id,
            None,
        )?;
        // Denial sets accept_control = false; this PC's value changed
        // iff it was previously holding control. Short-circuiting the
        // current value avoids spurious exclusive-mode updates when the
        // user denies a brand-new RequireControl.
        return Ok(ControlOutcome {
            connection_id: from_connection_id.to_string(),
            accept_control: false,
            changed: currently_has_control,
        });
    }

    let clipboard_approved = if !control_data.accept_clipboard_sync {
        false
    } else if should_short_circuit_clipboard(
        control_data.accept_clipboard_sync,
        currently_has_clipboard,
    ) {
        log::info!(
            "[pc_manager] {from_connection_id}: short-circuit ClipboardSync (already accepted)"
        );
        true
    } else {
        check_security_permission(
            settings,
            host_control_hub,
            allow_clipboard,
            SecurityPermissionType::ClipboardSync,
            Some(from_connection_id.to_string()),
            access_ceiling.is_some(),
        )
        .await
    };

    {
        let ctx = ctx.read().await;
        let mut s = ctx.signaling_state.write().await;
        s.accept_control = true;
        s.accept_clipboard_sync = clipboard_approved;
        log::info!(
            "[pc_manager] {from_connection_id}: AcceptControl \
             (accept_control=true, accept_clipboard_sync={clipboard_approved})"
        );
    }

    send_response::<()>(
        outbound,
        &model.request_id,
        SignalingType::AcceptControl,
        from_connection_id,
        None,
    )?;
    Ok(ControlOutcome {
        connection_id: from_connection_id.to_string(),
        accept_control: true,
        changed: !currently_has_control,
    })
}

/// Outcome the router needs to update the exclusive-mode layer. The
/// `changed` flag is true iff `accept_control` actually moved (a
/// re-grant of an already-accepted permission short-circuits in
/// `handle_require_control` but still returns `changed = false`),
/// letting the router skip the exclusive recompute entirely in that
/// common case. `connection_id` is the PC whose state moved; the
/// router does not currently key off it but the field is in place so
/// per-connection logging stays useful.
#[derive(Debug, Clone)]
pub struct ControlOutcome {
    pub connection_id: String,
    pub accept_control: bool,
    pub changed: bool,
}

#[cfg(test)]
mod tests;
