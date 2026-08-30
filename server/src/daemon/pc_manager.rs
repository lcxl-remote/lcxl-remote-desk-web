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
//! (`RequestRemoteAccess` / `Offer` / `Answer` / `IceCandidate` / `CloseRemoteSession`),
//! feeds the worker's media transport into the per-PC tracks it holds,
//! and registers the DataChannel handlers on top.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use desk_signal_facade::model::desk_settings::DeskSettings;
#[cfg(target_os = "linux")]
use desk_signal_facade::model::desk_settings::LinuxInputControlMode;
use desk_signal_facade::model::image_capture::Resolution;
use desk_signal_facade::model::media_capability::{
    AUTO_ENCODER_ORDER, EncoderCompatibility, VideoEncoderId, capabilities_for_encoder_names,
    check_encoder_input,
};
use desk_signal_facade::model::media_pipeline::{MediaPipelinePhase, MediaPipelineStateData};
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{
    OfferModel, RequestRemoteModel, SignalingModel, SignalingState, SignalingType,
};
use desk_utils::error::{CustomDeskError, DeskErrorCode};
#[cfg(target_os = "linux")]
use desk_utils::linux_display::LinuxDisplayServer;
use tokio::sync::{RwLock, broadcast, mpsc};
use webrtc::api::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9,
};
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
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
use crate::daemon::manager_credential_scope::CredentialFingerprint;
use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use crate::host_control::HostControlHub;
use crate::model::data_channel::SignalRequestControlData;
use crate::model::security_approval::{
    SecurityPermissionType, check_security_permission, effective_permission,
};
use crate::model::settings::Settings;
use crate::service::signaling::{should_short_circuit_clipboard, should_short_circuit_control};
use desk_capture_engine::audio_encoder::audio_encoder_factory::list_audio_encoder;
use desk_capture_engine::model::video_encoder::VideoEncoderType;
use desk_capture_engine::video_encoder::video_encoder_factory::list_video_encoder;
use desk_ipc_protocol::message::{
    AudioPipelinePhase, ClipboardPayload, CursorDataPayload, FileTransferPayload,
    FileTransferSendErrorKind, FileTransferSendFailedPayload, ForceKeyframePayload,
    MediaCapabilities, MediaCodec, MediaFrame, MediaFrameKind, MediaSettingsApplyOutcome,
    ServiceToWorker, StartMediaPayload, StopMediaPayload, UpdateMediaSettingsPayload,
};
use desk_signal_facade::model::signal::RemoteAccessInitializedData;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaRestartTrigger {
    TransportStuck,
    UserRetry,
    RenegotiatedSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaRestartStage {
    UnknownConnection,
    StartMedia,
    ForceKeyframe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartOutcome {
    Restarted,
    NoCachedPayload { left_paused: bool },
    Failed { stage: MediaRestartStage },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MediaRetryAdmission {
    Accepted,
    RequiresRenegotiation,
    Duplicate,
    NotRetryable,
    UnknownConnection,
}

pub(crate) fn retry_requires_renegotiation(
    has_video_track: bool,
    cached_start_video: bool,
    state_has_encoder: bool,
) -> bool {
    (!has_video_track && cached_start_video) || !state_has_encoder
}

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
    OwnTurnEndpoints, build_peer_connection, filter_ice_servers, own_turn_endpoints,
};
// =====================================================================
// Per-connection PC context + registry
// =====================================================================

#[derive(Debug, Clone, Default)]
pub struct MediaOutputFenceState {
    pub video_epoch: String,
    pub video_generation: u32,
    pub audio_epoch: String,
    pub audio_generation: u32,
    pub audio_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSlotLifecycle {
    Stable,
    Transitioning,
}

#[derive(Debug, Clone)]
pub struct MediaSlot {
    pub lifecycle: MediaSlotLifecycle,
    pub generation: u32,
    pub pending_generation: Option<u32>,
}

impl Default for MediaSlot {
    fn default() -> Self {
        Self {
            lifecycle: MediaSlotLifecycle::Stable,
            generation: 0,
            pending_generation: None,
        }
    }
}

/// Single writer for controller-owned media state on one logical PC.
/// Async work records a candidate/target generation here, releases the lock,
/// and commits only after a matching worker terminal event returns.
#[derive(Default)]
pub struct PerConnectionMediaCoordinator {
    pub accepted_baseline: Option<desk_signal_facade::model::remote_session::RemoteSessionSettings>,
    pub recovery_start_media: Option<StartMediaPayload>,
    pub video: MediaSlot,
    pub audio: MediaSlot,
    pub adaptive_quality_override: Option<u32>,
    pub current_apply_request_id: Option<String>,
    pub pending_audio_approval_id: Option<String>,
    pub pending_audio_candidate:
        Option<desk_signal_facade::model::remote_session::AudioPipelineSettings>,
    pub actual_video_phase: Option<MediaPipelinePhase>,
    pub actual_audio_phase: Option<desk_ipc_protocol::message::AudioPipelinePhase>,
    /// The terminal currently authorized for the audio slot. A Stop keeps the
    /// same generation, so generation alone cannot distinguish its expected
    /// `Off` from a late `Active` emitted by the cancelled runner.
    pub audio_expected_terminal: Option<desk_ipc_protocol::message::AudioPipelinePhase>,
    /// Desired output state for the current generation. This remains false
    /// after an `Off` terminal so an even later duplicate `Active` cannot
    /// reopen the daemon output fence.
    pub audio_desired_active: bool,
    pub video_terminal_waiter: Option<(
        u32,
        tokio::sync::oneshot::Sender<Result<MediaPipelinePhase, MediaSettingsApplyOutcome>>,
    )>,
    pub audio_terminal_waiter: Option<(
        u32,
        tokio::sync::oneshot::Sender<
            Result<desk_ipc_protocol::message::AudioPipelinePhase, MediaSettingsApplyOutcome>,
        >,
    )>,
    pub closed: bool,
}

/// All daemon-side state for one browser connection. Each browser
/// gets exactly one of these; multi-browser concurrency = many
/// `PeerConnectionContext`s sharing the same daemon process.
///
/// `pc` + `signaling_state` are populated on `RequestRemoteAccess` / `Offer`,
/// along with (when the offer includes media) `video_track` /
/// `audio_track`, which are fed samples from worker `MediaFrame`s. The
/// `on_data_channel` handler routes browser DC traffic over IPC to the
/// worker (mouse / keyboard / clipboard / file / whiteboard) and stashes
/// the cursor-sync DC in `cursor_data_channel` for worker-side cursor
/// updates to be pushed back to.
pub struct PeerConnectionContext {
    pub connection_id: String,
    pub connection_epoch: String,
    pub host_settings: DeskSettings,
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
    /// Most recent valid cursor JSON received from the worker. The media
    /// pipeline can publish its initial cursor before WebRTC finishes opening
    /// `cursor_sync_event`; the channel's on-open handler replays this cache.
    pub latest_cursor_payload: Arc<RwLock<Option<String>>>,
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
    /// Serialized output gate shared with `write_video_frame`. Audio starts
    /// closed and is opened only after a matching active terminal event.
    pub media_output_fence: Arc<RwLock<MediaOutputFenceState>>,
    pub media_coordinator: Arc<tokio::sync::Mutex<PerConnectionMediaCoordinator>>,
    /// Cached payload from the most recent `handle_offer` for
    /// this connection. After a worker swap [`PcRegistry::resume_active_media`]
    /// re-issues this (plus a `ForceKeyframe`) so the new worker
    /// re-arms its per-`connection_id` encoder without a fresh SDP
    /// round-trip. `None` means the offer hasn't been exchanged yet
    /// (PC up but no media negotiated) — resume is a no-op for those.
    pub cached_start_media: Arc<RwLock<Option<StartMediaPayload>>>,
    /// Last worker-reported pipeline state. Retry is admitted only from a
    /// blocked/failed phase; keeping it beside the cached payload makes that
    /// decision connection-scoped and independent of browser claims.
    pub media_pipeline_state: Arc<RwLock<Option<MediaPipelineStateData>>>,
    pub last_media_retry_request_id: Arc<RwLock<Option<String>>>,
    /// Per-connection adaptive bitrate-cap state. Shared between the
    /// RTCP feedback task (REMB decisions) and the settings router
    /// (enable/disable edges); see `daemon::bitrate_controller` for
    /// the locking contract. Created enabled; the first `Offer`
    /// applies the browser's `desk_settings.adaptive_bitrate`
    /// preference.
    pub adaptive_bitrate: Arc<crate::daemon::bitrate_controller::AdaptiveBitrateShared>,
}

impl PeerConnectionContext {
    /// Install the recovery snapshot once. ICE restart/re-offer must not
    /// overwrite the accepted baseline with settings cached by the browser.
    pub async fn install_initial_media(
        &self,
        payload: StartMediaPayload,
        baseline: Option<desk_signal_facade::model::remote_session::RemoteSessionSettings>,
    ) -> bool {
        let mut slot = self.cached_start_media.write().await;
        if slot.is_some() {
            return false;
        }
        *slot = Some(payload.clone());
        let mut coordinator = self.media_coordinator.lock().await;
        coordinator.accepted_baseline = baseline;
        coordinator.recovery_start_media = Some(payload.clone());
        coordinator.video.generation = payload.video_generation;
        coordinator.audio.generation = payload.audio_generation;
        coordinator.audio_desired_active = payload.audio.is_some();
        coordinator.audio_expected_terminal = payload
            .audio
            .as_ref()
            .map(|_| desk_ipc_protocol::message::AudioPipelinePhase::Active);
        true
    }
}

/// How a signaling connection was admitted, recorded when its `RequestRemoteAccess`
/// is authorized and consulted by the router's first door. Independent of the
/// PC's lifecycle so it survives a `CloseRemoteSession` PC teardown (see
/// [`PcRegistry::admissions`]).
#[derive(Debug, Clone)]
pub enum Admission {
    /// An owner / full session — no capability ceiling.
    OwnerFull,
    /// A redeemed-grant or legacy-support session, capped by this ceiling.
    Capped(SecuritySettings),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionOrigin {
    Local,
    RemoteSignal,
    Manager(CredentialFingerprint),
}

#[derive(Debug, Clone)]
pub struct AdmissionRecord {
    pub class: Admission,
    pub origin: AdmissionOrigin,
}

#[derive(Debug, Default)]
struct AdmissionRegistry {
    by_connection: HashMap<String, AdmissionRecord>,
    by_manager_credential: HashMap<CredentialFingerprint, HashSet<String>>,
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
/// share the same generation (the central stamps it per RequestRemoteAccess); a directed
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
    outbound_tx: Arc<tokio::sync::OnceCell<broadcast::Sender<String>>>,
    host_activity: Arc<tokio::sync::OnceCell<crate::host_activity::HostActivityRegistry>>,
    manager_credential_scopes: Arc<
        tokio::sync::OnceCell<
            crate::daemon::manager_credential_scope::ManagerCredentialScopeRegistry,
        >,
    >,
    /// The hub that owns pending approval prompts, so tearing a connection down
    /// can cancel the ones it raised. Weak because the hub outlives the
    /// registry it installed itself into and holding it strongly would make the
    /// pair immortal.
    host_control_hub: Arc<tokio::sync::OnceCell<std::sync::Weak<HostControlHub>>>,
    /// Counts in-flight `RequestRemoteAccess` handlers that have not yet
    /// registered a [`PeerConnectionContext`]. Used by
    /// [`crate::daemon::pc_manager::cleanup_pc`] to suppress N→0
    /// virtual-display detach while a new browser is mid-`ensure_attached`
    /// but hasn't called [`Self::create_for_request_remote`] yet. The
    /// counter is bumped via [`Self::enter_pending`] which returns a
    /// RAII guard that decrements on drop (panics / early returns are
    /// covered).
    pending_requests: Arc<AtomicUsize>,
    /// External `host:port` endpoints of this node's own bundled TURN server,
    /// resolved from the running runtime on each use (empty when no embedded
    /// TURN is serving). [`filter_ice_servers`] drops relay candidates that
    /// point back at these so the node never relays through itself. Reading it
    /// live rather than freezing it at startup is what keeps the filter honest
    /// across a settings change that moves or stops the relay.
    own_turn_endpoints: OwnTurnEndpoints,
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
    /// `from_connection_id`. Recorded when a connection's `RequestRemoteAccess` is
    /// authorized (owner → [`Admission::OwnerFull`]; redeemed grant / legacy
    /// support → [`Admission::Capped`] with the ceiling) and — crucially — kept for
    /// the whole **signaling** connection, i.e. **not** cleared when the PC is torn
    /// down by `CloseRemoteSession` / [`cleanup_pc`], only by
    /// [`Self::clear_admission`] on the real `ConnectionRemoved` (or a grant
    /// revoke). This outlives the PC so the router's first door still classifies a
    /// capped connection as capped after it drops its PC — closing the
    /// post-teardown escalation where a capped client sends `CloseRemoteSession` then
    /// reuses the same connection id for owner-plane frames. Shared via `Arc` so
    /// registry clones stay consistent.
    admissions: Arc<RwLock<AdmissionRegistry>>,
    /// Connection ids that are **terminal** WS connections (a distinct connection
    /// per open terminal, admitted via `StartTerminal` rather than `RequestRemoteAccess`
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
    /// Test-only extra remote-desktop count kept separate from lifecycle len.
    #[cfg(test)]
    test_remote_desktop_extra: Arc<AtomicUsize>,
}

/// Errors produced by [`PcRegistry`] handlers. Worker-side equivalents
/// in `service::signaling` use the broader `DeskError`; the registry
/// re-uses it so callers don't have to bridge two error types.
type RegistryResult<T> = Result<T, DeskError>;

/// RAII guard returned by [`PcRegistry::enter_pending`]. While held
/// the registry's `pending_requests` counter stays incremented;
/// `Drop` decrements it. Guarantees the counter survives panics and
/// early returns inside the `RequestRemoteAccess` handler.
pub struct PendingRequestGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

fn security_permission_is_tighter(previous: Option<bool>, current: Option<bool>) -> bool {
    let strictness = |permission: Option<bool>| match permission {
        Some(false) => 0,
        None => 1,
        Some(true) => 2,
    };
    strictness(current) < strictness(previous)
}

impl PcRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the node's own bundled-TURN endpoint source (see
    /// [`PcRegistry::own_turn_endpoints`]). Builder-style so existing
    /// `PcRegistry::new()` call sites stay unchanged; the daemon entry point
    /// chains this once at startup with a view onto the live runtime.
    pub fn with_own_turn_endpoints(mut self, own_turn_endpoints: OwnTurnEndpoints) -> Self {
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

    /// Enforce hot tightening of the independent system-audio permission.
    /// Every affected output fence is closed before the first worker await;
    /// widening to allow only invalidates old decisions and never starts audio.
    pub fn spawn_system_audio_policy_enforcer(
        &self,
        mut policy_rx: tokio::sync::watch::Receiver<
            desk_signal_facade::model::policy_snapshot::PolicySnapshot,
        >,
        worker_mgr: WorkerManager,
    ) {
        let registry = self.clone();
        tokio::spawn(async move {
            let mut previous_audio = policy_rx
                .borrow()
                .capability(SecurityPermissionType::SystemAudioCapture);
            while policy_rx.changed().await.is_ok() {
                let snapshot = policy_rx.borrow_and_update().clone();
                let current_audio = snapshot.capability(SecurityPermissionType::SystemAudioCapture);
                if current_audio.generation == previous_audio.generation {
                    // Approval timeout, clipboard, terminal, etc. may publish a
                    // new whole-policy snapshot. They must not invalidate a
                    // one-time system-audio approval while this dimension is
                    // still Prompt.
                    continue;
                }
                let prior_audio = previous_audio;
                previous_audio = current_audio;
                let pcs: Vec<_> = registry.inner.read().await.values().cloned().collect();
                let mut revoke = Vec::new();

                // Linearization pass: close every affected fence first.
                for pc in &pcs {
                    let pc_guard = pc.read().await;
                    let ceiling = pc_guard.signaling_state.read().await.access_ceiling.clone();
                    let previous_effective = effective_permission(
                        ceiling.as_ref(),
                        prior_audio.permission,
                        |settings| settings.allow_system_audio_capture,
                    );
                    let current_effective = effective_permission(
                        ceiling.as_ref(),
                        current_audio.permission,
                        |settings| settings.allow_system_audio_capture,
                    );
                    if !security_permission_is_tighter(previous_effective, current_effective) {
                        continue;
                    }
                    let coordinator = pc_guard.media_coordinator.lock().await;
                    let had_audio = coordinator
                        .accepted_baseline
                        .as_ref()
                        .and_then(|settings| settings.audio.as_ref())
                        .is_some()
                        || coordinator.pending_audio_candidate.is_some()
                        || coordinator
                            .recovery_start_media
                            .as_ref()
                            .and_then(|payload| payload.audio.as_ref())
                            .is_some();
                    let generation = coordinator.audio.generation;
                    drop(coordinator);
                    if had_audio {
                        pc_guard.media_output_fence.write().await.audio_open = false;
                        registry.set_system_audio_capture_activity(&pc_guard.connection_id, false);
                        revoke.push((
                            pc_guard.connection_id.clone(),
                            pc_guard.connection_epoch.clone(),
                            generation,
                            Arc::clone(pc),
                        ));
                    }
                }

                let revocation_deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                let mut revocation_waiters = Vec::new();
                let mut recycle_worker = false;
                for (connection_id, connection_epoch, generation, pc) in revoke {
                    let pc_guard = pc.read().await;
                    let mut coordinator = pc_guard.media_coordinator.lock().await;
                    let (terminal_tx, terminal_rx) = tokio::sync::oneshot::channel();
                    coordinator.pending_audio_approval_id = None;
                    coordinator.pending_audio_candidate = None;
                    if let Some(baseline) = coordinator.accepted_baseline.as_mut() {
                        baseline.audio = None;
                    }
                    if let Some(recovery) = coordinator.recovery_start_media.as_mut() {
                        recovery.audio = None;
                    }
                    coordinator.audio.lifecycle = MediaSlotLifecycle::Transitioning;
                    coordinator.audio.pending_generation = Some(generation);
                    coordinator.audio_desired_active = false;
                    coordinator.audio_expected_terminal = Some(AudioPipelinePhase::Off);
                    coordinator.audio_terminal_waiter = Some((generation, terminal_tx));
                    if let Some(cached) = pc_guard.cached_start_media.write().await.as_mut() {
                        cached.audio = None;
                    }
                    drop(coordinator);
                    drop(pc_guard);
                    if let Err(error) = worker_mgr
                        .send_to_connection_worker(
                            &connection_id,
                            ServiceToWorker::ApplyMediaSettings(
                                desk_ipc_protocol::message::ApplyMediaSettingsPayload {
                                    source_request_id: None,
                                    connection_id: connection_id.clone(),
                                    connection_epoch: connection_epoch.clone(),
                                    media_kind: desk_ipc_protocol::message::MediaKind::Audio,
                                    action: desk_ipc_protocol::message::MediaSettingsAction::Stop {
                                        target_generation: generation,
                                    },
                                },
                            ),
                        )
                        .await
                    {
                        log::warn!(
                            "[audio-policy] failed to stop revoked audio for {connection_id}: {error}"
                        );
                        recycle_worker = true;
                    } else {
                        revocation_waiters.push((connection_id, connection_epoch, terminal_rx));
                    }
                }

                for (connection_id, connection_epoch, waiter) in revocation_waiters {
                    let terminal_ok = match tokio::time::timeout_at(revocation_deadline, waiter)
                        .await
                    {
                        Ok(Ok(Ok(desk_ipc_protocol::message::AudioPipelinePhase::Off))) => true,
                        Ok(Ok(Ok(phase))) => {
                            log::warn!(
                                "[audio-policy] revoked audio for {connection_id} reached \
                                 unexpected terminal {phase:?}; recycling worker"
                            );
                            false
                        }
                        Ok(Ok(Err(outcome))) => {
                            log::warn!(
                                "[audio-policy] worker rejected audio revocation for \
                                 {connection_id}: {outcome:?}"
                            );
                            false
                        }
                        Ok(Err(_)) => {
                            log::warn!(
                                "[audio-policy] revoked audio waiter closed for {connection_id}; \
                                 recycling worker"
                            );
                            false
                        }
                        Err(_) => {
                            log::warn!(
                                "[audio-policy] revoked audio for {connection_id} did not stop \
                                 within the 5 second hard deadline; recycling worker"
                            );
                            false
                        }
                    };
                    if !terminal_ok {
                        let still_current = match registry.get(&connection_id).await {
                            Some(pc) => pc.read().await.connection_epoch == connection_epoch,
                            None => false,
                        };
                        if still_current {
                            recycle_worker = true;
                        } else {
                            log::debug!(
                                "[audio-policy] ignoring revocation failure for finalized \
                                 connection {connection_id}"
                            );
                        }
                    }
                }
                if recycle_worker
                    && let Err(error) = worker_mgr.recycle_for_remote_access_timeout().await
                {
                    log::error!(
                        "[audio-policy] failed to recycle worker after revocation timeout: {error}"
                    );
                }
            }
        });
    }

    pub fn set_host_activity(&self, registry: crate::host_activity::HostActivityRegistry) {
        if self.host_activity.set(registry).is_err() {
            log::debug!("[pc_manager] host activity registry already installed; ignoring");
        }
    }

    pub fn set_outbound_tx(&self, outbound: broadcast::Sender<String>) {
        if self.outbound_tx.set(outbound).is_err() {
            log::debug!("[pc_manager] outbound signaling sender already installed; ignoring");
        }
    }

    /// Enter the permanent in-process media fail-safe state. Every current PC is
    /// notified with the dedicated error, its output fences are closed, and the
    /// PC is removed/closed. New remote-desktop admissions are rejected by
    /// `WorkerManager::media_worker_restart_required` until process restart.
    pub async fn fail_media_worker_restart_required(&self) {
        let ids = self.all_connection_ids().await;
        for connection_id in ids {
            let Some(pc) = self.remove(&connection_id).await else {
                continue;
            };
            let pc = pc.read().await;
            pc.media_paused.store(true, Ordering::Release);
            pc.media_output_fence.write().await.audio_open = false;
            let mut coordinator = pc.media_coordinator.lock().await;
            coordinator.closed = true;
            coordinator.current_apply_request_id = None;
            coordinator.pending_audio_approval_id = None;
            coordinator.pending_audio_candidate = None;
            coordinator.video_terminal_waiter = None;
            coordinator.audio_terminal_waiter = None;
            coordinator.audio_expected_terminal = None;
            coordinator.audio_desired_active = false;
            coordinator.actual_audio_phase = Some(AudioPipelinePhase::Failed);
            let accepted_audio = coordinator
                .accepted_baseline
                .as_ref()
                .and_then(|settings| settings.audio.clone());
            drop(coordinator);
            if let Some(activity) = self.host_activity() {
                // This path bypasses `cleanup_pc`, so remove the complete
                // connection footprint (view/control/audio), not just its
                // system-audio bit.
                activity.remove_connection(&connection_id);
            }
            if let Some(hub) = self.host_control_hub() {
                hub.cancel_pending_for_connection(&connection_id);
            }

            if let Some(outbound) = self.outbound_tx.get() {
                let payload = desk_signal_facade::model::remote_session::SystemAudioCaptureStateData {
                    connection_epoch: pc.connection_epoch.clone(),
                    state: desk_signal_facade::model::remote_session::SystemAudioCaptureState::Failed,
                    accepted_audio,
                    resolved_audio_device_id: None,
                    error_code: Some(DeskErrorCode::MEDIA_WORKER_RESTART_REQUIRED),
                };
                if let Ok(model) = SignalingModel::new_request(
                    SignalingType::SystemAudioCaptureStateChanged,
                    Some(connection_id.clone()),
                    Some(&payload),
                ) && let Ok(text) = serde_json::to_string(&model)
                {
                    let _ = outbound.send(text);
                }
            }
            if let Err(error) = pc.pc.close().await {
                log::warn!(
                    "[pc_manager] failed to close {connection_id} after unsafe media-worker stop: {error}"
                );
            }
            drop(pc);
            self.unindex_grant_connection(&connection_id).await;
            self.clear_admission(&connection_id).await;
            log::error!(
                "[pc_manager] closed {connection_id}: media worker requires host process restart"
            );
        }
    }

    pub fn set_manager_credential_scopes(
        &self,
        registry: crate::daemon::manager_credential_scope::ManagerCredentialScopeRegistry,
    ) {
        if self.manager_credential_scopes.set(registry).is_err() {
            log::debug!("[pc_manager] manager credential scope registry already installed");
        }
    }

    pub fn set_host_control_hub(&self, hub: &Arc<HostControlHub>) {
        if self.host_control_hub.set(Arc::downgrade(hub)).is_err() {
            log::debug!("[pc_manager] host control hub already installed; ignoring");
        }
    }

    pub(crate) fn host_control_hub(&self) -> Option<Arc<HostControlHub>> {
        self.host_control_hub.get().and_then(|hub| hub.upgrade())
    }

    fn host_activity(&self) -> Option<crate::host_activity::HostActivityRegistry> {
        self.host_activity.get().cloned()
    }

    pub fn set_system_audio_capture_activity(&self, connection_id: &str, active: bool) {
        if let Some(activity) = self.host_activity() {
            activity.set_system_audio_capture(connection_id, active);
        }
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

    pub async fn record_media_pipeline_state(
        &self,
        connection_id: &str,
        connection_epoch: &str,
        video_generation: u32,
        state: MediaPipelineStateData,
    ) -> bool {
        let Some(ctx) = self.get(connection_id).await else {
            return false;
        };
        let ctx = ctx.read().await;
        if ctx.connection_epoch != connection_epoch {
            return false;
        }
        let mut coordinator = ctx.media_coordinator.lock().await;
        if coordinator.video.generation != video_generation {
            return false;
        }
        coordinator.actual_video_phase = Some(state.phase);
        if matches!(
            state.phase,
            MediaPipelinePhase::Streaming
                | MediaPipelinePhase::Blocked
                | MediaPipelinePhase::Failed
        ) {
            coordinator.video.lifecycle = MediaSlotLifecycle::Stable;
            coordinator.video.pending_generation = None;
        }
        if coordinator
            .video_terminal_waiter
            .as_ref()
            .is_some_and(|(generation, _)| *generation == video_generation)
            && let Some((_, waiter)) = coordinator.video_terminal_waiter.take()
        {
            let _ = waiter.send(Ok(state.phase));
        }
        drop(coordinator);
        *ctx.media_pipeline_state.write().await = Some(state);
        true
    }

    pub(crate) async fn claim_media_pipeline_retry(
        &self,
        connection_id: &str,
        request_id: &str,
    ) -> MediaRetryAdmission {
        let Some(ctx) = self.get(connection_id).await else {
            return MediaRetryAdmission::UnknownConnection;
        };
        let ctx = ctx.read().await;
        let cached_start_video = ctx
            .cached_start_media
            .read()
            .await
            .as_ref()
            .is_some_and(|payload| payload.start_video);
        let has_video_track = ctx.video_track.is_some();
        let mut last_request_id = ctx.last_media_retry_request_id.write().await;
        if last_request_id.as_deref() == Some(request_id) {
            return MediaRetryAdmission::Duplicate;
        }
        let mut state = ctx.media_pipeline_state.write().await;
        if !matches!(
            state.as_ref().map(|state| state.phase),
            Some(MediaPipelinePhase::Blocked | MediaPipelinePhase::Failed)
        ) {
            return MediaRetryAdmission::NotRetryable;
        }
        *last_request_id = Some(request_id.to_string());
        if retry_requires_renegotiation(
            has_video_track,
            cached_start_video,
            state.as_ref().and_then(|state| state.encoder).is_some(),
        ) {
            // Either Auto has no concrete encoder, or the SDP answer was
            // completed without a video sender. Stop+Start cannot add an RTP
            // sender after negotiation; the controller must send a fresh
            // offer instead of receiving a false-success restart.
            return MediaRetryAdmission::RequiresRenegotiation;
        }
        // Reserve the single bounded retry until the worker reports its next
        // Streaming/Blocked/Failed transition.
        *state = None;
        MediaRetryAdmission::Accepted
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
        ids.extend(self.admissions.read().await.by_connection.keys().cloned());
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
    /// grant key entirely once its last logical signaling connection departs.
    /// PeerConnection-only replacement deliberately keeps the index so grant
    /// revocation still reaches the controller during the handoff gap. A no-op
    /// for connections that carry no grant.
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
    /// [`handle_request_remote`] when the connection's `RequestRemoteAccess` is
    /// authorized. Kept for the whole signaling connection — see
    /// [`Self::admissions`].
    pub async fn record_admission(&self, connection_id: &str, admission: Admission) {
        self.record_admission_with_origin(connection_id, admission, AdmissionOrigin::Local)
            .await;
    }

    pub async fn record_admission_with_origin(
        &self,
        connection_id: &str,
        admission: Admission,
        origin: AdmissionOrigin,
    ) {
        let mut registry = self.admissions.write().await;
        if let Some(previous) = registry.by_connection.remove(connection_id)
            && let AdmissionOrigin::Manager(fingerprint) = previous.origin
            && let Some(connections) = registry.by_manager_credential.get_mut(&fingerprint)
        {
            connections.remove(connection_id);
            if connections.is_empty() {
                registry.by_manager_credential.remove(&fingerprint);
            }
        }
        if let AdmissionOrigin::Manager(fingerprint) = &origin {
            registry
                .by_manager_credential
                .entry(fingerprint.clone())
                .or_default()
                .insert(connection_id.to_string());
        }
        registry.by_connection.insert(
            connection_id.to_string(),
            AdmissionRecord {
                class: admission,
                origin,
            },
        );
    }

    /// The admission class of `connection_id`, if its `RequestRemoteAccess` was
    /// authorized on this instance. `None` for a connection that never did an
    /// authorized `RequestRemoteAccess` (e.g. a central/owner management-only connection
    /// whose privileged frames are gated by their own source/authz gates).
    pub async fn admission(&self, connection_id: &str) -> Option<Admission> {
        self.admissions
            .read()
            .await
            .by_connection
            .get(connection_id)
            .map(|record| record.class.clone())
    }

    pub async fn manager_credential_connections(
        &self,
        fingerprint: &CredentialFingerprint,
    ) -> Vec<String> {
        self.admissions
            .read()
            .await
            .by_manager_credential
            .get(fingerprint)
            .map(|connections| connections.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop `connection_id`'s admission record. Called only when the signaling
    /// connection truly ends (`ConnectionRemoved`) or its grant is revoked — never
    /// on a `CloseRemoteSession` PC teardown, so a capped connection stays classified as
    /// capped for the life of its signaling connection.
    pub async fn clear_admission(&self, connection_id: &str) {
        let mut registry = self.admissions.write().await;
        let manager_fingerprint = registry
            .by_connection
            .remove(connection_id)
            .and_then(|record| {
                if let AdmissionOrigin::Manager(fingerprint) = record.origin {
                    if let Some(connections) = registry.by_manager_credential.get_mut(&fingerprint)
                    {
                        connections.remove(connection_id);
                        if connections.is_empty() {
                            registry.by_manager_credential.remove(&fingerprint);
                        }
                    }
                    Some(fingerprint)
                } else {
                    None
                }
            });
        drop(registry);
        if let Some(fingerprint) = manager_fingerprint
            && let Some(scopes) = self.manager_credential_scopes.get()
        {
            scopes.remove_member(&fingerprint, connection_id).await;
        }
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

    pub async fn remote_desktop_count(&self) -> usize {
        let pcs: Vec<_> = self.inner.read().await.values().cloned().collect();
        let mut count = 0usize;
        for pc in pcs {
            if pc.read().await.signaling_state.read().await.purpose
                == desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop
            {
                count += 1;
            }
        }
        #[cfg(test)]
        {
            count += self
                .test_remote_desktop_extra
                .load(std::sync::atomic::Ordering::Relaxed);
        }
        count
    }

    #[cfg(test)]
    pub fn set_test_remote_desktop_extra(&self, extra: usize) {
        self.test_remote_desktop_extra
            .store(extra, std::sync::atomic::Ordering::Relaxed);
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

    /// Mark the start of a new `RequestRemoteAccess` handler that has not yet
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

    /// Snapshot of in-flight `RequestRemoteAccess` handlers (those holding a
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
    /// RemoteAccessInitialized response (codecs / device list) is intentionally NOT sent
    /// here — that requires `MediaCapabilities` from the worker, which
    /// the caller folds into the RemoteAccessInitialized response once the worker reports them.
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
            &self.own_turn_endpoints.current(),
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
            connection_epoch: uuid::Uuid::new_v4().to_string(),
            host_settings: local_settings.desk.clone(),
            pc: Arc::new(pc),
            signaling_state: Arc::new(RwLock::new(SignalingState::default())),
            video_track: None,
            audio_track: None,
            cursor_data_channel: Arc::new(RwLock::new(None)),
            latest_cursor_payload: Arc::new(RwLock::new(None)),
            clipboard_data_channel: Arc::new(RwLock::new(None)),
            file_transfer_data_channel,
            file_transfer_writer_tx,
            media_paused: Arc::new(AtomicBool::new(false)),
            media_output_fence: Arc::new(RwLock::new(MediaOutputFenceState::default())),
            media_coordinator: Arc::new(tokio::sync::Mutex::new(
                PerConnectionMediaCoordinator::default(),
            )),
            cached_start_media: Arc::new(RwLock::new(None)),
            media_pipeline_state: Arc::new(RwLock::new(None)),
            last_media_retry_request_id: Arc::new(RwLock::new(None)),
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

    pub async fn pause_media_for_connections(&self, connection_ids: &[String]) {
        for id in connection_ids {
            let Some(ctx) = self.get(id).await else {
                continue;
            };
            let ctx = ctx.read().await;
            ctx.media_paused.store(true, Ordering::Relaxed);
            log::debug!("[pc_manager] paused media for {id} (interactive route switch)");
        }
    }

    pub async fn connection_epoch(&self, connection_id: &str) -> Option<String> {
        let ctx = self.get(connection_id).await?;
        Some(ctx.read().await.connection_epoch.clone())
    }

    /// Build the authoritative worker-recovery payload with fresh per-media
    /// generations. The accepted recovery snapshot, cache, coordinator slots
    /// and output fence advance together before anything reaches the worker, so
    /// frames or terminal events from the old incarnation can never reopen the
    /// new pipeline.
    async fn prepare_fresh_recovery_payload(
        &self,
        connection_id: &str,
    ) -> Result<Option<StartMediaPayload>, ()> {
        let Some(ctx) = self.get(connection_id).await else {
            return Ok(None);
        };
        let pc = ctx.read().await;
        pc.media_paused.store(true, Ordering::Relaxed);
        let mut coordinator = pc.media_coordinator.lock().await;
        let Some(mut payload) = coordinator.recovery_start_media.clone() else {
            return Ok(None);
        };
        let Some(video_generation) = coordinator.video.generation.checked_add(1) else {
            return Err(());
        };
        let Some(audio_generation) = coordinator.audio.generation.checked_add(1) else {
            return Err(());
        };
        payload.video_generation = video_generation;
        payload.audio_generation = audio_generation;
        coordinator.video.generation = video_generation;
        coordinator.video.pending_generation = Some(video_generation);
        coordinator.video.lifecycle = MediaSlotLifecycle::Transitioning;
        coordinator.audio.generation = audio_generation;
        coordinator.audio.pending_generation = payload.audio.as_ref().map(|_| audio_generation);
        coordinator.audio.lifecycle = if payload.audio.is_some() {
            MediaSlotLifecycle::Transitioning
        } else {
            MediaSlotLifecycle::Stable
        };
        coordinator.actual_video_phase = None;
        coordinator.actual_audio_phase = Some(if payload.audio.is_some() {
            AudioPipelinePhase::Starting
        } else {
            AudioPipelinePhase::Off
        });
        coordinator.audio_desired_active = payload.audio.is_some();
        coordinator.audio_expected_terminal =
            payload.audio.as_ref().map(|_| AudioPipelinePhase::Active);
        // Adaptive quality is an ephemeral controller decision, not part of
        // the accepted recovery snapshot. A new worker incarnation always
        // starts from the user's baseline and lets the controller converge
        // again from fresh telemetry.
        coordinator.adaptive_quality_override = None;
        coordinator.video_terminal_waiter = None;
        coordinator.audio_terminal_waiter = None;
        coordinator.recovery_start_media = Some(payload.clone());
        *pc.cached_start_media.write().await = Some(payload.clone());
        let mut fence = pc.media_output_fence.write().await;
        fence.video_epoch = pc.connection_epoch.clone();
        fence.video_generation = video_generation;
        fence.audio_epoch = pc.connection_epoch.clone();
        fence.audio_generation = audio_generation;
        fence.audio_open = false;
        Ok(Some(payload))
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
        let snapshot: Vec<(String, Option<Admission>)> = {
            let map = self.inner.read().await;
            let admissions = self.admissions.read().await;
            let mut out = Vec::with_capacity(map.len());
            for id in map.keys() {
                out.push((
                    id.clone(),
                    admissions
                        .by_connection
                        .get(id)
                        .map(|record| record.class.clone()),
                ));
            }
            out
        };
        self.resume_media_snapshot(worker_mgr, virtual_display, snapshot)
            .await;
    }

    pub async fn resume_media_for_connections(
        &self,
        worker_mgr: &WorkerManager,
        virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
        connection_ids: &[String],
    ) {
        let snapshot = {
            let admissions = self.admissions.read().await;
            connection_ids
                .iter()
                .map(|id| {
                    (
                        id.clone(),
                        admissions
                            .by_connection
                            .get(id)
                            .map(|record| record.class.clone()),
                    )
                })
                .collect()
        };
        self.resume_media_snapshot(worker_mgr, virtual_display, snapshot)
            .await;
    }

    async fn resume_media_snapshot(
        &self,
        worker_mgr: &WorkerManager,
        virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
        snapshot: Vec<(String, Option<Admission>)>,
    ) {
        for (id, admission) in snapshot {
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
                    .send_to_connection_worker(
                        &id,
                        ServiceToWorker::SetConnectionCeiling(
                            desk_ipc_protocol::message::SetConnectionCeilingPayload {
                                connection_id: id.clone(),
                                ceiling: Some(ceiling.clone()),
                            },
                        ),
                    )
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
            let payload = match self.prepare_fresh_recovery_payload(&id).await {
                Ok(Some(payload)) => payload,
                Ok(None) => {
                    log::debug!(
                        "[pc_manager] resume: no accepted recovery media for {id} (offer not exchanged \
                         yet) — skipping"
                    );
                    continue;
                }
                Err(()) => {
                    log::warn!(
                        "[pc_manager] resume: media generation exhausted for {id}; tearing down"
                    );
                    cleanup_pc(
                        self,
                        worker_mgr,
                        virtual_display,
                        &id,
                        "resume: media generation exhausted",
                    )
                    .await;
                    continue;
                }
            };
            log::info!("[pc_manager] resume: re-issuing StartMedia + ForceKeyframe for {id}");
            if let Err(e) = worker_mgr
                .send_to_interactive_connection_worker(&id, ServiceToWorker::StartMedia(payload))
                .await
            {
                log::warn!("[pc_manager] resume StartMedia for {id} failed: {e}");
                continue;
            }
            if let Err(e) = worker_mgr
                .send_to_interactive_connection_worker(
                    &id,
                    ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                        connection_id: id.clone(),
                    }),
                )
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
    pub(crate) async fn reset_media_for(
        &self,
        connection_id: &str,
        worker_mgr: &WorkerManager,
    ) -> RestartOutcome {
        self.restart_media_from_cached_payload(
            connection_id,
            worker_mgr,
            MediaRestartTrigger::TransportStuck,
        )
        .await
    }

    pub(crate) async fn restart_media_from_cached_payload(
        &self,
        connection_id: &str,
        worker_mgr: &WorkerManager,
        trigger: MediaRestartTrigger,
    ) -> RestartOutcome {
        if !self.contains(connection_id).await {
            log::debug!(
                "[pc_manager] restart_media_from_cached_payload: unknown connection \
                     {connection_id}; trigger={trigger:?}"
            );
            return RestartOutcome::Failed {
                stage: MediaRestartStage::UnknownConnection,
            };
        }
        let payload = match self.prepare_fresh_recovery_payload(connection_id).await {
            Ok(Some(payload)) => payload,
            Ok(None) => {
                log::warn!(
                    "[pc_manager] restart_media_from_cached_payload {connection_id}: \
                     trigger={trigger:?}; no cached StartMedia (offer never landed); leaving \
                     connection paused — caller must redo handle_offer"
                );
                return RestartOutcome::NoCachedPayload { left_paused: true };
            }
            Err(()) => {
                log::warn!(
                    "[pc_manager] restart_media_from_cached_payload {connection_id}: \
                     media generation exhausted"
                );
                return RestartOutcome::Failed {
                    stage: MediaRestartStage::StartMedia,
                };
            }
        };

        log::info!(
            "[pc_manager] restart_media_from_cached_payload {connection_id}: trigger={trigger:?}; \
             issuing StopMedia + StartMedia + ForceKeyframe"
        );
        if let Err(e) = worker_mgr
            .send_to_interactive_connection_worker(
                connection_id,
                ServiceToWorker::StopMedia(StopMediaPayload {
                    connection_id: connection_id.to_string(),
                    connection_epoch: payload.connection_epoch.clone(),
                }),
            )
            .await
        {
            log::warn!(
                "[pc_manager] restart_media_from_cached_payload {connection_id}: trigger={trigger:?}; \
                 StopMedia failed: {e}"
            );
            // Continue anyway — StartMedia is the actual recovery action.
        }

        if let Err(e) = worker_mgr
            .send_to_interactive_connection_worker(
                connection_id,
                ServiceToWorker::StartMedia(payload),
            )
            .await
        {
            log::warn!(
                "[pc_manager] restart_media_from_cached_payload {connection_id}: \
                 trigger={trigger:?}; StartMedia failed: {e}"
            );
            return RestartOutcome::Failed {
                stage: MediaRestartStage::StartMedia,
            };
        }
        if let Err(e) = worker_mgr
            .send_to_interactive_connection_worker(
                connection_id,
                ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                    connection_id: connection_id.to_string(),
                }),
            )
            .await
        {
            log::warn!(
                "[pc_manager] restart_media_from_cached_payload {connection_id}: \
                 trigger={trigger:?}; ForceKeyframe failed: {e}"
            );
            return RestartOutcome::Failed {
                stage: MediaRestartStage::ForceKeyframe,
            };
        }
        RestartOutcome::Restarted
    }

    /// Apply encoder-relevant live settings to exactly one admitted
    /// connection. Session settings are never broadcast to sibling browsers.
    pub async fn update_media_settings_for_connection(
        &self,
        worker_mgr: &WorkerManager,
        connection_id: &str,
        fps: Option<u32>,
        bitrate_kbps: Option<u32>,
        quality: Option<u32>,
        enable_dirty_rect: Option<bool>,
        show_mouse: Option<bool>,
    ) -> bool {
        if fps.is_none()
            && bitrate_kbps.is_none()
            && quality.is_none()
            && enable_dirty_rect.is_none()
            && show_mouse.is_none()
        {
            return true;
        }
        let Some(ctx) = self.get(connection_id).await else {
            return false;
        };
        let ctx = ctx.read().await;
        let Some(cached) = ctx.cached_start_media.read().await.clone() else {
            return false;
        };
        let payload = UpdateMediaSettingsPayload {
            connection_id: connection_id.to_string(),
            connection_epoch: cached.connection_epoch,
            video_generation: cached.video_generation,
            fps,
            bitrate_kbps,
            quality,
            enable_dirty_rect,
            show_mouse,
        };
        drop(ctx);
        if let Err(e) = worker_mgr
            .send_to_interactive_connection_worker(
                connection_id,
                ServiceToWorker::UpdateMediaSettings(payload),
            )
            .await
        {
            log::warn!(
                "[pc_manager] update_media_settings_for_connection {connection_id}: send failed: {e}"
            );
            return false;
        }
        true
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
    connection_epoch: &str,
    video_generation: u32,
    directive: CapDirective,
    state: &mut AdaptiveBitrateState,
) {
    let payload = UpdateMediaSettingsPayload {
        connection_id: connection_id.to_string(),
        connection_epoch: connection_epoch.to_string(),
        video_generation,
        fps: None,
        bitrate_kbps: Some(directive.wire_kbps()),
        quality: None,
        enable_dirty_rect: None,
        show_mouse: None,
    };
    match worker_mgr
        .send_to_interactive_connection_worker(
            connection_id,
            ServiceToWorker::UpdateMediaSettings(payload),
        )
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

fn stable_video_generation(coordinator: &PerConnectionMediaCoordinator) -> Option<u32> {
    (coordinator.video.lifecycle == MediaSlotLifecycle::Stable)
        .then_some(coordinator.video.generation)
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
/// CloseRemoteSession, which is the natural lifetime of the task. A noisy
/// transient read error logs at warn level and continues, because the
/// rtp_sender survives single bad reads (e.g. malformed RTCP packet
/// from a buggy proxy).
fn spawn_rtcp_feedback_task(
    rtp_sender: Arc<RTCRtpSender>,
    connection_id: String,
    connection_epoch: String,
    worker_mgr: WorkerManager,
    adaptive_bitrate: Arc<AdaptiveBitrateShared>,
    media_coordinator: Arc<tokio::sync::Mutex<PerConnectionMediaCoordinator>>,
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
                            if let Err(e) = worker_mgr
                                .send_to_interactive_connection_worker(&connection_id, msg)
                                .await
                            {
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
                            // Serialize with a 301 video transition. If a
                            // restart has reserved a new generation but has not
                            // yet reached its terminal, neither the old nor the
                            // pending encoder is a safe bitrate target.
                            let video_generation = {
                                let coordinator = media_coordinator.lock().await;
                                stable_video_generation(&coordinator)
                            };
                            let Some(video_generation) = video_generation else {
                                continue;
                            };
                            let mut state = adaptive_bitrate.state.lock().await;
                            if let Some(directive) =
                                state.decide_on_remb(std::time::Instant::now(), remb.bitrate as f64)
                            {
                                send_cap_directive(
                                    &worker_mgr,
                                    &connection_id,
                                    &connection_epoch,
                                    video_generation,
                                    directive,
                                    &mut state,
                                )
                                .await;
                            }
                            drop(state);
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

mod signaling_helpers;
use signaling_helpers::*;

mod request_remote;
pub use request_remote::*;

mod offer;
use offer::media_codec_to_str;
pub use offer::*;

mod media_output;
pub use media_output::*;

mod control_output;
pub use control_output::*;

mod file_output;
use file_output::spawn_file_transfer_writer_task;
pub use file_output::*;

mod data_channel_limits;
use data_channel_limits::*;

mod connection_lifecycle;
use connection_lifecycle::register_peer_connection_state_cleanup;
pub use connection_lifecycle::*;

mod control;
pub use control::*;

#[cfg(test)]
mod tests;
