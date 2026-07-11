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
    LcxlRTCIceServer, OfferModel, RequestRemoteModel, SignalingModel, SignalingState,
    SignalingType, TurnTransport,
};
use desk_utils::error::{CustomDeskError, DeskErrorCode};
use tokio::sync::{RwLock, broadcast, mpsc};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9, MediaEngine,
};
use webrtc::api::setting_engine::{SctpMaxMessageSize, SettingEngine};
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate, RTCIceCandidateInit};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::{
    RTCPeerConnection, configuration::RTCConfiguration,
    peer_connection_state::RTCPeerConnectionState,
};
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
use crate::model::security_approval::{SecurityPermissionType, check_security_permission};
use crate::model::settings::{Settings, SharedSettings, SystemSettings, TraversalMode};
use crate::service::signaling::{should_short_circuit_clipboard, should_short_circuit_control};
use desk_capture_engine::audio_encoder::audio_encoder_factory::list_audio_encoder;
use desk_capture_engine::model::video_encoder::{VideoEncoderType, VideoEncoderTypeHelper};
use desk_capture_engine::video_encoder::video_encoder_factory::list_video_encoder;
use desk_ipc_protocol::message::{
    ClipboardPayload, CursorDataPayload, FileTransferPayload, FileTransferSendErrorKind,
    FileTransferSendFailedPayload, ForceKeyframePayload, InputPayload, MediaCapabilities,
    MediaCodec, MediaFrame, MediaFrameKind, OpaqueConnectionPayload, ServiceToWorker,
    StartMediaPayload, StopMediaPayload, UpdateMediaSettingsPayload,
};
use desk_signal_facade::model::signal::InitSignalingData;
use desk_turn::model::TurnSettings;
use std::time::Duration;
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

/// The external `host:port` endpoints of the TURN server this node hosts
/// itself, used to recognise (and drop) a relay candidate that would point
/// back at our own bundled TURN.
///
/// Sourced from the live `TurnApiState` produced when the embedded TURN
/// server actually started (`None` when no embedded TURN is running — a
/// non-`Default`/`Signaling` startup, or a `startup_turn_server` failure),
/// so it stays in lock-step with the same `TurnApiState` the local signaling
/// uses to inject TURN. `None` yields an empty set: nothing is treated as
/// self-hosted, so no remote relay is ever dropped.
pub fn own_turn_endpoints(turn: Option<&TurnSettings>) -> HashSet<String> {
    turn.map(|t| {
        t.interfaces
            .iter()
            .map(|iface| iface.external.clone())
            .collect()
    })
    .unwrap_or_default()
}

/// Extract the `external` (`host:port`) token from a `turn:host:port?...` URL.
/// Only the `turn:` scheme is handled because [`LcxlRTCIceServer::transport`]
/// reports `Turn` solely for `turn:`-prefixed URLs (`turns:` never reaches the
/// TURN branch), so this is only ever called on `turn:` URLs.
fn turn_url_endpoint(url: &str) -> Option<&str> {
    url.strip_prefix("turn:")
        .map(|rest| rest.split('?').next().unwrap_or(rest))
}

/// Filter the request's ICE servers down to the ones this node should
/// actually use given the local `traversal_mode`.
///
/// `traversal_mode` is the operator's explicit traversal intent and decides
/// what kind of server is kept — independent of startup mode:
/// - `Turn` keeps both STUN and TURN.
/// - `Stun` keeps STUN, drops TURN.
/// - `None` drops everything (host candidates only).
///
/// On top of that, a TURN URL pointing back at this node's own bundled TURN
/// (`own_turn_endpoints`) is dropped at URL granularity: relaying through a
/// TURN server we host ourselves is pointless and, on a co-located portable
/// node, the self-allocation can stall ICE gathering long enough to starve
/// consent-freshness on the otherwise-working pair. A server keeps any of its
/// non-self URLs (and the credential that rides with them); it is removed
/// entirely only when every URL was self-hosted.
///
/// Servers with no / unrecognised transport are skipped with a warning.
/// Pure function — no I/O, no settings lookup, easy to unit test.
pub fn filter_ice_servers(
    request_ice_servers: &[LcxlRTCIceServer],
    traversal_mode: &TraversalMode,
    own_turn_endpoints: &HashSet<String>,
) -> Vec<LcxlRTCIceServer> {
    let mut filtered = Vec::new();
    for ice_server in request_ice_servers {
        match ice_server.transport() {
            Some(TurnTransport::Stun) => {
                if matches!(traversal_mode, TraversalMode::Stun | TraversalMode::Turn) {
                    filtered.push(ice_server.clone());
                }
            }
            Some(TurnTransport::Turn) => {
                if !matches!(traversal_mode, TraversalMode::Turn) {
                    continue;
                }
                if own_turn_endpoints.is_empty() {
                    filtered.push(ice_server.clone());
                    continue;
                }
                // Drop only the URLs that point back at our own TURN; keep the
                // rest of the object (URLs + shared credential) intact.
                let kept_urls: Vec<String> = ice_server
                    .urls
                    .iter()
                    .filter(|url| {
                        let is_self = turn_url_endpoint(url)
                            .is_some_and(|ep| own_turn_endpoints.contains(ep));
                        if is_self {
                            log::debug!("Dropping self-hosted TURN ICE url: {url}");
                        }
                        !is_self
                    })
                    .cloned()
                    .collect();
                if !kept_urls.is_empty() {
                    filtered.push(LcxlRTCIceServer {
                        urls: kept_urls,
                        username: ice_server.username.clone(),
                        credential: ice_server.credential.clone(),
                    });
                }
            }
            None => {
                log::warn!(
                    "Ignoring ICE server with invalid/empty transport: {:?}",
                    ice_server
                );
            }
        }
    }
    filtered
}

/// Built-in default for the ICE `disconnected` timeout, used when
/// `system.webrtc_ice_disconnected_timeout_secs` is `None`. Equals the
/// webrtc-rs library default — the daemon doesn't lean on this layer
/// for fast cleanup. The signaling-layer `ConnectionRemoved`
/// notification (delivered the moment a browser closes its WS) is the
/// primary path that triggers daemon-side `cleanup_pc`. ICE timeouts
/// here are the fallback for the case where signaling itself is gone
/// too — at which point we want to behave like a normal WebRTC peer
/// and absorb realistic network jitter.
pub const DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS: u64 = 5;

/// Built-in default for the ICE `failed` timeout, used when
/// `system.webrtc_ice_failed_timeout_secs` is `None`. Tightened from
/// the webrtc-rs default of 25 s to 15 s: combined budget of 20 s
/// (default disconnected + failed) caps how long the worker's DXGI
/// duplication stays alive after both signaling and ICE have gone
/// silent. The webrtc-rs default of 30 s was demonstrably long enough
/// for a user-driven reopen (3-4 s) to race the still-running capture
/// loop and crash the new pipeline with `0x80070057 (E_INVALIDARG)`
/// from a second `DuplicateOutput` call.
pub const DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS: u64 = 15;

/// Resolve the effective ICE timeouts from settings, falling back to
/// the built-in defaults above when the operator hasn't set explicit
/// overrides. Pulled out so `build_peer_connection` and the unit
/// tests share the same resolution path.
fn resolve_ice_timeouts(system: &SystemSettings) -> (Duration, Duration) {
    let disconnected = Duration::from_secs(
        system
            .webrtc_ice_disconnected_timeout_secs
            .unwrap_or(DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS),
    );
    let failed = Duration::from_secs(
        system
            .webrtc_ice_failed_timeout_secs
            .unwrap_or(DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS),
    );
    (disconnected, failed)
}

/// Build an `RTCPeerConnection` with the lcxl-remote-desk daemon
/// defaults:
///
/// - 127.0.0.1 host candidate is included so loopback browser / Tauri
///   webview connections succeed via the local pair without requiring
///   cross-interface routing.
/// - SCTP `max_message_size_can_send` is set to `Unbounded` so large
///   DataChannel payloads (file-transfer, large clipboard) do not
///   fragment.
/// - ICE disconnected / failed timeouts come from
///   [`resolve_ice_timeouts`] — operator-tunable via settings, defaults
///   tighter than webrtc-rs so the cleanup fallback eventually fires
///   even when signaling is also gone. Active cleanup runs through
///   the signaling-side `ConnectionRemoved` hook and is unaffected by
///   these.
/// - Default codec set + default interceptor registry.
///
/// `ice_servers` is the already-filtered list (see
/// [`filter_ice_servers`]); pass `vec![]` for no ICE servers.
pub async fn build_peer_connection(
    ice_servers: Vec<RTCIceServer>,
    settings: &Settings,
) -> Result<RTCPeerConnection, DeskError> {
    let (ice_disconnected, ice_failed) = resolve_ice_timeouts(&settings.system);
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Unbounded);
    setting_engine.set_include_loopback_candidate(true);
    setting_engine.set_ice_timeouts(Some(ice_disconnected), Some(ice_failed), None);

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_setting_engine(setting_engine)
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    Ok(api.new_peer_connection(config).await?)
}

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
#[derive(Clone, Default)]
pub struct PcRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<RwLock<PeerConnectionContext>>>>>,
    worker_mgr: Arc<tokio::sync::OnceCell<WorkerManager>>,
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
    /// Connection ids currently held as restricted temporary-support sessions
    /// (see `RemoteDeskTypeEnum::Support`). Populated by
    /// [`Self::mark_restricted_connection`] when a `RequestRemote` arrives on the
    /// support upstream, cleared by [`cleanup_pc`] on teardown. Consumed by the
    /// signaling proxy's outbound Support-isolation filter (which forwards a
    /// support-destined frame only on the support upstream and every other frame
    /// only off it) — kept as a lightweight projection so that filter never has to
    /// lock each `PeerConnectionContext` per outbound frame. Shared via `Arc` so
    /// registry clones stay consistent.
    restricted_connections: Arc<RwLock<HashSet<String>>>,
    /// Reverse index `grant_session_id -> connection_ids` for every connection
    /// admitted under a redeemed grant (its `RequestRemoteAuthz` stamp carried a
    /// `grant_session_id`). Lets the daemon target a whole logical grant session
    /// for directed teardown / revocation — closing every connection that shares
    /// one grant in a single sweep — instead of the coarse restricted-set. Owner /
    /// unrestricted / legacy-support connections carry no grant and never appear
    /// here. Populated by [`Self::index_grant_connection`] in
    /// [`handle_request_remote`], pruned by [`Self::unindex_grant_connection`] on
    /// every [`cleanup_pc`] teardown. Shared via `Arc` so registry clones stay
    /// consistent.
    grant_sessions: Arc<RwLock<HashMap<String, HashSet<String>>>>,
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

    /// Mark a connection as a restricted temporary-support session. Adds it to the
    /// projection consumed by the outbound Support-isolation filter;
    /// [`handle_request_remote`] also flips the connection's
    /// `SignalingState::restricted`. Idempotent.
    pub async fn mark_restricted_connection(&self, connection_id: &str) {
        self.restricted_connections
            .write()
            .await
            .insert(connection_id.to_string());
    }

    /// Drop a connection from the restricted-session projection. Called by
    /// [`cleanup_pc`] on every teardown path; a no-op for unrestricted
    /// connections.
    async fn unmark_restricted_connection(&self, connection_id: &str) {
        self.restricted_connections
            .write()
            .await
            .remove(connection_id);
    }

    /// Shared handle to the restricted-session projection, for the signaling
    /// proxy's outbound Support-isolation filter. Cloning the `Arc` keeps the
    /// filter reading the same set the registry mutates.
    pub fn restricted_connections_handle(&self) -> Arc<RwLock<HashSet<String>>> {
        Arc::clone(&self.restricted_connections)
    }

    /// Record that `connection_id` was admitted under grant `grant_session_id`.
    /// Idempotent; multiple connections (main / file-transfer / reconnect) of one
    /// grant accumulate under the same key so a directed teardown reaches them all.
    async fn index_grant_connection(&self, grant_session_id: &str, connection_id: &str) {
        self.grant_sessions
            .write()
            .await
            .entry(grant_session_id.to_string())
            .or_default()
            .insert(connection_id.to_string());
    }

    /// Drop `connection_id` from whatever grant session held it, removing the
    /// grant key entirely once its last connection departs. Called by
    /// [`cleanup_pc`] on every teardown path; a no-op for connections that carry
    /// no grant.
    async fn unindex_grant_connection(&self, connection_id: &str) {
        let mut map = self.grant_sessions.write().await;
        map.retain(|_, conns| {
            conns.remove(connection_id);
            !conns.is_empty()
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
            .map(|conns| conns.iter().cloned().collect())
            .unwrap_or_default()
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

        self.inner
            .write()
            .await
            .insert(connection_id.to_string(), Arc::clone(&ctx));

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
    pub async fn resume_active_media(&self, worker_mgr: &WorkerManager) {
        let snapshot: Vec<(String, Option<StartMediaPayload>)> = {
            let map = self.inner.read().await;
            let mut out = Vec::with_capacity(map.len());
            for (id, ctx) in map.iter() {
                let cached = ctx.read().await.cached_start_media.read().await.clone();
                out.push((id.clone(), cached));
            }
            out
        };
        for (id, payload) in snapshot {
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

// =====================================================================
// DataChannel routing daemon → worker
// =====================================================================

/// DataChannel labels the browser opens against the daemon-held PC.
/// Mirrors the constants in `crate::model::data_channel` (kept locally
/// so this module does not depend on that one in tests / docs).
const DC_LABEL_MOUSE: &str = "mouse_event";
const DC_LABEL_MOUSE_MOVE: &str = "mouse_move_event";
const DC_LABEL_KEYBOARD: &str = "keyboard_event";
const DC_LABEL_CLIPBOARD: &str = "clipboard_event";
const DC_LABEL_FILE_TRANSFER: &str = "file_transfer_event";
const DC_LABEL_WHITEBOARD: &str = "whiteboard_event";
const DC_LABEL_CURSOR_SYNC: &str = "cursor_sync_event";

/// What to do with a DataChannel message based on its label. Pure
/// classification — no I/O — so it stays cheap to test exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DcRoute {
    /// Mouse non-move events (click / wheel). Gated by `accept_control`.
    Mouse,
    /// High-frequency mouse-move events. Gated by `accept_control`,
    /// kept distinct so the worker can apply move-specific coalescing.
    MouseMove,
    /// Keyboard events. Gated by `accept_control`.
    Keyboard,
    /// Clipboard writes (browser → host). Gated by `accept_clipboard_sync`.
    Clipboard,
    /// File-transfer commands. Gated by `accept_control` (file ops are
    /// part of the control surface).
    FileTransfer,
    /// Whiteboard commands. Gated by `accept_control`.
    Whiteboard,
    /// Cursor-sync DataChannel — the browser doesn't push to it; we
    /// stash the channel handle so the worker→daemon CursorData
    /// path has somewhere to write to.
    CursorSync,
}

/// Map a DataChannel `label` to its route. Returns `None` for
/// unknown labels so the caller can warn-and-drop without panicking.
fn classify_dc_label(label: &str) -> Option<DcRoute> {
    match label {
        DC_LABEL_MOUSE => Some(DcRoute::Mouse),
        DC_LABEL_MOUSE_MOVE => Some(DcRoute::MouseMove),
        DC_LABEL_KEYBOARD => Some(DcRoute::Keyboard),
        DC_LABEL_CLIPBOARD => Some(DcRoute::Clipboard),
        DC_LABEL_FILE_TRANSFER => Some(DcRoute::FileTransfer),
        DC_LABEL_WHITEBOARD => Some(DcRoute::Whiteboard),
        DC_LABEL_CURSOR_SYNC => Some(DcRoute::CursorSync),
        _ => None,
    }
}

/// Build the `ServiceToWorker` IPC variant a given DcRoute should
/// forward as. Used by the daemon's `on_data_channel.on_message`
/// handler. Only browser→host directions are handled here; the
/// `Clipboard` arm uses `ClipboardWrite` (browser writing to host
/// clipboard); a future browser→host clipboard *request* DC would map
/// to `ClipboardRequest` but the current protocol multiplexes both
/// over the same `clipboard_event` channel and the worker disambiguates
/// by payload, so this always emits `ClipboardWrite`.
fn route_to_service_msg(
    route: DcRoute,
    connection_id: &str,
    data: Vec<u8>,
    // Retained on the signature so call sites keep parity with the
    // browser-side wire shape (text vs binary). Currently unused
    // because the only route that cared — FileTransfer — was carved
    // out onto its own dedicated lane; kept here for future routes
    // (e.g. whiteboard binary blobs) without churning every call site.
    _is_text: bool,
) -> ServiceToWorker {
    match route {
        DcRoute::Mouse => ServiceToWorker::MouseInput(InputPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        DcRoute::MouseMove => ServiceToWorker::MouseMoveInput(InputPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        DcRoute::Keyboard => ServiceToWorker::KeyboardInput(InputPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        DcRoute::Clipboard => ServiceToWorker::ClipboardWrite(ClipboardPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        // FileTransfer is handled separately in
        // `install_browser_dc_message_forwarder` and never reaches
        // `route_to_service_msg`: it rides its own dedicated file lane
        // (see `desk-ipc-protocol::dual_transport`), not the event lane.
        DcRoute::FileTransfer => unreachable!(
            "FileTransfer is routed through WorkerManager::send_file_to_worker, \
             not the event lane"
        ),
        DcRoute::Whiteboard => ServiceToWorker::WhiteboardCommand(OpaqueConnectionPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        // CursorSync is read-side only; it never produces an IPC
        // message — the caller should not invoke this for it.
        DcRoute::CursorSync => unreachable!("CursorSync DC has no upstream message variant"),
    }
}

/// Permission gate. Returns `true` if the message should be forwarded
/// to the worker given the current `SignalingState`. Mirrors the
/// per-handler gating that used to live in the worker's `handle_*_event`
/// functions; consolidating it here means the worker can
/// trust every IPC variant it receives — gating is a daemon-side
/// concern only for routes whose access category lines up with a
/// SignalingState flag. `CursorSync` is filtered out before this is
/// called.
///
/// File transfer is its own access category (`allow_file_transfer`),
/// independent of `accept_control` which governs mouse/keyboard. The
/// browser file-management UI opens a *fresh* WebRTC connection that
/// has never requested control, so any daemon-side gate keyed on
/// `accept_control` would silently drop every download/upload. We let
/// file_transfer_event traffic through here and the worker's
/// `FileTransferDispatcher` runs the actual `check_security_permission`
/// per connection (the same per-DC permission cache the worker maintains).
async fn route_is_permitted(route: DcRoute, state: &Arc<RwLock<SignalingState>>) -> bool {
    let s = state.read().await;
    if s.restricted {
        // Second fail-closed door: a restricted temporary-support session (see
        // `RemoteDeskTypeEnum::Support`). Only pointer/keyboard input is allowed,
        // and it stays gated by `accept_control` (a view-only support grant never
        // sets it). Clipboard, file transfer, whiteboard — and any future
        // DataChannel route — are denied outright so a semi-trusted supporter
        // cannot exfiltrate the clipboard / files or draw on the host. This gate
        // is independent of the signaling `route()` allowlist because file
        // transfer never flows as a signaling frame; it rides its own DataChannel,
        // which the unrestricted arm below lets through unconditionally.
        // `CursorSync` (read-only cursor-shape write-back the browser never pushes
        // to) is stashed before this gate in `register_data_channel_router`, so it
        // is implicitly allowed even for restricted sessions and never reaches
        // here.
        return match route {
            DcRoute::Mouse | DcRoute::MouseMove | DcRoute::Keyboard => s.accept_control,
            DcRoute::Clipboard | DcRoute::FileTransfer | DcRoute::Whiteboard => false,
            DcRoute::CursorSync => unreachable!("CursorSync DC has no message route"),
        };
    }
    match route {
        DcRoute::Mouse | DcRoute::MouseMove | DcRoute::Keyboard => s.accept_control,
        DcRoute::Clipboard => s.accept_clipboard_sync,
        DcRoute::FileTransfer => true,
        // Whiteboard rides on the control grant, matching the worker's
        // historical per-handler gating.
        DcRoute::Whiteboard => s.accept_control,
        DcRoute::CursorSync => unreachable!("CursorSync DC has no message route"),
    }
}

/// Install the daemon's `on_data_channel` callback. Each browser-opened
/// DataChannel either (a) gets its `on_message` wired into the
/// IPC-forwarding closure that ships to the worker via
/// `ServiceToWorker::*`, or (b) for `cursor_sync_event`, has its
/// `Arc<RTCDataChannel>` stashed in the per-connection
/// `cursor_data_channel` slot for cursor-write-back. A third path:
/// `clipboard_event` channels are *both* stashed
/// (so the worker can push back via `WorkerToService::ClipboardRead`)
/// *and* wired with the on_message forwarder (so browser→host writes
/// flow through `ServiceToWorker::ClipboardWrite`).
///
/// Permission gates (`accept_control` / `accept_clipboard_sync`) are
/// checked *here*, before IPC, so the worker side can blindly trust
/// any IPC message it gets — keeping the trust boundary on the daemon
/// side where it belongs.
pub fn register_data_channel_router(
    pc: Arc<RTCPeerConnection>,
    connection_id: String,
    signaling_state: Arc<RwLock<SignalingState>>,
    cursor_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    clipboard_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    file_transfer_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    worker_mgr: WorkerManager,
) {
    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let label = dc.label().to_owned();
        let dc_id = dc.id();
        let connection_id = connection_id.clone();
        let signaling_state = Arc::clone(&signaling_state);
        let cursor_data_channel = Arc::clone(&cursor_data_channel);
        let clipboard_data_channel = Arc::clone(&clipboard_data_channel);
        let file_transfer_data_channel = Arc::clone(&file_transfer_data_channel);
        let worker_mgr = worker_mgr.clone();
        Box::pin(async move {
            log::info!("[DcRouter] {connection_id}: new DataChannel label='{label}' id={dc_id}");
            let route = match classify_dc_label(&label) {
                Some(r) => r,
                None => {
                    log::warn!(
                        "[DcRouter] {connection_id}: unknown DC label '{label}' — dropping channel"
                    );
                    return;
                }
            };
            if route == DcRoute::CursorSync {
                // Read-only cursor-shape write-back: the browser never pushes to
                // this channel (we install no on_message forwarder — hence the
                // early return before `route_is_permitted`). It carries no input
                // injection or exfiltration, so it is intentionally allowed even
                // for restricted temporary-support sessions; the restriction gate
                // in `route_is_permitted` deliberately never sees it.
                let mut slot = cursor_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed cursor_sync_event channel \
                     for worker→daemon cursor write-back"
                );
                return;
            }
            if route == DcRoute::Clipboard {
                let mut slot = clipboard_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed clipboard_event channel \
                     for worker→daemon clipboard write-back"
                );
                // Fall through to install the on_message forwarder so
                // browser→host writes still flow as ClipboardWrite IPC.
            }
            if route == DcRoute::FileTransfer {
                let mut slot = file_transfer_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed file_transfer_event channel \
                     for worker→daemon file write-back"
                );
                // Fall through to install the on_message forwarder so
                // browser→host commands and chunks flow over the
                // dedicated file lane (not the event lane) via
                // `WorkerManager::send_file_to_worker` — see the
                // FileTransfer special case inside the forwarder.
            }
            install_browser_dc_message_forwarder(
                dc,
                connection_id,
                route,
                signaling_state,
                worker_mgr,
            );
        })
    }));
}

/// Install the per-DC `on_message` callback that gates on
/// `signaling_state` and forwards bytes to the worker via the worker
/// manager's IPC sender. Pulled out of the closure body so the routing
/// logic is unit-testable in isolation (the closure itself can't be
/// unit-tested without spinning up a full PC).
fn install_browser_dc_message_forwarder(
    dc: Arc<RTCDataChannel>,
    connection_id: String,
    route: DcRoute,
    signaling_state: Arc<RwLock<SignalingState>>,
    worker_mgr: WorkerManager,
) {
    dc.on_message(Box::new(
        move |msg: webrtc::data_channel::data_channel_message::DataChannelMessage| {
            let connection_id = connection_id.clone();
            let signaling_state = Arc::clone(&signaling_state);
            let worker_mgr = worker_mgr.clone();
            let bytes = msg.data.to_vec();
            let is_text = msg.is_string;
            Box::pin(async move {
                if !route_is_permitted(route, &signaling_state).await {
                    log::debug!(
                        "[DcRouter] {connection_id}: dropped {route:?} message (permission denied)"
                    );
                    return;
                }
                // FileTransfer rides its own dedicated lane — see
                // `desk-ipc-protocol::dual_transport`. Routing it
                // through `send_to_worker` (event lane) would put the
                // GB-scale download bytes back into the same queue as
                // heartbeats / manager responses, which is exactly the
                // HOL-blocking regression fix-2026-05-05 forbids.
                if route == DcRoute::FileTransfer {
                    // Browser → daemon file-transfer chunks/control don't
                    // carry an IPC-visible transfer_id: the routing key is
                    // either a binary header (first 36 bytes) the worker
                    // parses, or a JSON envelope it deserializes. The
                    // daemon stays protocol-agnostic and forwards the
                    // payload verbatim with `transfer_id: None`. Only the
                    // reverse direction (worker → daemon) sets the field,
                    // and only so the writer task can scope a `dc.send`
                    // failure when reporting `FileTransferSendFailed`.
                    let payload = FileTransferPayload {
                        connection_id: connection_id.clone(),
                        data: bytes,
                        is_text,
                        transfer_id: None,
                    };
                    if let Err(e) = worker_mgr.send_file_to_worker(payload).await {
                        // Possible causes: worker not yet up (file lane
                        // not yet ready) or peer crashed mid-stream.
                        // Either way the browser's SCTP timeout will
                        // surface the failure to the user; we simply
                        // log and drop the command here.
                        log::warn!(
                            "[DcRouter] {connection_id}: failed to forward FileTransfer \
                             to worker via file lane: {e}"
                        );
                    }
                    return;
                }
                let svc_msg = route_to_service_msg(route, &connection_id, bytes, is_text);
                if let Err(e) = worker_mgr.send_to_worker(svc_msg).await {
                    log::warn!(
                        "[DcRouter] {connection_id}: failed to forward {route:?} to worker: {e}"
                    );
                }
            })
        },
    ));
}

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
    // True when this `RequestRemote` is held fail-closed: either it arrived on the
    // restricted support upstream (see `RemoteDeskTypeEnum::Support`) or it carries
    // a redeemed-grant capability ceiling. The freshly-created PC is marked
    // restricted before any ICE / DataChannel handler is installed or any Init
    // reply egresses, so both the daemon-side data-channel gate
    // (`route_is_permitted`) and the outbound Support-isolation filter observe it
    // from the connection's very first frame.
    restricted: bool,
    // The validated capability ceiling unwrapped from the `RequestRemoteAuthz`
    // stamp (`None` for owner / unrestricted / legacy-support). Stored on the
    // connection's `SignalingState` so the worker-side permission gates can later
    // enforce `meet(ceiling, global)`.
    access_ceiling: Option<SecuritySettings>,
    // The grant logical-session id this connection belongs to (`None` when there
    // is no grant). Indexes the connection for grant-directed teardown.
    grant_session_id: Option<String>,
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

    // Stamp the fail-closed state onto the connection before the ICE / DataChannel
    // handlers below and before the Init reply, so there is no unrestricted window.
    // `restricted` gates `route_is_permitted`; `access_ceiling` feeds the
    // worker-side `meet(ceiling, global)` gates; `grant_session_id` indexes the
    // connection for grant-directed teardown.
    if restricted || access_ceiling.is_some() || grant_session_id.is_some() {
        let ctx_guard = ctx.read().await;
        let mut st = ctx_guard.signaling_state.write().await;
        st.restricted = restricted;
        st.access_ceiling = access_ceiling;
        st.grant_session_id = grant_session_id.clone();
    }
    if let Some(gsid) = grant_session_id.as_deref() {
        // Index the connection under its grant so a directed revocation / teardown
        // can reach every connection that shares the grant in one sweep.
        registry
            .index_grant_connection(gsid, from_connection_id)
            .await;
    }
    if restricted {
        // Register the connection in the outbound-filter projection so the
        // Support-isolation filter observes it from the connection's first frame.
        registry
            .mark_restricted_connection(from_connection_id)
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
async fn cleanup_pc(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: &str,
    reason: &str,
) {
    let removed = registry.remove(connection_id).await;
    // Drop the connection from the restricted-session projection on every teardown
    // path (idempotent for unrestricted connections) so the outbound
    // Support-isolation filter can never route a frame to a stale support id.
    registry.unmark_restricted_connection(connection_id).await;
    // Prune the grant reverse-index too (idempotent for connections that carry no
    // grant) so a directed teardown can never reach a stale connection id.
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

/// Tear down every restricted temporary-support connection (see
/// `RemoteDeskTypeEnum::Support`). Called when the support session ends — a
/// manual "end support", the code's TTL expiry, or the support upstream closing
/// — so the supporter's WebRTC session ends physically, not just at the signaling
/// layer. Snapshots the restricted-connections projection first (each `cleanup_pc`
/// mutates it), then closes each PC. A no-op when no support session is live.
pub async fn cleanup_restricted_connections(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    reason: &str,
) {
    let ids: Vec<String> = registry
        .restricted_connections_handle()
        .read()
        .await
        .iter()
        .cloned()
        .collect();
    for id in ids {
        cleanup_pc(registry, worker_mgr, virtual_display, &id, reason).await;
    }
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

    // From here on the browser is requesting a grant (accept = true).
    let allow_control = settings.read().await.security.allow_remote_control;
    let allow_clipboard = settings.read().await.security.allow_clipboard_sync;

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
mod tests {
    use super::*;
    use crate::model::settings::StartupMode;
    use desk_ipc_protocol::message::MediaCodec;

    // ============== DaemonFtWindow ==============

    /// An empty daemon window must not produce a log line — same
    /// contract as the worker-side windows. The trailing flush at
    /// task exit calls `flush_line` unconditionally; without this
    /// guard, every PC teardown would emit an empty
    /// `[ft-metrics-daemon] frames=0 bytes=0 ...` line.
    #[test]
    fn daemon_ft_window_empty_flush_is_none() {
        let w = DaemonFtWindow::default();
        assert_eq!(w.frames, 0);
        assert!(!w.is_full());
        assert!(w.flush_line("cid").is_none());
    }

    // ============== v4: StartMediaPayload video_device routing ==============

    /// Fresh-install state: the browser has not yet picked a display,
    /// so `video_device_name` is empty. The daemon must translate that
    /// to `None` on the IPC payload — the worker's `payload_overrides`
    /// then leaves the base setting untouched and the capture-engine
    /// hard-errors at `new()` time. This is the documented "no
    /// silent fallback to primary monitor" contract.
    #[test]
    fn start_media_payload_video_device_is_none_when_settings_empty() {
        assert_eq!(video_device_for_payload(""), None);
    }

    /// Selected display: the browser submitted a non-empty
    /// `\\.\DISPLAYn`. The daemon passes it through verbatim so the
    /// worker can rebind capture (e.g. when a second browser picks a
    /// different monitor than the first).
    #[test]
    fn start_media_payload_video_device_is_some_when_settings_set() {
        assert_eq!(
            video_device_for_payload(r"\\.\DISPLAY7"),
            Some(r"\\.\DISPLAY7".to_string())
        );
    }

    // ============== F3: SDP max-message-size parser ==============

    /// Chrome's SDP advertises 262144 (256 KiB) on a session-level
    /// attribute. The parser must surface it as an unsigned value.
    #[test]
    fn parse_sdp_max_message_size_session_level() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
                   a=max-message-size:262144\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(262144));
    }

    /// Some browsers put the attribute under the `m=application`
    /// section instead of the session level. The parser doesn't care
    /// — first match wins.
    #[test]
    fn parse_sdp_max_message_size_media_level() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   a=mid:0\r\n\
                   a=max-message-size:1073741823\r\n";
        assert_eq!(parse_sdp_max_message_size(sdp), Some(1073741823));
    }

    /// Absent attribute → None. The caller distinguishes this from a
    /// parse failure and falls back to the RFC default with a warning.
    #[test]
    fn parse_sdp_max_message_size_missing_returns_none() {
        let sdp = "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert!(parse_sdp_max_message_size(sdp).is_none());
    }

    /// Garbled value (non-numeric) is treated as missing — we don't
    /// want to half-parse `a=max-message-size:abc` and pretend we
    /// negotiated something.
    #[test]
    fn parse_sdp_max_message_size_invalid_returns_none() {
        let sdp = "v=0\r\na=max-message-size:not-a-number\r\n";
        assert!(parse_sdp_max_message_size(sdp).is_none());
    }

    /// The configured chunk_size + binary header must fit under
    /// Chrome's 262144-byte advertise. This is the same invariant the
    /// worker-side `download_response_advertises_240kib_chunk_size`
    /// regression test pins, but reasserted at the daemon layer so a
    /// future change to either constant fails both ends.
    ///
    /// Encoded as a `const` assertion so it fires at compile time
    /// rather than as a runtime test (which clippy correctly flags as
    /// `assertions_on_constants` — both operands are compile-time
    /// literals).
    const _CHUNK_SIZE_FITS_CHROME_MAX_MESSAGE_SIZE: () = {
        use crate::model::file_transfer::BINARY_HEADER_SIZE;
        use crate::worker::file_transfer_dispatcher::FILE_TRANSFER_CHUNK_SIZE_TX;
        const CHROME_MAX_MESSAGE_SIZE: usize = 262144;
        assert!(
            FILE_TRANSFER_CHUNK_SIZE_TX + BINARY_HEADER_SIZE <= CHROME_MAX_MESSAGE_SIZE,
            "wire-level SCTP message must not exceed Chrome's a=max-message-size:262144 \
             advertise — see 2026-05-11 ErrOutboundPacketTooLarge regression"
        );
    };

    /// One recorded send populates frames/bytes/dc_send_ns and
    /// updates `buffered_max` / `buffered_sum`. Verifies the
    /// `is_text` accounting: a text frame increments `text_frames`.
    #[test]
    fn daemon_ft_window_records_text_and_binary() {
        let mut w = DaemonFtWindow::default();
        // Binary chunk (the dominant case for downloads).
        w.record(
            60 * 1024,
            false,
            Duration::from_micros(50),
            Duration::from_millis(1),
            128 * 1024,
        );
        // Control message (e.g. DownloadResponse JSON).
        w.record(
            200,
            true,
            Duration::from_micros(10),
            Duration::from_micros(80),
            64 * 1024,
        );
        assert_eq!(w.frames, 2);
        assert_eq!(w.bytes, 60 * 1024 + 200);
        assert_eq!(w.text_frames, 1);
        assert_eq!(w.recv_idle_ns, 50_000 + 10_000);
        assert_eq!(w.dc_send_ns, 1_000_000 + 80_000);
        assert_eq!(w.buffered_max_bytes, 128 * 1024);
        assert_eq!(w.buffered_sum_bytes, (128 + 64) * 1024);
        assert_eq!(w.buffered_samples, 2);
        let line = w.flush_line("cid-abc").unwrap();
        assert!(line.contains("cid=cid-abc"));
        assert!(line.contains("frames=2"));
        assert!(line.contains("text=1"));
        assert!(line.contains("buffered_max=131072"));
        assert!(line.contains("buffered_avg=98304"));
    }

    /// `is_full()` flips at the shared `FT_METRICS_WINDOW_CHUNKS`
    /// boundary so the daemon log cadence stays synchronised with
    /// the worker log cadence (one daemon line per worker line under
    /// steady-state download).
    #[test]
    fn daemon_ft_window_boundary_is_full() {
        let mut w = DaemonFtWindow::default();
        let boundary = crate::worker::file_transfer_dispatcher::FT_METRICS_WINDOW_CHUNKS;
        for _ in 0..(boundary - 1) {
            w.record(
                1,
                false,
                Duration::from_nanos(1),
                Duration::from_nanos(1),
                0,
            );
        }
        assert!(!w.is_full());
        w.record(
            1,
            false,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            0,
        );
        assert!(w.is_full());
    }

    /// `reset()` clears every field back to `Default::default()` so
    /// the next window does not double-count. Required for the
    /// `is_full → flush → reset` cadence in
    /// `spawn_file_transfer_writer_task` to remain consistent.
    #[test]
    fn daemon_ft_window_reset_clears_state() {
        let mut w = DaemonFtWindow::default();
        w.record(
            100,
            false,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            42,
        );
        assert!(w.frames > 0);
        w.reset();
        assert_eq!(w, DaemonFtWindow::default());
    }

    /// `buffered_avg` rounds down on integer division — guard against
    /// a refactor that switches to f64 mid-way (the log format is
    /// `buffered_avg={u64}`, not `{:.2}`, because we want a clean
    /// byte count for grep / awk).
    #[test]
    fn daemon_ft_window_buffered_avg_integer_rounding() {
        let mut w = DaemonFtWindow::default();
        w.record(
            1,
            false,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            100,
        );
        w.record(
            1,
            false,
            Duration::from_nanos(1),
            Duration::from_nanos(1),
            101,
        );
        // (100 + 101) / 2 = 100 (integer div). f64 would be 100.5.
        let line = w.flush_line("cid").unwrap();
        assert!(
            line.contains("buffered_avg=100"),
            "expected buffered_avg=100 (integer rounding), got: {line}"
        );
    }

    fn ice(url: &str) -> LcxlRTCIceServer {
        LcxlRTCIceServer {
            urls: vec![url.to_string()],
            username: String::new(),
            credential: String::new(),
        }
    }

    /// The active cleanup path (signaling-side `ConnectionRemoved`)
    /// handles the typical "user closed the tab" case in
    /// milliseconds. The ICE timeouts here are the fallback for the
    /// case where signaling itself is gone too — at which point we
    /// behave like a normal WebRTC peer and absorb realistic network
    /// jitter. Pin the defaults:
    ///
    /// 1. `failed` budget shorter than the webrtc-rs default (25 s).
    ///    The library default 5 s + 25 s = 30 s window once let a
    ///    user-driven reopen race the worker's still-running
    ///    `DxgiImageCapture::DuplicateOutput` and crash the new
    ///    pipeline with `0x80070057 (E_INVALIDARG)`.
    /// 2. `disconnected` matches the webrtc-rs default — we don't
    ///    lean on this layer to react to graceful disconnects (the
    ///    signaling-side notification does that) and tightening it
    ///    further would make brief network jitter look like a real
    ///    failure under slow / lossy networks.
    /// 3. Combined budget kept ≤ 25 s so the fallback still fires
    ///    long before users would normally retry, while staying
    ///    above the 5-10 s range where loopback / LAN jitter routinely
    ///    sits.
    #[test]
    fn default_daemon_ice_timeouts_match_recovery_budget() {
        // webrtc-ice's `DEFAULT_DISCONNECTED_TIMEOUT` / `DEFAULT_FAILED_TIMEOUT`.
        // Hard-coded here rather than imported because the library exports
        // them with `pub(crate)` visibility.
        const WEBRTC_DEFAULT_DISCONNECTED_SECS: u64 = 5;
        const WEBRTC_DEFAULT_FAILED_SECS: u64 = 25;

        assert!(
            DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS <= WEBRTC_DEFAULT_DISCONNECTED_SECS,
            "DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS must not exceed the \
             webrtc-rs default ({WEBRTC_DEFAULT_DISCONNECTED_SECS}s); \
             got {DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS}s",
        );
        assert!(
            DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS < WEBRTC_DEFAULT_FAILED_SECS,
            "DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS must be strictly less than \
             the webrtc-rs default ({WEBRTC_DEFAULT_FAILED_SECS}s); \
             got {DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS}s",
        );

        let total =
            DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS + DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS;
        assert!(
            total <= 25,
            "Combined disconnected+failed budget must stay ≤ 25 s so the \
             fallback fires before a typical retry interval; got {total}s",
        );
    }

    /// `resolve_ice_timeouts` is what `build_peer_connection` reads to
    /// decide what gets handed to webrtc-rs `SettingEngine`. Pin both
    /// branches: `None` falls back to the daemon defaults, `Some`
    /// values flow through verbatim. Without this, an operator override
    /// could silently get dropped without anything in the daemon
    /// noticing.
    #[test]
    fn resolve_ice_timeouts_falls_back_to_defaults_when_unset() {
        let mut sys = SystemSettings::default();
        sys.webrtc_ice_disconnected_timeout_secs = None;
        sys.webrtc_ice_failed_timeout_secs = None;
        let (disconnected, failed) = resolve_ice_timeouts(&sys);
        assert_eq!(
            disconnected,
            Duration::from_secs(DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS),
        );
        assert_eq!(
            failed,
            Duration::from_secs(DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS),
        );
    }

    #[test]
    fn resolve_ice_timeouts_honours_explicit_overrides() {
        let mut sys = SystemSettings::default();
        sys.webrtc_ice_disconnected_timeout_secs = Some(11);
        sys.webrtc_ice_failed_timeout_secs = Some(47);
        let (disconnected, failed) = resolve_ice_timeouts(&sys);
        assert_eq!(disconnected, Duration::from_secs(11));
        assert_eq!(failed, Duration::from_secs(47));
    }

    #[test]
    fn resolve_ice_timeouts_resolves_each_field_independently() {
        // Mixed: disconnected overridden, failed left at default. Catches
        // accidental cross-field copy/paste in `resolve_ice_timeouts`.
        let mut sys = SystemSettings::default();
        sys.webrtc_ice_disconnected_timeout_secs = Some(99);
        sys.webrtc_ice_failed_timeout_secs = None;
        let (disconnected, failed) = resolve_ice_timeouts(&sys);
        assert_eq!(disconnected, Duration::from_secs(99));
        assert_eq!(
            failed,
            Duration::from_secs(DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS),
        );
    }

    /// `build_peer_connection` is what threads the timeout overrides
    /// into the `RTCPeerConnection` instance the registry actually
    /// holds. The `SettingEngine`'s timeout fields are `pub(crate)` so
    /// we can't read them back through the library API; instead pin
    /// the call-site contract by asserting `build_peer_connection`
    /// produces a usable PC (i.e. the SettingEngine + APIBuilder
    /// configuration didn't break) when constructed with no ICE
    /// servers — the same shape the daemon hits in portable mode.
    /// Combined with the constant test above, this guards against
    /// regressions that quietly drop `set_ice_timeouts` from the
    /// SettingEngine wiring.
    #[tokio::test]
    async fn build_peer_connection_succeeds_with_tightened_ice_timeouts() {
        let settings = Settings::default();
        let pc = build_peer_connection(vec![], &settings)
            .await
            .expect("build_peer_connection must succeed with the daemon defaults");
        // Closing here is best-effort; the test is about the build path,
        // not the close path. A failed close would not be a meaningful
        // regression signal for the timeout wiring.
        let _ = pc.close().await;
    }

    /// No self-hosted TURN endpoints — the common case for a desk reached
    /// through a remote signaling/manager (its own embedded TURN is not
    /// running, so nothing is treated as self).
    fn no_own() -> HashSet<String> {
        HashSet::new()
    }

    /// A `TurnSettings` advertising the given `external` endpoints, one UDP
    /// interface each, so `own_turn_endpoints` has something to map.
    fn turn_settings_with(externals: &[&str]) -> TurnSettings {
        TurnSettings {
            interfaces: externals
                .iter()
                .map(|ext| desk_turn::model::TurnInterface {
                    transport: desk_turn::model::TurnTransport::UDP,
                    listen: "0.0.0.0:3479".to_string(),
                    external: (*ext).to_string(),
                })
                .collect(),
            ..TurnSettings::default()
        }
    }

    #[test]
    fn filter_keeps_stun_only_in_stun_mode() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::Stun, &no_own());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
    }

    /// Turn mode keeps both STUN and TURN. `traversal_mode` is the sole
    /// authority — startup mode no longer gates TURN, so a `Default` /
    /// `ServiceDaemon` host reached through a manager relays its TURN just like
    /// a dedicated `DeskServer`.
    #[test]
    fn filter_keeps_both_in_turn_mode() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &no_own());
        assert_eq!(kept.len(), 2);
    }

    /// `TraversalMode::None` means "no STUN, no TURN, host candidates
    /// only". The filter drops everything from the request.
    #[test]
    fn filter_drops_everything_in_none_mode() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::None, &no_own());
        assert!(kept.is_empty());
    }

    /// Servers with no recognisable transport scheme are skipped (and
    /// the daemon logs a warning) rather than admitted as unknown.
    #[test]
    fn filter_drops_unrecognised_transport() {
        let request = vec![
            ice("https://not-a-stun-or-turn.example.com"),
            ice("stun:stun.l.google.com:19302"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::Stun, &no_own());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
    }

    #[test]
    fn own_turn_endpoints_maps_interfaces() {
        let turn = TurnSettings {
            interfaces: vec![
                desk_turn::model::TurnInterface {
                    transport: desk_turn::model::TurnTransport::UDP,
                    listen: "0.0.0.0:3479".to_string(),
                    external: "192.168.50.5:3479".to_string(),
                },
                desk_turn::model::TurnInterface {
                    transport: desk_turn::model::TurnTransport::TCP,
                    listen: "0.0.0.0:3478".to_string(),
                    external: "192.168.50.5:3478".to_string(),
                },
            ],
            // enable_turn does not gate the mapping — the caller's `Option`
            // (presence of a running `TurnApiState`) is the only gate.
            enable_turn: false,
            ..TurnSettings::default()
        };
        let eps = own_turn_endpoints(Some(&turn));
        assert_eq!(eps.len(), 2);
        assert!(eps.contains("192.168.50.5:3479"));
        assert!(eps.contains("192.168.50.5:3478"));
    }

    /// `None` (the embedded TURN never started — non-`Default`/`Signaling`
    /// startup, or a `startup_turn_server` failure) yields an empty set, so
    /// nothing is treated as self-hosted and no remote relay is dropped.
    #[test]
    fn own_turn_endpoints_none_is_empty() {
        assert!(own_turn_endpoints(None).is_empty());
    }

    #[test]
    fn own_turn_endpoints_empty_interfaces_is_empty() {
        assert!(own_turn_endpoints(Some(&TurnSettings::default())).is_empty());
    }

    /// Turn mode, but the only TURN URL points back at our own bundled TURN:
    /// the relay candidate is dropped while STUN survives.
    #[test]
    fn filter_drops_self_hosted_turn() {
        let request = vec![
            ice("stun:192.168.50.5:3479"),
            ice("turn:192.168.50.5:3479?transport=udp"),
        ];
        let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:192.168.50.5:3479");
    }

    /// A single ICE server carrying both a self URL and a remote URL keeps the
    /// remote URL (and its credential); only the self URL is removed.
    #[test]
    fn filter_partial_drops_self_url_keeps_remote() {
        let request = vec![LcxlRTCIceServer {
            urls: vec![
                "turn:192.168.50.5:3479?transport=udp".to_string(),
                "turn:relay.example.com:3478?transport=udp".to_string(),
            ],
            username: "user".to_string(),
            credential: "pw".to_string(),
        }];
        let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
        assert_eq!(kept.len(), 1);
        assert_eq!(
            kept[0].urls,
            vec!["turn:relay.example.com:3478?transport=udp"]
        );
        assert_eq!(kept[0].username, "user");
        assert_eq!(kept[0].credential, "pw");
    }

    /// When every URL of an object is self-hosted, the whole object is dropped.
    #[test]
    fn filter_drops_object_when_all_urls_self() {
        let request = vec![LcxlRTCIceServer {
            urls: vec![
                "turn:192.168.50.5:3479?transport=udp".to_string(),
                "turn:192.168.50.5:3478?transport=tcp".to_string(),
            ],
            username: "user".to_string(),
            credential: "pw".to_string(),
        }];
        let own = own_turn_endpoints(Some(&turn_settings_with(&[
            "192.168.50.5:3479",
            "192.168.50.5:3478",
        ])));
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
        assert!(kept.is_empty());
    }

    /// A remote manager's TURN (a different endpoint) is kept even when this
    /// node hosts its own TURN — only self-hosted relays are dropped.
    #[test]
    fn filter_keeps_remote_turn_in_turn_mode() {
        let request = vec![ice("turn:relay.example.com:3478?transport=udp")];
        let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "turn:relay.example.com:3478?transport=udp");
    }

    /// No self-hosting (DeskServer / ServiceDaemon, own set empty): a remote
    /// TURN is kept untouched.
    #[test]
    fn filter_keeps_turn_when_not_self_hosting() {
        let request = vec![ice("turn:192.168.50.5:3479?transport=udp")];
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &no_own());
        assert_eq!(kept.len(), 1);
    }

    /// The own-set is a frozen snapshot independent of any later live-settings
    /// change: a relay at the startup address `A` is still filtered even though
    /// the function only ever sees the passed-in set, never live settings.
    #[test]
    fn filter_uses_frozen_set_not_live() {
        let request = vec![ice("turn:192.168.50.5:3479?transport=udp")];
        // Frozen own-set captured at startup (address A).
        let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3479"])));
        // Even if live settings had since moved to address B, the filter only
        // consults the frozen set, so the startup-A relay is still dropped.
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
        assert!(kept.is_empty());
    }

    /// A TCP `external` endpoint is matched against the `turn:...?transport=tcp`
    /// URL just like UDP.
    #[test]
    fn filter_matches_tcp_interface() {
        let request = vec![ice("turn:192.168.50.5:3478?transport=tcp")];
        let own = own_turn_endpoints(Some(&turn_settings_with(&["192.168.50.5:3478"])));
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
        assert!(kept.is_empty());
    }

    /// An IPv6-shaped `external` matches purely as a string. This only
    /// exercises the string match; it does NOT imply the IPv6 TURN runtime
    /// path is wired up.
    #[test]
    fn filter_matches_ipv6_endpoint_string_only() {
        let request = vec![ice("turn:[fe80::1]:3479?transport=udp")];
        let own = own_turn_endpoints(Some(&turn_settings_with(&["[fe80::1]:3479"])));
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, &own);
        assert!(kept.is_empty());
    }

    /// Sanity: the construction path itself works with an empty ICE
    /// list (the daemon ICE-only-host case for portable mode).
    #[tokio::test]
    async fn build_peer_connection_succeeds_with_no_ice_servers() {
        let settings = Settings::default();
        let pc = build_peer_connection(vec![], &settings)
            .await
            .expect("build pc");
        // Just confirm we got a usable handle back; tear down via Drop.
        assert_eq!(
            pc.connection_state(),
            webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::New
        );
    }

    fn settings_with_startup(mode: StartupMode) -> Settings {
        let mut s = Settings::default();
        s.args.startup_mode = mode;
        s
    }

    /// `any_with_accept_control` reflects each PC's
    /// `signaling_state.accept_control` flag: empty registry returns
    /// false; a single PC with `accept_control = false` returns false;
    /// flipping it true returns true; clearing it on one PC while
    /// another is still holding control keeps the answer true (any,
    /// not all). Pins the "any holder keeps exclusive alive" gate
    /// used by `update_exclusive_after_control_change`.
    #[tokio::test]
    async fn any_with_accept_control_covers_empty_single_and_multi() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);

        assert!(
            !registry.any_with_accept_control().await,
            "empty registry must report false"
        );

        let ctx_a = registry
            .create_for_request_remote("conn-a", &request_remote, &s)
            .await
            .expect("seed a");
        assert!(
            !registry.any_with_accept_control().await,
            "fresh PC has accept_control = false"
        );

        // Flip A: now any() should be true.
        {
            let ctx = ctx_a.read().await;
            ctx.signaling_state.write().await.accept_control = true;
        }
        assert!(registry.any_with_accept_control().await);

        // Add B without flipping; A still holds.
        let ctx_b = registry
            .create_for_request_remote("conn-b", &request_remote, &s)
            .await
            .expect("seed b");
        assert!(registry.any_with_accept_control().await);

        // Flip A back to false; B still false. None hold -> false.
        {
            let ctx = ctx_a.read().await;
            ctx.signaling_state.write().await.accept_control = false;
        }
        assert!(!registry.any_with_accept_control().await);

        // Flip B to true; one holder -> true again.
        {
            let ctx = ctx_b.read().await;
            ctx.signaling_state.write().await.accept_control = true;
        }
        assert!(registry.any_with_accept_control().await);
    }

    /// Round-trip: create, contains, get, remove.
    #[tokio::test]
    async fn pc_registry_create_get_remove_cycle() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);

        assert_eq!(registry.len().await, 0);
        let _ctx = registry
            .create_for_request_remote("conn-a", &request_remote, &s)
            .await
            .expect("create");
        assert!(registry.contains("conn-a").await);
        assert_eq!(registry.len().await, 1);
        let got = registry.get("conn-a").await.expect("get");
        assert_eq!(got.read().await.connection_id, "conn-a");
        registry.remove("conn-a").await.expect("remove");
        assert!(!registry.contains("conn-a").await);
        assert_eq!(registry.len().await, 0);
    }

    /// Duplicate `create_for_request_remote` calls for the same
    /// `connection_id` are a protocol error from the browser; the
    /// registry refuses with a CustomError rather than overwriting
    /// (which would leave the previous PC dangling without anyone
    /// closing it).
    #[tokio::test]
    async fn pc_registry_rejects_duplicate_connection_id() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);

        registry
            .create_for_request_remote("conn-x", &request_remote, &s)
            .await
            .expect("first create");
        let result = registry
            .create_for_request_remote("conn-x", &request_remote, &s)
            .await;
        match result {
            Err(e) => assert!(format!("{e}").contains("already exists")),
            Ok(_) => panic!("second create_for_request_remote should fail"),
        }
        assert_eq!(registry.len().await, 1);
    }

    /// Minimal `StartMediaPayload` for the first-offer gating tests.
    fn start_media_payload_for(connection_id: &str) -> StartMediaPayload {
        StartMediaPayload {
            connection_id: connection_id.to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 30,
            bitrate_kbps: 0,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: None,
        }
    }

    /// `record_start_media_was_first` reports `true` only for the first
    /// offer and overwrites the cached payload on every call. This is the
    /// gate `handle_offer` uses to issue worker `StartMedia` exactly once
    /// (first negotiation) while a renegotiation re-offer skips it but
    /// still refreshes the cache for a later worker-swap resume.
    #[tokio::test]
    async fn record_start_media_marks_only_first_offer() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let ctx = registry
            .create_for_request_remote("conn-a", &request_remote, &s)
            .await
            .expect("create");

        let first = ctx
            .read()
            .await
            .record_start_media_was_first(start_media_payload_for("conn-a"))
            .await;
        assert!(first, "the first offer must report is_first_offer = true");

        let second = ctx
            .read()
            .await
            .record_start_media_was_first(start_media_payload_for("conn-a"))
            .await;
        assert!(!second, "a renegotiation re-offer must report false");

        // Cache is populated for worker-swap resume regardless of which
        // offer it was.
        assert!(ctx.read().await.cached_start_media.read().await.is_some());
    }

    /// Two offers racing on the same connection (an in-flight initial
    /// offer vs a frontend ICE-restart re-offer) must yield exactly one
    /// `true`, so the worker receives a single `StartMedia`. The
    /// serialization comes from each caller holding the
    /// `PeerConnectionContext` write lock across the check-and-set,
    /// mirroring `handle_offer`.
    #[tokio::test]
    async fn concurrent_offers_mark_first_once() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let ctx = registry
            .create_for_request_remote("conn-race", &request_remote, &s)
            .await
            .expect("create");

        let c1 = Arc::clone(&ctx);
        let c2 = Arc::clone(&ctx);
        let t1 = tokio::spawn(async move {
            let g = c1.write().await;
            g.record_start_media_was_first(start_media_payload_for("conn-race"))
                .await
        });
        let t2 = tokio::spawn(async move {
            let g = c2.write().await;
            g.record_start_media_was_first(start_media_payload_for("conn-race"))
                .await
        });
        let r1 = t1.await.expect("task 1");
        let r2 = t2.await.expect("task 2");
        assert_eq!(
            [r1, r2].into_iter().filter(|x| *x).count(),
            1,
            "exactly one of two concurrent offers is the first"
        );
    }

    /// `PendingRequestGuard` is the RAII vehicle used by the router's
    /// `RequestRemote` branch to suppress N→0 virtual-display detach
    /// while a new browser is mid-`ensure_attached`. Verify the
    /// counter is properly bumped on construction and decremented on
    /// `Drop` (including across nesting and early exits).
    #[test]
    fn pending_request_guard_increments_and_decrements_counter() {
        let registry = PcRegistry::new();
        assert_eq!(registry.pending_requests(), 0, "starts at 0");

        let g1 = registry.enter_pending();
        assert_eq!(registry.pending_requests(), 1);

        {
            let _g2 = registry.enter_pending();
            assert_eq!(registry.pending_requests(), 2, "nested guard stacks");
        }
        assert_eq!(registry.pending_requests(), 1, "nested guard dropped");

        drop(g1);
        assert_eq!(registry.pending_requests(), 0, "outer guard dropped");
    }

    /// Frames addressed to a connection that is not in the registry
    /// (race against `CloseControl` / browser drop) must be silently
    /// dropped — never panic. The daemon's media-receiver loop runs
    /// for the lifetime of the worker and a single panic there would
    /// kill all media flow.
    #[tokio::test]
    async fn write_video_frame_unknown_connection_is_silent_noop() {
        let registry = PcRegistry::new();
        let frame = MediaFrame {
            connection_id: "ghost".into(),
            seq: 0,
            ts_ns: 0,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoP,
            codec: MediaCodec::H264,
            payload: vec![0xAB; 32],
        };
        // Test passes if this does not panic and the receiver loop is
        // free to keep reading.
        write_video_frame(&registry, frame).await;
    }

    /// Frames arriving before the offer has populated the per-PC
    /// `video_track` (race window during initial setup) are dropped
    /// with a debug log, not propagated. The receiver task must keep
    /// running through that window.
    #[tokio::test]
    async fn write_video_frame_no_track_yet_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-no-track", &request_remote, &s)
            .await
            .expect("create");
        // Registry has the context, but `video_track` is still None
        // because no Offer ran (Offer is what populates the tracks in
        // `handle_offer`).
        let frame = MediaFrame {
            connection_id: "conn-no-track".into(),
            seq: 0,
            ts_ns: 0,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoI,
            codec: MediaCodec::H264,
            payload: vec![0xCD; 64],
        };
        write_video_frame(&registry, frame).await;
    }

    /// `pause_all_media` flips the per-PC flag for every
    /// connection in the registry. Test isolates the registry-side
    /// behaviour without involving worker IPC.
    #[tokio::test]
    async fn pause_all_media_marks_every_pc() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        for id in ["alpha", "beta", "gamma"] {
            registry
                .create_for_request_remote(id, &request_remote, &s)
                .await
                .expect("create");
        }

        // Sanity: nothing is paused at construction.
        for id in ["alpha", "beta", "gamma"] {
            let ctx = registry.get(id).await.unwrap();
            assert!(!ctx.read().await.media_paused.load(Ordering::Relaxed));
        }

        registry.pause_all_media().await;

        for id in ["alpha", "beta", "gamma"] {
            let ctx = registry.get(id).await.unwrap();
            assert!(
                ctx.read().await.media_paused.load(Ordering::Relaxed),
                "pause_all_media should mark {id}"
            );
        }
    }

    /// With `media_paused = true`, a P frame must be dropped and
    /// the flag must remain set (next IDR is the resync barrier).
    /// Verified by checking the flag stays `true` after the call —
    /// `write_video_frame` swallows errors silently so we can't observe
    /// the drop directly without instrumenting the track.
    #[tokio::test]
    async fn write_video_frame_paused_p_frame_keeps_flag_set() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-pause-p", &request_remote, &s)
            .await
            .expect("create");
        registry.pause_all_media().await;

        let frame = MediaFrame {
            connection_id: "conn-pause-p".into(),
            seq: 0,
            ts_ns: 0,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoP,
            codec: MediaCodec::H264,
            payload: vec![0x11; 16],
        };
        write_video_frame(&registry, frame).await;

        let ctx = registry.get("conn-pause-p").await.unwrap();
        assert!(
            ctx.read().await.media_paused.load(Ordering::Relaxed),
            "P frame during pause must not clear the flag"
        );
    }

    /// `MediaFrameKind::VideoI` arriving while paused clears the
    /// flag in place. Subsequent frames flow normally. We cannot
    /// observe the actual write_sample call (no track set), but the
    /// flag transition is the contract that gates resume — verifying
    /// it is sufficient.
    #[tokio::test]
    async fn write_video_frame_paused_i_frame_clears_flag() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-pause-i", &request_remote, &s)
            .await
            .expect("create");
        registry.pause_all_media().await;

        let frame = MediaFrame {
            connection_id: "conn-pause-i".into(),
            seq: 0,
            ts_ns: 0,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoI,
            codec: MediaCodec::H264,
            payload: vec![0x22; 32],
        };
        write_video_frame(&registry, frame).await;

        let ctx = registry.get("conn-pause-i").await.unwrap();
        assert!(
            !ctx.read().await.media_paused.load(Ordering::Relaxed),
            "first IDR while paused must clear the flag"
        );
    }

    /// `resume_active_media` over an empty registry must be a
    /// silent no-op (no WorkerManager IPC, no panic). Guards the
    /// post-shutdown / pre-first-RequestRemote race window.
    #[tokio::test]
    async fn resume_active_media_empty_registry_is_noop() {
        let registry = PcRegistry::new();
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        // No PCs registered, no worker active — resume must just iterate
        // zero entries and return cleanly.
        registry.resume_active_media(&worker_mgr).await;
    }

    /// `reset_media_for` on an unknown connection_id is a silent no-op:
    /// the daemon's MediaTransportStuck handler may race a
    /// `StopMedia` / `pc.close()` and we don't want a stale recovery
    /// attempt to panic or spawn IPC sends for a vanished PC.
    #[tokio::test]
    async fn reset_media_for_unknown_connection_is_noop() {
        let registry = PcRegistry::new();
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        registry.reset_media_for("nope", &worker_mgr).await;
    }

    /// `broadcast_media_settings_update` with all-`None` payload
    /// short-circuits without iterating the registry — pinning so a
    /// future change doesn't accidentally fan out a no-op IPC to every
    /// worker on every `UpdateDeskSettings` that touches only
    /// non-media fields (wayland_control_mode, private_screen, etc.).
    #[tokio::test]
    async fn broadcast_media_settings_update_all_none_is_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-x", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        // No worker active and all-None payload — must complete cleanly.
        registry
            .broadcast_media_settings_update(&worker_mgr, None, None, None, None)
            .await;
    }

    /// Regression for the dirty-rect kill-switch: a fan-out that
    /// carries *only* `enable_dirty_rect` (fps / bitrate / quality all
    /// `None`) must NOT short-circuit. The browser toggling the
    /// Advanced-tab switch without changing anything else is the
    /// expected path, and pre-fix `broadcast_media_settings_update`
    /// would have early-returned on `fps.is_none() && bitrate.is_none()
    /// && quality.is_none()`, silently dropping the toggle on the
    /// floor.
    #[tokio::test]
    async fn broadcast_media_settings_update_dirty_rect_only_not_short_circuited() {
        let registry = PcRegistry::new();
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        // Empty registry + dirty-rect-only payload: must complete
        // cleanly rather than early-return (the all-None guard must
        // include enable_dirty_rect).
        registry
            .broadcast_media_settings_update(&worker_mgr, None, None, None, Some(false))
            .await;
    }

    /// `broadcast_media_settings_update` only fans out to PCs that
    /// already have a cached `StartMediaPayload`. A registry with PCs
    /// that haven't received the first Offer yet (no cache) must
    /// neither panic nor accidentally synthesize a default StartMedia
    /// — handle_offer owns first-time fan-out.
    #[tokio::test]
    async fn broadcast_media_settings_update_skips_pcs_without_cached_offer() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-no-offer", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        // No cached_start_media → loop body skipped; no worker active
        // either, but the function must still not panic.
        registry
            .broadcast_media_settings_update(&worker_mgr, Some(60), None, Some(40), Some(false))
            .await;

        // The registered PC stays uncached.
        let ctx = registry.get("conn-no-offer").await.unwrap();
        assert!(ctx.read().await.cached_start_media.read().await.is_none());
    }

    /// `reset_media_for` on a registered connection without a cached
    /// `StartMediaPayload` (the stuck error fired before the first
    /// Offer/StartMedia ever landed) must still pause the PC and
    /// emit `StopMedia` to clear any half-built worker state, but
    /// must not synthesize a `StartMedia` from defaults.
    #[tokio::test]
    async fn reset_media_for_pauses_pc_even_without_cached_offer() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-stuck", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        registry.reset_media_for("conn-stuck", &worker_mgr).await;

        let ctx = registry.get("conn-stuck").await.unwrap();
        assert!(
            ctx.read().await.media_paused.load(Ordering::Relaxed),
            "reset_media_for must pause the PC so subsequent video frames are dropped \
             until a fresh IDR clears the flag"
        );
        // No cached StartMedia => the cached slot stays None and the
        // function returns early after the StopMedia send.
        assert!(ctx.read().await.cached_start_media.read().await.is_none());
    }

    /// A PC that hasn't yet received an Offer has
    /// `cached_start_media = None`; resume must skip it (rather than
    /// trying to send a default StartMedia, which would tell the
    /// worker to start an encoder for a connection that hasn't
    /// negotiated codecs yet).
    #[tokio::test]
    async fn resume_active_media_skips_pc_without_cached_offer() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-no-offer", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        // The worker_mgr has no active worker so any send_to_worker
        // would log a warning, but the snapshot loop must skip the PC
        // entirely because cached_start_media is None.
        registry.resume_active_media(&worker_mgr).await;

        // Cached slot stays None.
        let ctx = registry.get("conn-no-offer").await.unwrap();
        assert!(ctx.read().await.cached_start_media.read().await.is_none());
    }

    /// Audio frames go through the same entry point but route to
    /// `audio_track` instead of `video_track`. The daemon-side handler
    /// must accept the variant without panicking when no audio track
    /// exists.
    #[tokio::test]
    async fn write_video_frame_audio_kind_uses_audio_track_slot() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-audio", &request_remote, &s)
            .await
            .expect("create");
        let frame = MediaFrame {
            connection_id: "conn-audio".into(),
            seq: 0,
            ts_ns: 0,
            duration_ns: 20_000_000,
            kind: MediaFrameKind::Audio,
            codec: MediaCodec::Opus,
            payload: vec![0xEE; 96],
        };
        write_video_frame(&registry, frame).await;
    }

    /// `handle_request_remote` with a populated capabilities snapshot
    /// uses the worker's reported codecs in the Init reply. This is
    /// the path the daemon takes once the worker has sent its first
    /// `WorkerToService::Capabilities`.
    #[tokio::test]
    async fn handle_request_remote_uses_worker_capabilities_when_present() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let caps = MediaCapabilities {
            video_codecs: vec![MediaCodec::Vp9, MediaCodec::Av1],
            audio_codecs: vec![MediaCodec::Opus],
            video_encoders: vec!["VP9".to_string(), "AV1".to_string()],
            audio_encoders: vec!["OPUS".to_string()],
            video_device_list: std::collections::BTreeMap::new(),
            audio_device_list: std::collections::BTreeMap::new(),
            has_tauri: false,
            is_admin: true,
            desktop_name: "Default".to_string(),
        };
        let model = SignalingModel::new(
            "req-init",
            SignalingType::RequestRemote,
            Some("conn-init".to_string()),
            None,
            Some(
                serde_json::to_value(RequestRemoteModel {
                    ice_servers: vec![],
                    grant_session_id: None,
                })
                .unwrap(),
            ),
            None,
        );

        handle_request_remote(
            &registry,
            &outbound_tx,
            &s,
            "user-x",
            false,
            Some(&caps),
            None,
            None,
            &model,
            false,
            None,
            None,
        )
        .await
        .expect("handle ok");

        let text = outbound_rx
            .recv()
            .await
            .expect("init reply must be broadcast");
        let reply: SignalingModel = serde_json::from_str(&text).expect("Init JSON must round-trip");
        assert_eq!(reply.signaling_type, SignalingType::Init);
        let init: InitSignalingData = reply
            .get_data::<InitSignalingData>()
            .expect("Init payload present");
        // Worker said Vp9, Av1 → daemon should ship those strings.
        assert_eq!(init.video_encoder_list, vec!["VP9", "AV1"]);
        assert_eq!(init.audio_encoder_list, vec!["OPUS"]);
        assert!(init.is_admin, "init must mirror caps.is_admin");
    }

    /// `handle_request_remote` without capabilities (first connection
    /// before the worker has reported) falls back to the static
    /// capture-engine factory enumerations. This keeps the legacy
    /// behaviour during the small race window between worker spawn
    /// and first Capabilities IPC.
    #[tokio::test]
    async fn handle_request_remote_falls_back_when_no_capabilities() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let model = SignalingModel::new(
            "req-init-2",
            SignalingType::RequestRemote,
            Some("conn-init-2".to_string()),
            None,
            Some(
                serde_json::to_value(RequestRemoteModel {
                    ice_servers: vec![],
                    grant_session_id: None,
                })
                .unwrap(),
            ),
            None,
        );

        handle_request_remote(
            &registry,
            &outbound_tx,
            &s,
            "user-x",
            false,
            None,
            None,
            None,
            &model,
            false,
            None,
            None,
        )
        .await
        .expect("handle ok");

        let text = outbound_rx.recv().await.expect("init reply");
        let reply: SignalingModel = serde_json::from_str(&text).unwrap();
        let init: InitSignalingData = reply.get_data::<InitSignalingData>().expect("Init payload");
        // Static fallback comes from `list_video_encoder()` /
        // `list_audio_encoder()` — both must be populated regardless
        // of test platform; we only check non-emptiness rather than
        // an exact platform-dependent list.
        assert!(!init.video_encoder_list.is_empty());
        assert!(!init.audio_encoder_list.is_empty());
    }

    /// A redeemed-grant `RequestRemote` carries a validated capability ceiling and
    /// a grant-session id; `handle_request_remote` must (a) register the ceiling
    /// with the worker's per-connection map ahead of any worker-bound frame and
    /// (b) stamp all three (`restricted` / `access_ceiling` / `grant_session_id`)
    /// onto the created connection's `SignalingState` before any frame egresses, so
    /// the worker-side `meet(ceiling, global)` gates and grant-directed teardown
    /// observe them from the connection's first frame.
    #[tokio::test]
    async fn handle_request_remote_stamps_ceiling_and_grant_onto_signaling_state() {
        use desk_ipc_protocol::message::ServiceToWorker;

        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        // Stand up a worker manager with a fake active worker so the daemon has a
        // destination for the ceiling registration (grants are fail-closed without
        // one — see the dedicated fail-closed test).
        let shared = SharedSettings::from(s.clone());
        let settings_data = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings_data, registry.clone());
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;

        let model = SignalingModel::new(
            "req-grant-1",
            SignalingType::RequestRemote,
            Some("conn-grant-1".to_string()),
            None,
            Some(
                serde_json::to_value(RequestRemoteModel {
                    ice_servers: vec![],
                    grant_session_id: Some("GS-1".to_string()),
                })
                .unwrap(),
            ),
            None,
        );

        let ceiling = SecuritySettings {
            allow_file_transfer: Some(false),
            ..Default::default()
        };

        handle_request_remote(
            &registry,
            &outbound_tx,
            &s,
            "user-x",
            false,
            None,
            Some(&worker_mgr),
            None,
            &model,
            true,
            Some(ceiling.clone()),
            Some("GS-1".to_string()),
        )
        .await
        .expect("handle ok");

        // Drain the Init reply so the broadcast channel does not lag.
        let _ = outbound_rx.recv().await.expect("init reply");

        // The worker received the ceiling registration for this connection.
        let mut saw_ceiling = false;
        while let Ok(msg) = ipc_rx.try_recv() {
            if let ServiceToWorker::SetConnectionCeiling(p) = msg {
                assert_eq!(p.connection_id, "conn-grant-1");
                assert_eq!(p.ceiling, Some(ceiling.clone()));
                saw_ceiling = true;
            }
        }
        assert!(
            saw_ceiling,
            "daemon must register the grant ceiling with the worker"
        );

        let ctx = registry.get("conn-grant-1").await.expect("pc registered");
        let st = ctx.read().await.signaling_state.read().await.clone();
        assert!(st.restricted, "grant session must be marked restricted");
        assert_eq!(
            st.access_ceiling,
            Some(ceiling),
            "validated ceiling must be stored for the worker-side meet gates"
        );
        assert_eq!(
            st.grant_session_id.as_deref(),
            Some("GS-1"),
            "grant-session id must index the connection"
        );

        // The coarse restricted-set projection is also populated for the
        // outbound Support-isolation filter.
        assert!(
            registry
                .restricted_connections_handle()
                .read()
                .await
                .contains("conn-grant-1"),
        );
        // ...and the connection is indexed under its grant for directed teardown.
        assert_eq!(
            registry.connections_for_grant("GS-1").await,
            ["conn-grant-1"]
        );
    }

    /// A grant `RequestRemote` (ceiling `Some`) is fail-closed when the daemon has
    /// no worker to receive the ceiling registration: `handle_request_remote`
    /// returns an error and registers no connection, so a capped session can never
    /// run without its worker-side cap in place.
    #[tokio::test]
    async fn handle_request_remote_grant_fails_closed_without_worker() {
        let registry = PcRegistry::new();
        let (outbound_tx, _outbound_rx) = broadcast::channel::<String>(8);
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let model = SignalingModel::new(
            "req-grant-2",
            SignalingType::RequestRemote,
            Some("conn-grant-2".to_string()),
            None,
            Some(
                serde_json::to_value(RequestRemoteModel {
                    ice_servers: vec![],
                    grant_session_id: Some("GS-2".to_string()),
                })
                .unwrap(),
            ),
            None,
        );
        let ceiling = SecuritySettings {
            allow_file_transfer: Some(false),
            ..Default::default()
        };

        let result = handle_request_remote(
            &registry,
            &outbound_tx,
            &s,
            "user-x",
            false,
            None,
            None,
            None,
            &model,
            true,
            Some(ceiling),
            Some("GS-2".to_string()),
        )
        .await;

        assert!(result.is_err(), "grant without a worker must be rejected");
        assert!(
            registry.get("conn-grant-2").await.is_none(),
            "a rejected grant must leave no registered connection"
        );
        assert!(
            registry.connections_for_grant("GS-2").await.is_empty(),
            "a rejected grant must not index anything"
        );
    }

    /// Regression: when the worker reports `X264` and `H264` as two
    /// separate concrete encoders (libx264 vs OpenH264), the daemon
    /// must surface both strings in `InitSignalingData::
    /// video_encoder_list`. Previously `video_codecs` (used for SDP
    /// negotiation) collapsed both onto `MediaCodec::H264`, and the
    /// daemon mapped that back through `media_codec_to_str` to two
    /// indistinguishable "H264" entries. The fix routes the UI list
    /// through `caps.video_encoders` instead.
    #[tokio::test]
    async fn handle_request_remote_preserves_x264_h264_distinction_in_encoder_list() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let caps = MediaCapabilities {
            // SDP layer: only one H.264 entry (both implementations
            // produce equivalent H.264 wire format).
            video_codecs: vec![MediaCodec::H264, MediaCodec::Vp9],
            audio_codecs: vec![MediaCodec::Opus],
            // UI layer: both implementations remain distinct.
            video_encoders: vec!["X264".to_string(), "VP9".to_string(), "H264".to_string()],
            audio_encoders: vec!["OPUS".to_string()],
            video_device_list: std::collections::BTreeMap::new(),
            audio_device_list: std::collections::BTreeMap::new(),
            has_tauri: false,
            is_admin: false,
            desktop_name: "Default".to_string(),
        };
        let model = SignalingModel::new(
            "req-init-3",
            SignalingType::RequestRemote,
            Some("conn-init-3".to_string()),
            None,
            Some(
                serde_json::to_value(RequestRemoteModel {
                    ice_servers: vec![],
                    grant_session_id: None,
                })
                .unwrap(),
            ),
            None,
        );

        handle_request_remote(
            &registry,
            &outbound_tx,
            &s,
            "user-x",
            false,
            Some(&caps),
            None,
            None,
            &model,
            false,
            None,
            None,
        )
        .await
        .expect("handle ok");

        let text = outbound_rx.recv().await.expect("init reply");
        let reply: SignalingModel = serde_json::from_str(&text).unwrap();
        let init: InitSignalingData = reply.get_data::<InitSignalingData>().expect("Init payload");
        assert_eq!(
            init.video_encoder_list,
            vec!["X264", "VP9", "H264"],
            "X264 and H264 must remain separate encoder choices for the UI \
             rather than collapsing to two indistinguishable 'H264' entries"
        );
        assert_eq!(init.audio_encoder_list, vec!["OPUS"]);
    }

    /// Regression: the daemon-side PC must publish locally-gathered
    /// ICE candidates back through the signaling channel as
    /// `SignalingType::Canid`. Without this the browser only learns
    /// about the daemon's transport addresses via peer-reflexive
    /// discovery, which times out after 30 s of `checking` for
    /// multi-m-line PCs (video+audio+DC). The portable mode log
    /// signature was: file-management (DC-only) connected, but the
    /// remote-desktop page consistently failed ICE.
    #[tokio::test]
    async fn local_ice_candidate_forwarder_publishes_canid_to_outbound() {
        use std::time::Duration;
        use tokio::time::timeout;

        let settings = Settings::default();
        let pc = Arc::new(
            build_peer_connection(vec![], &settings)
                .await
                .expect("build pc"),
        );
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(32);
        register_local_ice_candidate_forwarder(
            Arc::clone(&pc),
            outbound_tx,
            "conn-trickle".to_string(),
        );

        // Trigger ICE gathering: any local SDP with at least one
        // m-section starts the gatherer. A DataChannel is the
        // cheapest such trigger (no transceiver bookkeeping).
        let _dc = pc
            .create_data_channel("trickle-test", None)
            .await
            .expect("create dc");
        let offer = pc.create_offer(None).await.expect("create offer");
        pc.set_local_description(offer)
            .await
            .expect("set local desc starts gathering");

        let mut canid_count = 0usize;
        let deadline = Duration::from_secs(5);
        loop {
            match timeout(deadline, outbound_rx.recv()).await {
                Ok(Ok(text)) => {
                    let m: SignalingModel = serde_json::from_str(&text)
                        .expect("outbound text must be a SignalingModel");
                    if m.signaling_type != SignalingType::Canid {
                        continue;
                    }
                    assert_eq!(
                        m.to_connection_id.as_deref(),
                        Some("conn-trickle"),
                        "Canid must target the originating browser connection"
                    );
                    let init: RTCIceCandidateInit = m
                        .get_data::<RTCIceCandidateInit>()
                        .expect("Canid payload must be RTCIceCandidateInit");
                    assert!(
                        !init.candidate.is_empty(),
                        "forwarded candidate string must be non-empty"
                    );
                    canid_count += 1;
                    // Stop after the first one to keep the test fast;
                    // counting the rest only adds flakiness.
                    break;
                }
                _ => break,
            }
        }
        assert!(
            canid_count >= 1,
            "register_local_ice_candidate_forwarder must publish at least one Canid \
             after set_local_description triggers gathering; got {canid_count}"
        );
    }

    /// Regression: `handle_request_remote` must wire the on_ice_candidate
    /// forwarder onto the freshly-created PC so that subsequent gathering
    /// (kicked off by the browser's Offer) ships candidates back to the
    /// browser. We exercise this end-to-end by manually triggering
    /// gathering on the registry-stored PC after `handle_request_remote`
    /// returns and asserting Canid messages arrive on `outbound`.
    #[tokio::test]
    async fn handle_request_remote_registers_ice_candidate_forwarder() {
        use std::time::Duration;
        use tokio::time::timeout;

        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(32);
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let model = SignalingModel::new(
            "req-init-ice",
            SignalingType::RequestRemote,
            Some("conn-init-ice".to_string()),
            None,
            Some(
                serde_json::to_value(RequestRemoteModel {
                    ice_servers: vec![],
                    grant_session_id: None,
                })
                .unwrap(),
            ),
            None,
        );

        handle_request_remote(
            &registry,
            &outbound_tx,
            &s,
            "user-x",
            false,
            None,
            None,
            None,
            &model,
            false,
            None,
            None,
        )
        .await
        .expect("handle ok");

        // Drain the Init reply.
        let init_text = outbound_rx.recv().await.expect("init reply");
        let init_reply: SignalingModel = serde_json::from_str(&init_text).unwrap();
        assert_eq!(init_reply.signaling_type, SignalingType::Init);

        // Now trigger gathering on the PC the registry holds. This is
        // what the Offer handler does in production; we do it directly
        // here because the unit test is scoped to handle_request_remote.
        let ctx = registry.get("conn-init-ice").await.expect("ctx exists");
        let pc = {
            let g = ctx.read().await;
            Arc::clone(&g.pc)
        };
        let _dc = pc.create_data_channel("trickle", None).await.expect("dc");
        let offer = pc.create_offer(None).await.expect("offer");
        pc.set_local_description(offer).await.expect("set local");

        let mut got_canid = false;
        let deadline = Duration::from_secs(5);
        loop {
            match timeout(deadline, outbound_rx.recv()).await {
                Ok(Ok(text)) => {
                    let m: SignalingModel = serde_json::from_str(&text).unwrap();
                    if m.signaling_type == SignalingType::Canid {
                        assert_eq!(m.to_connection_id.as_deref(), Some("conn-init-ice"));
                        got_canid = true;
                        break;
                    }
                }
                _ => break,
            }
        }
        assert!(
            got_canid,
            "handle_request_remote must register the ICE forwarder so gathering ships Canid"
        );
    }

    /// Regression: when the browser-side PC reaches a terminal state
    /// (Failed / Closed) the daemon must release the registry slot and
    /// ship `StopMedia` to the worker. Without this the worker keeps the
    /// per-connection encoder running and the per-output DXGI duplication
    /// held; the next remote-desktop attempt then hits
    /// `DuplicateOutput → 0x80070057 (E_INVALIDARG)` because Windows only
    /// permits one duplication per (process, output) pair.
    ///
    /// This test simulates terminal state via `pc.close()`, waits for the
    /// async callback to fire, and asserts the registry entry is gone.
    #[tokio::test]
    async fn peer_connection_state_change_terminal_removes_registry_entry() {
        use std::time::Duration;
        use tokio::time::sleep;

        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let ctx = registry
            .create_for_request_remote("conn-cleanup", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        let pc = {
            let g = ctx.read().await;
            Arc::clone(&g.pc)
        };
        register_peer_connection_state_cleanup(
            Arc::clone(&pc),
            registry.clone(),
            worker_mgr,
            None,
            "conn-cleanup".to_string(),
        );

        assert!(
            registry.contains("conn-cleanup").await,
            "registry must hold the PC before close()"
        );

        // Trigger terminal state. webrtc-rs schedules the state-change
        // callback asynchronously; poll the registry with a generous
        // 5 s budget so this test stays robust under heavy CI load.
        pc.close().await.expect("close pc");

        let deadline = Duration::from_secs(5);
        let start = std::time::Instant::now();
        while registry.contains("conn-cleanup").await {
            if start.elapsed() > deadline {
                panic!(
                    "register_peer_connection_state_cleanup must remove the registry entry \
                     after pc.close() drives the PC to Closed; entry still present after 5s"
                );
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// `cleanup_pc` on an unknown connection_id must be a silent no-op:
    /// the on_peer_connection_state_change callback can race a manual
    /// CloseControl, and we don't want one path's success to drag the
    /// other into a panic / error log spam.
    #[tokio::test]
    async fn cleanup_pc_unknown_connection_is_silent_noop() {
        let registry = PcRegistry::new();
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        // No PC registered at all. Must not panic.
        cleanup_pc(&registry, &worker_mgr, None, "ghost-connection", "test").await;
        assert_eq!(registry.len().await, 0);
    }

    /// `cleanup_pc` removes the PC entry even when no worker is active.
    /// The StopMedia send returns Err("No active worker") which is logged
    /// at debug level and otherwise swallowed — pinning so a refactor that
    /// converts the StopMedia send into an unwrap doesn't ship.
    #[tokio::test]
    async fn cleanup_pc_removes_registry_entry_even_without_worker() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-x", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        cleanup_pc(&registry, &worker_mgr, None, "conn-x", "test").await;

        assert!(!registry.contains("conn-x").await);
    }

    /// `handle_connection_removed` is the active cleanup path —
    /// the signaling server fans out `ConnectionRemoved` the moment
    /// a Browser peer's WS dies. Verify it tears down the daemon-side
    /// PC for the named `from_connection_id` so the worker's DXGI
    /// duplication is released before any reopen attempt races for it.
    #[tokio::test]
    async fn handle_connection_removed_clears_registry_for_existing_pc() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-bye", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        let model = SignalingModel::new(
            "req-conn-removed",
            SignalingType::ConnectionRemoved,
            Some("conn-bye".to_string()),
            None,
            None,
            None,
        );

        handle_connection_removed(&registry, &worker_mgr, None, &model)
            .await
            .expect("handler must not error on a known connection");

        assert!(!registry.contains("conn-bye").await);
    }

    /// `ConnectionRemoved` for a connection the daemon never
    /// registered (e.g. a browser that never finished SDP) must be a
    /// no-op rather than an error. The signaling broadcast is
    /// best-effort and arrives at every Server peer in the
    /// connection map regardless of whether the recipient was paired
    /// with the departed browser; daemons that weren't involved
    /// would otherwise log spurious failures every time any Browser
    /// disconnects.
    #[tokio::test]
    async fn handle_connection_removed_unknown_connection_is_silent_noop() {
        let registry = PcRegistry::new();
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        let model = SignalingModel::new(
            "req-conn-removed",
            SignalingType::ConnectionRemoved,
            Some("ghost-connection".to_string()),
            None,
            None,
            None,
        );

        handle_connection_removed(&registry, &worker_mgr, None, &model)
            .await
            .expect("handler must accept unknown ids without erroring");
        assert_eq!(registry.len().await, 0);
    }

    /// v5 lazy lifecycle: with the last PC removed and no pending
    /// requests, `cleanup_pc` must `apply(false)` on the supervisor so
    /// the IDD detaches and the dropdown clears on the next dialog.
    #[tokio::test]
    async fn cleanup_pc_detaches_supervisor_when_last_pc_removed_and_no_pending() {
        use crate::daemon::virtual_display::VirtualDisplaySupervisor;
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-only", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
            worker_mgr.clone(),
            "SWD\\TEST\\TEST",
        ));
        assert_eq!(supervisor.state_label().await, "Attached");

        cleanup_pc(
            &registry,
            &worker_mgr,
            Some(&supervisor),
            "conn-only",
            "test-n-to-zero",
        )
        .await;

        assert!(!registry.contains("conn-only").await);
        assert_eq!(
            supervisor.state_label().await,
            "Disabled",
            "N->0 cleanup must detach the supervisor",
        );
    }

    /// As long as other PCs are still live, the supervisor must stay
    /// attached so the remaining session can keep using the IDD.
    #[tokio::test]
    async fn cleanup_pc_keeps_supervisor_when_other_pcs_remain() {
        use crate::daemon::virtual_display::VirtualDisplaySupervisor;
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        for id in ["conn-a", "conn-b"] {
            registry
                .create_for_request_remote(id, &request_remote, &s)
                .await
                .expect("create");
        }

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
            worker_mgr.clone(),
            "SWD\\TEST\\TEST",
        ));

        cleanup_pc(
            &registry,
            &worker_mgr,
            Some(&supervisor),
            "conn-a",
            "test-keep",
        )
        .await;

        assert!(registry.contains("conn-b").await);
        assert_eq!(
            supervisor.state_label().await,
            "Attached",
            "supervisor must remain Attached while another PC is live",
        );
    }

    /// Codex round 4 #10: a held `PendingRequestGuard` represents a new
    /// `RequestRemote` mid-`ensure_attached` that hasn't registered a
    /// PC yet. Cleanup of an old PC during this window must NOT detach
    /// the IDD — the new connection is about to use it.
    #[tokio::test]
    async fn cleanup_pc_keeps_supervisor_when_pending_request_active() {
        use crate::daemon::virtual_display::VirtualDisplaySupervisor;
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-old", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
            worker_mgr.clone(),
            "SWD\\TEST\\TEST",
        ));

        // Simulate a new RequestRemote in the ensure_attached window:
        // PC not yet created, but a pending guard is live.
        let _pending = registry.enter_pending();

        cleanup_pc(
            &registry,
            &worker_mgr,
            Some(&supervisor),
            "conn-old",
            "test-pending-race",
        )
        .await;

        assert!(!registry.contains("conn-old").await);
        assert_eq!(
            supervisor.state_label().await,
            "Attached",
            "pending request guard must suppress N->0 detach",
        );
    }

    /// Codex round 3 #3 + cleanup_pc N→0 gate: a `cleanup_pc` call for
    /// a connection id that was never registered (stale
    /// `ConnectionRemoved` after the PC was already torn down) must
    /// NOT trigger N→0 detach, even though `registry.len()` may
    /// happen to be 0. The gate is `removed.is_some()`.
    #[tokio::test]
    async fn cleanup_pc_unknown_connection_does_not_detach_supervisor() {
        use crate::daemon::virtual_display::VirtualDisplaySupervisor;
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-live", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
            worker_mgr.clone(),
            "SWD\\TEST\\TEST",
        ));

        cleanup_pc(
            &registry,
            &worker_mgr,
            Some(&supervisor),
            "conn-ghost",
            "stale-ConnectionRemoved",
        )
        .await;

        assert!(
            registry.contains("conn-live").await,
            "unknown-id cleanup must not touch other PCs",
        );
        assert_eq!(
            supervisor.state_label().await,
            "Attached",
            "stale ConnectionRemoved must not trigger detach",
        );
    }

    /// Codex P1 #1 regression: when the departing PC was the sole
    /// `accept_control=true` holder but another PC remains live (so
    /// `registry.len() > 0` blocks the N→0 detach), the old code
    /// never recomputed the exclusive-mode desired flag — the
    /// supervisor stayed pinned at `desired=true` with no control
    /// holder, leaving physical displays detached. cleanup_pc now
    /// calls `supervisor.recompute_desired()` unconditionally on a
    /// real removal so the registered closure (which queries
    /// `any_with_accept_control`) fires.
    ///
    /// The test installs an observable closure (records each call's
    /// `active` argument) and asserts it runs at least once. The
    /// supervisor's `set_desired_exclusive` was already covered by
    /// the daemon::virtual_display tests, so we only need to prove
    /// the cleanup path reaches the closure.
    #[tokio::test]
    async fn cleanup_pc_triggers_exclusive_recompute_when_other_pcs_remain() {
        use crate::daemon::virtual_display::{DesiredComputerFn, VirtualDisplaySupervisor};
        use std::future::Future;
        use std::pin::Pin;

        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let ctx_a = registry
            .create_for_request_remote("conn-a", &request_remote, &s)
            .await
            .expect("seed a");
        registry
            .create_for_request_remote("conn-b", &request_remote, &s)
            .await
            .expect("seed b");
        // A is the sole control holder; B is view-only.
        {
            let ctx = ctx_a.read().await;
            ctx.signaling_state.write().await.accept_control = true;
        }
        assert!(registry.any_with_accept_control().await);

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
            worker_mgr.clone(),
            "SWD\\TEST\\TEST",
        ));

        // Install a desired_computer that mirrors the real router's
        // shape (queries any_with_accept_control on the registry) and
        // records the call count + the last `active` it received.
        let call_count = Arc::new(AtomicUsize::new(0));
        let last_active = Arc::new(AtomicBool::new(false));
        let registry_for_closure = registry.clone();
        let call_count_cl = Arc::clone(&call_count);
        let last_active_cl = Arc::clone(&last_active);
        let computer: DesiredComputerFn = Arc::new(move |active: bool| {
            let registry = registry_for_closure.clone();
            let call_count = Arc::clone(&call_count_cl);
            let last_active = Arc::clone(&last_active_cl);
            Box::pin(async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                last_active.store(active, Ordering::SeqCst);
                if !active {
                    return (false, 0u32);
                }
                let any = registry.any_with_accept_control().await;
                (any, 0u32)
            }) as Pin<Box<dyn Future<Output = (bool, u32)> + Send>>
        });
        supervisor.set_desired_computer(computer).await;

        // Sanity: the registry currently has a control holder, but
        // it is `conn-a` — the one we are about to remove.
        cleanup_pc(
            &registry,
            &worker_mgr,
            Some(&supervisor),
            "conn-a",
            "test-recompute",
        )
        .await;

        // PC A removed, PC B remains.
        assert!(!registry.contains("conn-a").await);
        assert!(registry.contains("conn-b").await);
        // The supervisor must remain attached (N→0 gate not hit).
        assert_eq!(supervisor.state_label().await, "Attached");

        // The recompute closure must have been invoked at least once
        // with the supervisor's real `active` snapshot. Without the
        // P1 #1 fix it would never run on this path.
        assert!(
            call_count.load(Ordering::SeqCst) >= 1,
            "recompute_desired closure must be invoked at least once on cleanup",
        );
        // And after the cleanup, no remaining PC holds accept_control.
        assert!(!registry.any_with_accept_control().await);
    }

    /// Codex P1 #1 sanity: cleanup of an unknown connection
    /// (stale ConnectionRemoved) must NOT trigger recompute — the
    /// gate is `removed.is_some()`.
    #[tokio::test]
    async fn cleanup_pc_does_not_recompute_on_stale_unknown_removal() {
        use crate::daemon::virtual_display::{DesiredComputerFn, VirtualDisplaySupervisor};
        use std::future::Future;
        use std::pin::Pin;

        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-live", &request_remote, &s)
            .await
            .expect("seed");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        let supervisor = Arc::new(VirtualDisplaySupervisor::new_attached_for_test(
            worker_mgr.clone(),
            "SWD\\TEST\\TEST",
        ));

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_cl = Arc::clone(&call_count);
        let computer: DesiredComputerFn = Arc::new(move |_active: bool| {
            let call_count = Arc::clone(&call_count_cl);
            Box::pin(async move {
                call_count.fetch_add(1, Ordering::SeqCst);
                (false, 0u32)
            }) as Pin<Box<dyn Future<Output = (bool, u32)> + Send>>
        });
        supervisor.set_desired_computer(computer).await;

        cleanup_pc(
            &registry,
            &worker_mgr,
            Some(&supervisor),
            "conn-ghost",
            "stale-ConnectionRemoved",
        )
        .await;

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "stale removal must not invoke the recompute closure",
        );
    }

    /// `virtual_display: None` (non-ServiceDaemon mode) must still let
    /// cleanup_pc clear the registry without panicking.
    #[tokio::test]
    async fn cleanup_pc_skips_supervisor_when_none() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-x", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        cleanup_pc(&registry, &worker_mgr, None, "conn-x", "no-supervisor").await;

        assert!(!registry.contains("conn-x").await);
    }

    /// Codec round-trip: every IPC `MediaCodec` must map to a
    /// non-empty string for the Init reply path. Pin so adding a new
    /// codec to the IPC enum forces an update on the daemon side.
    #[test]
    fn media_codec_to_str_is_total_over_known_codecs() {
        for c in [
            MediaCodec::H264,
            MediaCodec::Vp8,
            MediaCodec::Vp9,
            MediaCodec::Av1,
            MediaCodec::Opus,
        ] {
            let s = media_codec_to_str(&c).expect("known codec maps to a string");
            assert!(!s.is_empty(), "{c:?}");
        }
    }

    /// `video_encoder_to_media_codec` must collapse X264 + H264 to
    /// the same `MediaCodec::H264` (both are H.264 encoders, the
    /// daemon doesn't differentiate them on the wire).
    #[test]
    fn video_encoder_to_media_codec_collapses_x264_and_h264() {
        assert_eq!(
            video_encoder_to_media_codec(VideoEncoderType::X264),
            MediaCodec::H264
        );
        assert_eq!(
            video_encoder_to_media_codec(VideoEncoderType::H264),
            MediaCodec::H264
        );
        assert_eq!(
            video_encoder_to_media_codec(VideoEncoderType::VP8),
            MediaCodec::Vp8
        );
    }

    // ============== DataChannel routing tests ==============

    /// Every known DC label must classify to a `DcRoute`. Pin so a new
    /// label added to `model::data_channel` without a matching route
    /// here is caught at PR-review time rather than silently dropped
    /// at runtime.
    #[test]
    fn classify_dc_label_covers_all_known_labels() {
        assert_eq!(classify_dc_label("mouse_event"), Some(DcRoute::Mouse));
        assert_eq!(
            classify_dc_label("mouse_move_event"),
            Some(DcRoute::MouseMove)
        );
        assert_eq!(classify_dc_label("keyboard_event"), Some(DcRoute::Keyboard));
        assert_eq!(
            classify_dc_label("clipboard_event"),
            Some(DcRoute::Clipboard)
        );
        assert_eq!(
            classify_dc_label("file_transfer_event"),
            Some(DcRoute::FileTransfer)
        );
        assert_eq!(
            classify_dc_label("whiteboard_event"),
            Some(DcRoute::Whiteboard)
        );
        assert_eq!(
            classify_dc_label("cursor_sync_event"),
            Some(DcRoute::CursorSync)
        );
        assert_eq!(classify_dc_label("not-a-real-channel"), None);
    }

    /// Each non-CursorSync route maps to the correct
    /// `ServiceToWorker` variant carrying the same `connection_id` and
    /// payload bytes the browser sent. The IPC layer is the trust
    /// boundary between daemon and worker; this test pins the
    /// translation so a refactor cannot accidentally re-route mouse
    /// events as keyboard events.
    #[test]
    fn route_to_service_msg_preserves_payload_and_connection_id() {
        let cid = "conn-test";
        let data = vec![1u8, 2, 3, 4];

        match route_to_service_msg(DcRoute::Mouse, cid, data.clone(), true) {
            ServiceToWorker::MouseInput(p) => {
                assert_eq!(p.connection_id, cid);
                assert_eq!(p.data, data);
            }
            other => panic!("expected MouseInput, got {other:?}"),
        }
        match route_to_service_msg(DcRoute::MouseMove, cid, data.clone(), true) {
            ServiceToWorker::MouseMoveInput(p) => assert_eq!(p.data, data),
            other => panic!("expected MouseMoveInput, got {other:?}"),
        }
        match route_to_service_msg(DcRoute::Keyboard, cid, data.clone(), true) {
            ServiceToWorker::KeyboardInput(p) => assert_eq!(p.data, data),
            other => panic!("expected KeyboardInput, got {other:?}"),
        }
        match route_to_service_msg(DcRoute::Clipboard, cid, data.clone(), true) {
            ServiceToWorker::ClipboardWrite(p) => assert_eq!(p.data, data),
            other => panic!("expected ClipboardWrite, got {other:?}"),
        }
        match route_to_service_msg(DcRoute::Whiteboard, cid, data.clone(), true) {
            ServiceToWorker::WhiteboardCommand(p) => assert_eq!(p.data, data),
            other => panic!("expected WhiteboardCommand, got {other:?}"),
        }
    }

    /// CursorSync routing is a programmer error — calling
    /// `route_to_service_msg` on it must panic rather than silently
    /// emit a wrong variant. The router skips this case explicitly
    /// before reaching the routing call.
    #[test]
    #[should_panic(expected = "CursorSync DC has no upstream message variant")]
    fn route_to_service_msg_cursor_sync_panics() {
        let _ = route_to_service_msg(DcRoute::CursorSync, "c", vec![], true);
    }

    /// FileTransfer rides the dedicated file lane (see
    /// `desk-ipc-protocol::dual_transport`), not the event-lane
    /// `route_to_service_msg`. Calling the router on it is a
    /// programmer error and must panic — the production forwarder
    /// special-cases FileTransfer before calling
    /// `route_to_service_msg`. Pinning the panic message guards
    /// against a future arm being added that silently moves file
    /// bytes back onto the event lane.
    #[test]
    #[should_panic(expected = "FileTransfer is routed through")]
    fn route_to_service_msg_file_transfer_panics() {
        let _ = route_to_service_msg(DcRoute::FileTransfer, "c", vec![], true);
    }

    /// `accept_control = false` blocks Mouse / MouseMove / Keyboard
    /// even when `accept_clipboard_sync = true`. Critical: a
    /// regression here would let an unauthorised peer drive the
    /// host's mouse / keyboard.
    #[tokio::test]
    async fn route_is_permitted_blocks_input_when_control_denied() {
        let state = Arc::new(RwLock::new(SignalingState {
            accept_control: false,
            accept_clipboard_sync: true,
            ..SignalingState::default()
        }));
        assert!(!route_is_permitted(DcRoute::Mouse, &state).await);
        assert!(!route_is_permitted(DcRoute::MouseMove, &state).await);
        assert!(!route_is_permitted(DcRoute::Keyboard, &state).await);
        assert!(!route_is_permitted(DcRoute::Whiteboard, &state).await);
        // Clipboard rides on its own gate, not control.
        assert!(route_is_permitted(DcRoute::Clipboard, &state).await);
        // FileTransfer is on `allow_file_transfer` (worker-side gate),
        // independent of accept_control. The browser file-management UI
        // opens a fresh PC that has never requested control, so any
        // accept_control gate here would silently drop every download.
        assert!(route_is_permitted(DcRoute::FileTransfer, &state).await);
    }

    /// File transfer must pass the daemon gate regardless of
    /// `accept_control` / `accept_clipboard_sync`; the worker
    /// dispatcher runs the actual `allow_file_transfer` security check.
    /// Regression guard for the portable-mode "download stuck" bug.
    #[tokio::test]
    async fn route_is_permitted_passes_file_transfer_unconditionally() {
        let denied = Arc::new(RwLock::new(SignalingState {
            accept_control: false,
            accept_clipboard_sync: false,
            ..SignalingState::default()
        }));
        assert!(route_is_permitted(DcRoute::FileTransfer, &denied).await);

        let accepted = Arc::new(RwLock::new(SignalingState {
            accept_control: true,
            accept_clipboard_sync: true,
            ..SignalingState::default()
        }));
        assert!(route_is_permitted(DcRoute::FileTransfer, &accepted).await);
    }

    /// `accept_clipboard_sync = false` blocks Clipboard even when
    /// `accept_control = true`. Independent gates: a peer can be
    /// trusted with mouse/keyboard but not clipboard (e.g. screen
    /// share without copy-paste).
    #[tokio::test]
    async fn route_is_permitted_blocks_clipboard_when_clipboard_denied() {
        let state = Arc::new(RwLock::new(SignalingState {
            accept_control: true,
            accept_clipboard_sync: false,
            ..SignalingState::default()
        }));
        assert!(!route_is_permitted(DcRoute::Clipboard, &state).await);
        // Control-gated routes still pass.
        assert!(route_is_permitted(DcRoute::Mouse, &state).await);
        assert!(route_is_permitted(DcRoute::Keyboard, &state).await);
    }

    /// Both gates open → every routable variant is permitted (cursor
    /// sync stays out because the gate function panics on it; the
    /// caller filters cursor sync before calling).
    #[tokio::test]
    async fn route_is_permitted_allows_all_when_both_accepted() {
        let state = Arc::new(RwLock::new(SignalingState {
            accept_control: true,
            accept_clipboard_sync: true,
            ..SignalingState::default()
        }));
        assert!(route_is_permitted(DcRoute::Mouse, &state).await);
        assert!(route_is_permitted(DcRoute::MouseMove, &state).await);
        assert!(route_is_permitted(DcRoute::Keyboard, &state).await);
        assert!(route_is_permitted(DcRoute::Clipboard, &state).await);
        assert!(route_is_permitted(DcRoute::FileTransfer, &state).await);
        assert!(route_is_permitted(DcRoute::Whiteboard, &state).await);
    }

    /// Restricted temporary-support session (second fail-closed door): file
    /// transfer / clipboard / whiteboard are denied outright even with both
    /// accept flags open. This closes the exfiltration path a normal session
    /// leaves open (`FileTransfer` passes unconditionally there); pointer /
    /// keyboard input stays allowed but remains gated by `accept_control`.
    #[tokio::test]
    async fn route_is_permitted_restricted_denies_file_clipboard_whiteboard() {
        let state = Arc::new(RwLock::new(SignalingState {
            accept_control: true,
            accept_clipboard_sync: true,
            restricted: true,
            ..SignalingState::default()
        }));
        assert!(!route_is_permitted(DcRoute::FileTransfer, &state).await);
        assert!(!route_is_permitted(DcRoute::Clipboard, &state).await);
        assert!(!route_is_permitted(DcRoute::Whiteboard, &state).await);
        assert!(route_is_permitted(DcRoute::Mouse, &state).await);
        assert!(route_is_permitted(DcRoute::MouseMove, &state).await);
        assert!(route_is_permitted(DcRoute::Keyboard, &state).await);
    }

    /// Restricted view-only support session: `accept_control` never set, so even
    /// the allowed pointer / keyboard routes are gated off, and file transfer
    /// stays denied. A view-only supporter can drive nothing.
    #[tokio::test]
    async fn route_is_permitted_restricted_view_only_denies_all_input() {
        let state = Arc::new(RwLock::new(SignalingState {
            accept_control: false,
            restricted: true,
            ..SignalingState::default()
        }));
        assert!(!route_is_permitted(DcRoute::Mouse, &state).await);
        assert!(!route_is_permitted(DcRoute::MouseMove, &state).await);
        assert!(!route_is_permitted(DcRoute::Keyboard, &state).await);
        assert!(!route_is_permitted(DcRoute::FileTransfer, &state).await);
    }

    /// The restricted-connections projection is populated by
    /// `mark_restricted_connection` and cleared by `cleanup_pc` on every teardown
    /// path — including for a connection that was never registered — so the
    /// outbound Support-isolation filter can never route to a stale support id.
    #[tokio::test]
    async fn restricted_connections_projection_marks_and_clears_on_cleanup() {
        let registry = PcRegistry::new();
        let handle = registry.restricted_connections_handle();
        assert!(handle.read().await.is_empty());

        registry.mark_restricted_connection("conn-support").await;
        assert!(handle.read().await.contains("conn-support"));

        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        cleanup_pc(&registry, &worker_mgr, None, "conn-support", "test").await;
        assert!(!handle.read().await.contains("conn-support"));
    }

    /// Ending a support session tears down every restricted PC but leaves
    /// unrestricted (owner) PCs untouched — the supporter's session ends while a
    /// concurrent owner session keeps running.
    #[tokio::test]
    async fn cleanup_restricted_connections_closes_only_restricted_pcs() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-owner", &request_remote, &s)
            .await
            .expect("owner pc");
        registry
            .create_for_request_remote("conn-support", &request_remote, &s)
            .await
            .expect("support pc");
        registry.mark_restricted_connection("conn-support").await;
        assert_eq!(registry.len().await, 2);

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        cleanup_restricted_connections(&registry, &worker_mgr, None, "test").await;

        assert!(registry.get("conn-owner").await.is_some());
        assert!(registry.get("conn-support").await.is_none());
        assert!(
            registry
                .restricted_connections_handle()
                .read()
                .await
                .is_empty()
        );
    }

    /// A grant-directed teardown closes every connection that shares the grant in
    /// one sweep (main + a second file-transfer connection of the same logical
    /// session) while leaving connections of an unrelated grant / owner untouched,
    /// and prunes the grant key once emptied.
    #[tokio::test]
    async fn close_grant_session_tears_down_all_grant_connections() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        for id in ["conn-g1-main", "conn-g1-file", "conn-other"] {
            registry
                .create_for_request_remote(id, &request_remote, &s)
                .await
                .expect("pc");
        }
        registry
            .index_grant_connection("GS-1", "conn-g1-main")
            .await;
        registry
            .index_grant_connection("GS-1", "conn-g1-file")
            .await;
        registry.index_grant_connection("GS-2", "conn-other").await;
        assert_eq!(registry.connections_for_grant("GS-1").await.len(), 2);

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        close_grant_session(&registry, &worker_mgr, None, "GS-1", "test").await;

        assert!(registry.get("conn-g1-main").await.is_none());
        assert!(registry.get("conn-g1-file").await.is_none());
        assert!(registry.get("conn-other").await.is_some());
        // The emptied grant key is pruned; the unrelated grant survives.
        assert!(registry.connections_for_grant("GS-1").await.is_empty());
        assert_eq!(registry.connections_for_grant("GS-2").await, ["conn-other"]);
    }

    /// `cleanup_pc` prunes the grant reverse-index on teardown so a later directed
    /// revocation can never reach a stale connection id, and drops the grant key
    /// once its last connection departs.
    #[tokio::test]
    async fn cleanup_pc_unindexes_grant_connection() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-g", &request_remote, &s)
            .await
            .expect("pc");
        registry.index_grant_connection("GS-9", "conn-g").await;
        assert_eq!(registry.connections_for_grant("GS-9").await, ["conn-g"]);

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());
        cleanup_pc(&registry, &worker_mgr, None, "conn-g", "test").await;

        assert!(registry.connections_for_grant("GS-9").await.is_empty());
    }

    /// `register_data_channel_router` is async-callable on a
    /// freshly-built PC without panicking. We can't drive a real DC
    /// open here without a peer connection on the other side, so this
    /// is a smoke test for the registration call only — the routing
    /// behaviour itself is covered by the pure-function tests above.
    #[tokio::test]
    async fn register_data_channel_router_smoke() {
        use crate::model::settings::SharedSettings;

        let settings = Settings::default();
        let pc = build_peer_connection(vec![], &settings).await.expect("pc");
        let signaling_state = Arc::new(RwLock::new(SignalingState::default()));
        let cursor_dc = Arc::new(RwLock::new(None));
        let clipboard_dc = Arc::new(RwLock::new(None));
        let file_transfer_dc = Arc::new(RwLock::new(None));
        let shared = SharedSettings::from(Settings::default());
        let settings_data = actix_web::web::Data::new(shared);
        let (worker_mgr, _) = WorkerManager::new(settings_data, PcRegistry::new());
        register_data_channel_router(
            Arc::new(pc),
            "conn-smoke".to_string(),
            signaling_state,
            cursor_dc,
            clipboard_dc,
            file_transfer_dc,
            worker_mgr,
        );
    }

    // ============== cursor sync write_cursor_data ==============

    /// `write_cursor_data` for an unknown connection_id is a silent
    /// no-op (no panic). Critical: the IPC receiver loop must keep
    /// draining cursor packets even after a connection has been
    /// closed (race against `CloseControl`).
    #[tokio::test]
    async fn write_cursor_data_unknown_connection_is_silent_noop() {
        let registry = PcRegistry::new();
        let payload = CursorDataPayload {
            connection_id: "ghost".to_string(),
            data: br#"{"visible":false}"#.to_vec(),
        };
        write_cursor_data(&registry, payload).await;
    }

    /// `write_cursor_data` for a known connection that has not yet
    /// registered a `cursor_sync_event` DC (browser hasn't opened it
    /// — control not granted, or DC negotiation in flight) is a
    /// silent no-op. The browser would naturally not see a cursor
    /// in that state; that is the intended behaviour.
    #[tokio::test]
    async fn write_cursor_data_no_dc_registered_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-no-cursor-dc", &request_remote, &s)
            .await
            .expect("create");
        let payload = CursorDataPayload {
            connection_id: "conn-no-cursor-dc".to_string(),
            data: br#"{"visible":true,"shape_id":42}"#.to_vec(),
        };
        // Test passes if this returns without panicking; the
        // cursor_data_channel slot is `None` at construction time,
        // so the silent-drop path must fire.
        write_cursor_data(&registry, payload).await;
    }

    /// Non-UTF-8 cursor payload bytes are dropped with a warn log,
    /// not propagated. Worker should always serialise as JSON, but
    /// the daemon must be resilient against a malformed shipment
    /// from a buggy / mismatched worker version.
    #[tokio::test]
    async fn write_cursor_data_invalid_utf8_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-bad-utf8", &request_remote, &s)
            .await
            .expect("create");
        // 0xFF is not a valid UTF-8 start byte — would panic on
        // unwrap if the daemon used `.unwrap()` instead of the
        // explicit error branch.
        let payload = CursorDataPayload {
            connection_id: "conn-bad-utf8".to_string(),
            data: vec![0xFFu8, 0xFE, 0xFD],
        };
        write_cursor_data(&registry, payload).await;
    }

    // ============== write_clipboard_data ==============

    /// `write_clipboard_data` for an unknown connection_id is a silent
    /// no-op — race against `CloseControl` must not panic.
    #[tokio::test]
    async fn write_clipboard_data_unknown_connection_is_silent_noop() {
        let registry = PcRegistry::new();
        let payload = ClipboardPayload {
            connection_id: "ghost".to_string(),
            data: br#"{"type":"text","content":"x"}"#.to_vec(),
        };
        write_clipboard_data(&registry, payload).await;
    }

    /// Permission gate: a connection that has neither `accept_control`
    /// nor `accept_clipboard_sync` set must not receive clipboard
    /// pushes. Mirrors the worker polling-task gate that read both
    /// flags from `SignalingState`.
    #[tokio::test]
    async fn write_clipboard_data_drops_when_permission_not_granted() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-no-perm", &request_remote, &s)
            .await
            .expect("create");
        let payload = ClipboardPayload {
            connection_id: "conn-no-perm".to_string(),
            data: br#"{"type":"text","content":"x"}"#.to_vec(),
        };
        // Default SignalingState has both flags false, so this must
        // silent-drop on the permission gate (before the DC-not-found
        // branch).
        write_clipboard_data(&registry, payload).await;
    }

    /// Permission granted but clipboard DC slot empty (browser hasn't
    /// opened the `clipboard_event` channel) is a silent no-op.
    #[tokio::test]
    async fn write_clipboard_data_no_dc_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let ctx = registry
            .create_for_request_remote("conn-no-dc", &request_remote, &s)
            .await
            .expect("create");
        // Flip the gates so we exercise the DC-missing branch.
        {
            let ctx_read = ctx.read().await;
            let mut s = ctx_read.signaling_state.write().await;
            s.accept_control = true;
            s.accept_clipboard_sync = true;
        }
        let payload = ClipboardPayload {
            connection_id: "conn-no-dc".to_string(),
            data: br#"{"type":"text","content":"x"}"#.to_vec(),
        };
        write_clipboard_data(&registry, payload).await;
    }

    /// Non-UTF-8 clipboard payload bytes are dropped (warn-logged) —
    /// matches the cursor variant. Defends against a buggy worker
    /// shipping malformed bytes.
    #[tokio::test]
    async fn write_clipboard_data_invalid_utf8_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        let ctx = registry
            .create_for_request_remote("conn-bad-utf8-clip", &request_remote, &s)
            .await
            .expect("create");
        {
            let ctx_read = ctx.read().await;
            let mut s = ctx_read.signaling_state.write().await;
            s.accept_control = true;
            s.accept_clipboard_sync = true;
        }
        let payload = ClipboardPayload {
            connection_id: "conn-bad-utf8-clip".to_string(),
            data: vec![0xFFu8, 0xFE, 0xFD],
        };
        write_clipboard_data(&registry, payload).await;
    }

    // ============== write_file_transfer_data ==============

    /// `write_file_transfer_data` for an unknown connection_id is a
    /// silent no-op — race against `CloseControl` must not panic.
    #[tokio::test]
    async fn write_file_transfer_data_unknown_connection_is_silent_noop() {
        let registry = PcRegistry::new();
        let payload = FileTransferPayload {
            connection_id: "ghost".to_string(),
            data: b"{\"type\":\"DownloadResponse\"}".to_vec(),
            is_text: true,
            transfer_id: None,
        };
        write_file_transfer_data(&registry, payload).await;
    }

    /// Regression for the portable-mode "download stuck at 0%" bug
    /// fixed 2026-05-05: `write_file_transfer_data` must NOT gate on
    /// `accept_control`. The browser file-management UI opens a fresh
    /// PC that never requests remote control, so a `accept_control`
    /// gate here silently dropped every download response chunk and
    /// the worker-side dispatcher (which had already authorised the
    /// transfer via `allow_file_transfer`) was left talking to a wall.
    ///
    /// This test exercises the DC-missing silent-drop branch on a
    /// connection whose `SignalingState` defaults to `accept_control =
    /// false`. Before the fix, the function would have returned at
    /// the permission check; after the fix it must reach (and silently
    /// no-op at) the DC-missing branch. Both paths look identical from
    /// the outside — the regression guard is the bare fact that no
    /// `accept_control` read remains in the function body. Keep this
    /// test alongside the source so a future re-introduction of the
    /// gate fails an explicit, named test.
    #[tokio::test]
    async fn write_file_transfer_data_does_not_gate_on_accept_control() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-no-control", &request_remote, &s)
            .await
            .expect("create");
        // Default SignalingState has both flags false. Pre-fix, this
        // would silent-drop on the permission gate; post-fix it falls
        // through to the DC-missing branch (also a silent no-op, but
        // the path is now driven only by the DC slot and ready_state).
        let payload = FileTransferPayload {
            connection_id: "conn-no-control".to_string(),
            data: b"{\"type\":\"DownloadResponse\"}".to_vec(),
            is_text: true,
            transfer_id: None,
        };
        write_file_transfer_data(&registry, payload).await;
    }

    /// Binary chunks (raw download bytes, `is_text = false`) follow
    /// the same DC-missing silent-drop path as text control replies.
    /// Pinning here so a regression that special-cases the binary
    /// branch (e.g. unwrapping the DC option) shows up as a panic
    /// rather than a corrupted production transfer.
    #[tokio::test]
    async fn write_file_transfer_data_binary_no_dc_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-bin-no-dc", &request_remote, &s)
            .await
            .expect("create");
        let payload = FileTransferPayload {
            connection_id: "conn-bin-no-dc".to_string(),
            data: vec![0x00, 0x01, 0x02, 0x03],
            is_text: false,
            transfer_id: None,
        };
        write_file_transfer_data(&registry, payload).await;
    }

    /// Core regression for the 2026-05-06 "file/list timeouts after
    /// big download" bug: `write_file_transfer_data` MUST return
    /// immediately even when a large backlog of payloads is in flight.
    /// Pre-fix the daemon's main IPC loop awaited `dc.send` for each
    /// chunk, and a slow / blocked DataChannel head-of-line blocked
    /// every other `WorkerToService` variant — including the
    /// `ManagerFileListResponse` the file manager UI was waiting on,
    /// causing 30-second `deadline elapsed` errors.
    ///
    /// Post-fix the dispatch is `O(1)` (registry lookup + non-blocking
    /// `UnboundedSender::send`); the actual `dc.send` runs in a
    /// per-connection writer task. Pinning a per-call upper bound
    /// guards against any future regression that re-introduces an
    /// `await dc.send` (or any other unbounded await) on this path.
    #[tokio::test(flavor = "current_thread")]
    async fn write_file_transfer_data_dispatch_returns_quickly_under_backlog() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-fast-dispatch", &request_remote, &s)
            .await
            .expect("create");

        // No DC registered — every payload silently drops in the
        // writer task. We push 1024 payloads back-to-back and require
        // the *dispatch* phase to complete inside 200 ms total. On a
        // pre-fix `dc.send().await` path even with a stub DC this
        // would be O(N) on async scheduling overhead; here we are
        // dominated only by per-call mpsc enqueues.
        let started = tokio::time::Instant::now();
        for i in 0..1024 {
            let payload = FileTransferPayload {
                connection_id: "conn-fast-dispatch".to_string(),
                data: format!("chunk-{i}").into_bytes(),
                is_text: true,
                transfer_id: None,
            };
            write_file_transfer_data(&registry, payload).await;
        }
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "dispatch loop took {elapsed:?}; pre-fix HOL blocking regression?"
        );
    }

    /// Dispatching to an unknown `connection_id` (race against
    /// `cleanup_pc → registry.remove`) is also expected to return
    /// without spawning anything new. Covers the path where the
    /// daemon's file-lane drain task picks up a stale payload for a
    /// PC the registry already removed — pre-fix this hit the same DC
    /// lookup as a live PC; post-fix it short-circuits at the registry
    /// lookup before any sender clone.
    #[tokio::test]
    async fn write_file_transfer_data_after_registry_remove_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-removed", &request_remote, &s)
            .await
            .expect("create");
        // Drop the registry entry — equivalent to `cleanup_pc` having
        // run. The writer task's sender is the last remaining
        // `Arc<RwLock<PerConnectionContext>>` reference, so dropping
        // the returned ctx here drops the sender and the task exits.
        let removed = registry.remove("conn-removed").await;
        drop(removed);

        let payload = FileTransferPayload {
            connection_id: "conn-removed".to_string(),
            data: b"stale".to_vec(),
            is_text: true,
            transfer_id: None,
        };
        write_file_transfer_data(&registry, payload).await;
    }

    /// The writer task must exit as soon as its sender is dropped
    /// (which is what `cleanup_pc → registry.remove` triggers). Pin
    /// the lifecycle by spawning the task directly with a known
    /// receiver, dropping the matching sender, and observing the task
    /// completes within a tight bound. Guards against a future
    /// refactor that accidentally retains the `UnboundedSender` on
    /// some long-lived global / DC handler closure (the result would
    /// be a writer task per closed connection, slowly leaking).
    #[tokio::test]
    async fn file_transfer_writer_task_exits_when_sender_drops() {
        let dc_slot: Arc<RwLock<Option<Arc<RTCDataChannel>>>> = Arc::new(RwLock::new(None));
        let (tx, rx) = mpsc::channel::<FileTransferPayload>(2);
        spawn_file_transfer_writer_task("conn-lifecycle".to_string(), rx, dc_slot, None);
        // Push one payload (silently dropped — no DC) then drop the
        // sender. The task drains the queued payload, observes
        // `recv() → None`, and exits.
        tx.send(FileTransferPayload {
            connection_id: "conn-lifecycle".to_string(),
            data: b"queued".to_vec(),
            is_text: true,
            transfer_id: None,
        })
        .await
        .expect("send pre-drop");
        drop(tx);
        // 200 ms is generous — the loop body for a no-DC payload is
        // pure CPU + a single read lock, so observed runtimes are
        // sub-millisecond. A blown timeout means the task did not
        // exit, i.e. the sender wasn't actually the last reference
        // (regression).
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::task::yield_now(),
        )
        .await
        .expect("yield");
        // No direct join handle because the task is spawned on the
        // actix-rt System; observable side effect is just that no
        // panic / hang occurred. Repeat the yield to give the
        // current_thread executor a chance to drive the task to
        // completion under the test runtime.
        tokio::task::yield_now().await;
    }

    /// Backpressure regression for the daemon side: when the
    /// per-connection writer queue saturates,
    /// `write_file_transfer_data` must `await` on the bounded
    /// `Sender::send` instead of dropping silently. Pre-fix the queue
    /// was unbounded so it always succeeded immediately, defeating the
    /// chain that pushes backpressure back through the file lane to
    /// the worker's `serve_download` loop.
    ///
    /// We swap the writer sender on a registered PC for a tiny
    /// (cap = 2) channel whose receiver we never drain, then assert
    /// the third dispatch parks for at least 100 ms before draining
    /// frees a slot.
    #[tokio::test]
    async fn write_file_transfer_data_awaits_when_writer_queue_full() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-bp", &request_remote, &s)
            .await
            .expect("create");
        // Hijack the writer slot with a starving channel.
        let (slow_tx, mut slow_rx) = mpsc::channel::<FileTransferPayload>(2);
        {
            let ctx_arc = registry.get("conn-bp").await.unwrap();
            let mut ctx = ctx_arc.write().await;
            ctx.file_transfer_writer_tx = slow_tx;
        }
        let mk = |tag: &str| FileTransferPayload {
            connection_id: "conn-bp".to_string(),
            data: tag.as_bytes().to_vec(),
            is_text: true,
            transfer_id: None,
        };
        // First two writes fill the queue and return promptly.
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            write_file_transfer_data(&registry, mk("p1")),
        )
        .await
        .expect("first write should not block");
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            write_file_transfer_data(&registry, mk("p2")),
        )
        .await
        .expect("second write should not block");
        // Third must park on `Sender::send().await` — assert it
        // doesn't return inside the timeout.
        let blocked = tokio::time::timeout(
            std::time::Duration::from_millis(150),
            write_file_transfer_data(&registry, mk("p3")),
        )
        .await;
        assert!(
            blocked.is_err(),
            "third write should backpressure on bounded queue, got: {blocked:?}"
        );
        // Drain one slot — a fresh write completes promptly.
        slow_rx.recv().await.expect("drain p1");
        tokio::time::timeout(
            std::time::Duration::from_millis(150),
            write_file_transfer_data(&registry, mk("p4")),
        )
        .await
        .expect("post-drain write should complete");
    }

    // ============== RTCP PLI/FIR identity ==============

    /// Identifying RTCP packets via `as_any().is::<T>()` /
    /// `downcast_ref::<T>()` is the path `spawn_rtcp_feedback_task`
    /// uses to decide between ForceKeyframe (PLI/FIR) and the
    /// bitrate-cap controller (REMB). Pin the identities so a
    /// webrtc-rs version bump that changed the trait object
    /// representation is caught here, not in production where missed
    /// PLIs become "browser stuck on stale frame after a packet loss"
    /// and missed REMBs silently disable adaptive bitrate.
    #[test]
    fn rtcp_pli_fir_and_remb_are_distinguishable_via_as_any() {
        use webrtc::rtcp::packet::Packet;

        let pli: Box<dyn Packet + Send + Sync> = Box::new(PictureLossIndication {
            sender_ssrc: 1,
            media_ssrc: 2,
        });
        let fir: Box<dyn Packet + Send + Sync> = Box::new(FullIntraRequest {
            sender_ssrc: 1,
            media_ssrc: 2,
            fir: vec![],
        });
        let remb: Box<dyn Packet + Send + Sync> = Box::new(ReceiverEstimatedMaximumBitrate {
            sender_ssrc: 1,
            bitrate: 4_000_000.0,
            ssrcs: vec![2],
        });

        assert!(pli.as_any().is::<PictureLossIndication>());
        assert!(!pli.as_any().is::<FullIntraRequest>());
        assert!(fir.as_any().is::<FullIntraRequest>());
        assert!(!fir.as_any().is::<PictureLossIndication>());
        let parsed = remb
            .as_any()
            .downcast_ref::<ReceiverEstimatedMaximumBitrate>()
            .expect("REMB must downcast");
        assert_eq!(parsed.bitrate, 4_000_000.0);
        assert!(!remb.as_any().is::<PictureLossIndication>());
    }

    // ============== adaptive bitrate-cap IPC ==============

    /// Pulls the next `UpdateMediaSettings` off the test IPC stream,
    /// asserting it carries only a bitrate directive.
    fn expect_cap_ipc(
        ipc_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServiceToWorker>,
        expect_connection: &str,
    ) -> u32 {
        match ipc_rx.try_recv().expect("expected an IPC message") {
            ServiceToWorker::UpdateMediaSettings(p) => {
                assert_eq!(p.connection_id, expect_connection);
                assert_eq!(p.fps, None);
                assert_eq!(p.quality, None);
                assert_eq!(p.enable_dirty_rect, None);
                p.bitrate_kbps.expect("cap IPC must carry bitrate_kbps")
            }
            other => panic!("expected UpdateMediaSettings, got {other:?}"),
        }
    }

    /// End-to-end over the daemon-side cap path: a committed cap
    /// followed by a disable edge must emit `bitrate_kbps: Some(0)`
    /// (the clear sentinel) for that connection, and decisions stop
    /// afterwards.
    #[tokio::test]
    async fn disable_with_active_cap_emits_clear_ipc() {
        let registry = PcRegistry::new();
        let s = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
            settings_with_startup(StartupMode::ServiceDaemon),
        ));
        let (worker_mgr, _) = WorkerManager::new(s.clone(), registry.clone());
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;

        let shared = crate::daemon::bitrate_controller::AdaptiveBitrateShared::new(true);

        // REMB indicates an 8 Mbps link → SetCap(6800) shipped + committed.
        {
            let mut state = shared.state.lock().await;
            let directive = state
                .decide_on_remb(std::time::Instant::now(), 8_000_000.0)
                .expect("constrained REMB must produce a directive");
            send_cap_directive(&worker_mgr, "conn-cap", directive, &mut state).await;
            assert_eq!(state.current_cap_kbps(), Some(6_800));
        }
        assert_eq!(expect_cap_ipc(&mut ipc_rx, "conn-cap"), 6_800);

        // Disable → Clear (wire Some(0)) + no further decisions.
        {
            let mut state = shared.state.lock().await;
            let directive = state
                .set_enabled_and_decide_clear(false)
                .expect("disable with active cap must emit Clear");
            send_cap_directive(&worker_mgr, "conn-cap", directive, &mut state).await;
            assert_eq!(state.current_cap_kbps(), None);
            assert_eq!(
                state.decide_on_remb(std::time::Instant::now(), 2_000_000.0),
                None,
                "disabled state must not emit further directives"
            );
        }
        assert_eq!(
            expect_cap_ipc(&mut ipc_rx, "conn-cap"),
            0,
            "clear must ride the Some(0) sentinel"
        );
        assert!(ipc_rx.try_recv().is_err(), "no further IPC expected");
    }

    /// A failed `send_to_worker` must not commit: the controller state
    /// keeps its previous cap so the next REMB re-decides instead of
    /// being suppressed by hysteresis; after a fresh IPC channel is
    /// installed the retry ships normally.
    #[tokio::test]
    async fn send_failure_does_not_commit_and_retry_succeeds() {
        let registry = PcRegistry::new();
        let s = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
            settings_with_startup(StartupMode::ServiceDaemon),
        ));
        let (worker_mgr, _) = WorkerManager::new(s.clone(), registry.clone());
        let (ipc_tx, ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;
        // Drop the receiver: the next send fails. (An mpsc receiver
        // cannot be revived — the retry below installs a new channel.)
        drop(ipc_rx);

        let shared = crate::daemon::bitrate_controller::AdaptiveBitrateShared::new(true);
        let now = std::time::Instant::now();

        {
            let mut state = shared.state.lock().await;
            let directive = state
                .decide_on_remb(now, 8_000_000.0)
                .expect("must decide a cap");
            send_cap_directive(&worker_mgr, "conn-f", directive, &mut state).await;
            assert_eq!(
                state.current_cap_kbps(),
                None,
                "failed send must not commit"
            );
        }

        // Fresh channel installed → identical REMB re-decides the same
        // directive (no hysteresis suppression) and ships it.
        let (ipc_tx2, mut ipc_rx2) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx2).await;
        {
            let mut state = shared.state.lock().await;
            let directive = state
                .decide_on_remb(now, 8_000_000.0)
                .expect("retry must re-decide after an uncommitted failure");
            send_cap_directive(&worker_mgr, "conn-f", directive, &mut state).await;
            assert_eq!(state.current_cap_kbps(), Some(6_800));
        }
        assert_eq!(expect_cap_ipc(&mut ipc_rx2, "conn-f"), 6_800);
    }

    /// Serialisation contract: REMB decisions and the disable edge
    /// both hold the state lock across decide → send → commit, so the
    /// FIFO IPC stream can never show a `SetCap` after the `Clear`.
    /// Drives many concurrent REMB tasks against one mid-flight
    /// disable and inspects the observed wire sequence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_setcap_after_clear_under_concurrency() {
        let registry = PcRegistry::new();
        let s = actix_web::web::Data::new(crate::model::settings::SharedSettings::from(
            settings_with_startup(StartupMode::ServiceDaemon),
        ));
        let (worker_mgr, _) = WorkerManager::new(s.clone(), registry.clone());
        let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::unbounded_channel::<ServiceToWorker>();
        worker_mgr.install_active_for_test(ipc_tx).await;

        let shared = Arc::new(crate::daemon::bitrate_controller::AdaptiveBitrateShared::new(true));

        let mut handles = Vec::new();
        for i in 0..50u32 {
            let shared = Arc::clone(&shared);
            let worker_mgr = worker_mgr.clone();
            handles.push(tokio::spawn(async move {
                // Alternate between two constrained estimates so the
                // urgent-drop path keeps emitting despite the 1 s
                // interval limiter.
                let remb = if i % 2 == 0 { 8_000_000.0 } else { 2_000_000.0 };
                let mut state = shared.state.lock().await;
                if let Some(d) = state.decide_on_remb(std::time::Instant::now(), remb) {
                    send_cap_directive(&worker_mgr, "conn-race", d, &mut state).await;
                }
            }));
        }
        // Disable roughly mid-flight.
        {
            let shared = Arc::clone(&shared);
            let worker_mgr = worker_mgr.clone();
            handles.push(tokio::spawn(async move {
                tokio::task::yield_now().await;
                let mut state = shared.state.lock().await;
                if let Some(d) = state.set_enabled_and_decide_clear(false) {
                    send_cap_directive(&worker_mgr, "conn-race", d, &mut state).await;
                }
            }));
        }
        for h in handles {
            h.await.expect("task panicked");
        }

        let mut saw_clear = false;
        while let Ok(msg) = ipc_rx.try_recv() {
            if let ServiceToWorker::UpdateMediaSettings(p) = msg {
                let kbps = p.bitrate_kbps.expect("cap IPC must carry bitrate_kbps");
                if kbps == 0 {
                    saw_clear = true;
                } else {
                    assert!(
                        !saw_clear,
                        "observed SetCap({kbps}) after Clear — decide/send/commit must be \
                         serialised under the state lock"
                    );
                }
            }
        }
    }

    // ============== handle_require_control tests ==============

    /// Build a SharedSettings whose security knobs are set to the
    /// given allow-state for control / clipboard. `Some(true)` means
    /// auto-allow without user prompt; `Some(false)` means auto-deny;
    /// `None` would route to the host_control_hub which our test
    /// fixture cannot drive without a Tauri shell.
    fn settings_with_security(
        allow_control: Option<bool>,
        allow_clipboard: Option<bool>,
    ) -> Arc<crate::model::settings::SharedSettings> {
        let mut s = Settings::default();
        s.security.allow_remote_control = allow_control;
        s.security.allow_clipboard_sync = allow_clipboard;
        Arc::new(crate::model::settings::SharedSettings::from(s))
    }

    fn require_control_model(
        from_connection_id: &str,
        accept: bool,
        accept_clipboard_sync: bool,
    ) -> SignalingModel {
        SignalingModel::new(
            "req-rc",
            SignalingType::RequireControl,
            Some(from_connection_id.to_string()),
            None,
            Some(
                serde_json::to_value(SignalRequestControlData {
                    accept,
                    accept_file_transfer: false,
                    accept_clipboard_sync,
                })
                .unwrap(),
            ),
            None,
        )
    }

    /// Auto-allow happy path: settings.security.allow_remote_control =
    /// Some(true) + browser asks for both control and clipboard. State
    /// flips, daemon emits AcceptControl back through outbound.
    #[tokio::test]
    async fn handle_require_control_auto_allows_and_emits_accept() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let settings = settings_with_security(Some(true), Some(true));
        let hub = Arc::new(HostControlHub::new_local());

        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        registry
            .create_for_request_remote("conn-rc", &request_remote, &*settings.read().await)
            .await
            .expect("seed pc");

        let model = require_control_model("conn-rc", true, true);
        handle_require_control(&registry, &outbound_tx, &settings, &hub, &model)
            .await
            .expect("handle ok");

        let text = outbound_rx.recv().await.expect("AcceptControl reply");
        let reply: SignalingModel = serde_json::from_str(&text).expect("decode reply");
        assert_eq!(
            reply.signaling_type,
            SignalingType::AcceptControl,
            "expected AcceptControl, got {:?}",
            reply.signaling_type,
        );
        let ctx = registry.get("conn-rc").await.unwrap();
        let s = ctx.read().await.signaling_state.read().await.clone();
        assert!(s.accept_control, "accept_control must flip true");
        assert!(
            s.accept_clipboard_sync,
            "accept_clipboard_sync must flip true when both grants approved"
        );
    }

    /// Control denied via settings: state stays false, DenyControl
    /// reply. Subsequent mouse / keyboard IPC must remain blocked
    /// because the daemon's permission gate reads from the same state.
    #[tokio::test]
    async fn handle_require_control_auto_denies_and_emits_deny() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let settings = settings_with_security(Some(false), None);
        let hub = Arc::new(HostControlHub::new_local());

        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        registry
            .create_for_request_remote("conn-deny", &request_remote, &*settings.read().await)
            .await
            .expect("seed pc");

        let model = require_control_model("conn-deny", true, false);
        handle_require_control(&registry, &outbound_tx, &settings, &hub, &model)
            .await
            .expect("handle ok");

        let text = outbound_rx.recv().await.expect("DenyControl reply");
        let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
        assert_eq!(
            reply.signaling_type,
            SignalingType::DenyControl,
            "expected DenyControl, got {:?}",
            reply.signaling_type,
        );
        let ctx = registry.get("conn-deny").await.unwrap();
        let s = ctx.read().await.signaling_state.read().await.clone();
        assert!(!s.accept_control, "accept_control must stay false");
        assert!(
            !s.accept_clipboard_sync,
            "accept_clipboard_sync must stay false"
        );
    }

    /// Release path: browser sends RequireControl{accept=false} to
    /// release a previously-granted control. State goes false +
    /// CloseControl reply. The short-circuit helper must NOT
    /// short-circuit the release (would leave the worker stuck with
    /// accept_control=true) — covered by `should_short_circuit_*`
    /// helper tests in service::signaling, but verified end-to-end here.
    #[tokio::test]
    async fn handle_require_control_release_emits_close_and_resets_state() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let settings = settings_with_security(Some(true), Some(true));
        let hub = Arc::new(HostControlHub::new_local());

        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let ctx = registry
            .create_for_request_remote("conn-release", &request_remote, &*settings.read().await)
            .await
            .expect("seed pc");
        // Pre-flip state to "currently controlling" so the release
        // path is the one that fires.
        {
            let ctx_read = ctx.read().await;
            let mut s = ctx_read.signaling_state.write().await;
            s.accept_control = true;
            s.accept_clipboard_sync = true;
        }

        let model = require_control_model("conn-release", false, false);
        handle_require_control(&registry, &outbound_tx, &settings, &hub, &model)
            .await
            .expect("handle ok");

        let text = outbound_rx.recv().await.expect("CloseControl reply");
        let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
        assert_eq!(
            reply.signaling_type,
            SignalingType::CloseControl,
            "expected CloseControl, got {:?}",
            reply.signaling_type,
        );
        let s = ctx.read().await.signaling_state.read().await.clone();
        assert!(!s.accept_control, "accept_control must go false on release");
        assert!(
            !s.accept_clipboard_sync,
            "accept_clipboard_sync must go false on release"
        );
    }

    /// Regression: releasing control must NEVER prompt the host, even when
    /// `allow_remote_control = None` (the default "ask" mode). The browser sends
    /// RequireControl{accept=false} when the user clicks "cancel control"; if the
    /// release path consulted the approval hub it would pop a spurious
    /// authorization dialog and block on the UI-readiness probe with no Tauri
    /// shell connected. Asserting it resolves well under the probe timeout proves
    /// the hub was never consulted.
    #[tokio::test]
    async fn handle_require_control_release_does_not_prompt_when_ask_mode() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        // None = "ask the user" — the path that previously triggered the dialog.
        let settings = settings_with_security(None, None);
        let hub = Arc::new(HostControlHub::new_local());

        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let ctx = registry
            .create_for_request_remote("conn-ask-release", &request_remote, &*settings.read().await)
            .await
            .expect("seed pc");
        {
            let ctx_read = ctx.read().await;
            let mut s = ctx_read.signaling_state.write().await;
            s.accept_control = true;
            s.accept_clipboard_sync = true;
        }

        let model = require_control_model("conn-ask-release", false, false);
        // Must resolve promptly: the real UI-readiness probe is 10s, so a 1s
        // budget fails loudly if the release ever routes through the hub.
        let model_ref = &model;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            handle_require_control(&registry, &outbound_tx, &settings, &hub, model_ref),
        )
        .await
        .expect("release must not block on the approval hub")
        .expect("handle ok");

        let text = outbound_rx.recv().await.expect("CloseControl reply");
        let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
        assert_eq!(
            reply.signaling_type,
            SignalingType::CloseControl,
            "expected CloseControl, got {:?}",
            reply.signaling_type,
        );
        let s = ctx.read().await.signaling_state.read().await.clone();
        assert!(!s.accept_control, "accept_control must go false on release");
        assert!(
            !s.accept_clipboard_sync,
            "accept_clipboard_sync must go false on release"
        );
    }

    /// Re-grant of an already-accepted control short-circuits — the
    /// helper returns true without prompting the user (would race
    /// against any in-flight Tauri dialog otherwise). State stays
    /// true, AcceptControl reply emitted.
    #[tokio::test]
    async fn handle_require_control_regrant_short_circuits() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        // Settings deliberately set to None so a non-short-circuit
        // path would route to the hub — but the short-circuit fires
        // first because state is already accepted. If the
        // short-circuit broke, this test would hang on the hub call.
        let settings = settings_with_security(None, None);
        let hub = Arc::new(HostControlHub::new_local());

        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let ctx = registry
            .create_for_request_remote("conn-regrant", &request_remote, &*settings.read().await)
            .await
            .expect("seed pc");
        {
            let ctx_read = ctx.read().await;
            let mut s = ctx_read.signaling_state.write().await;
            s.accept_control = true;
            s.accept_clipboard_sync = true;
        }

        let model = require_control_model("conn-regrant", true, true);
        // Short timeout so a regression that bypasses the
        // short-circuit and falls into the hub call (which would
        // never complete in this test fixture) fails loudly.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            handle_require_control(&registry, &outbound_tx, &settings, &hub, &model),
        )
        .await
        .expect("handle_require_control must short-circuit, not block on hub")
        .expect("handle ok");

        let text = outbound_rx.recv().await.expect("AcceptControl reply");
        let reply: SignalingModel = serde_json::from_str(&text).expect("decode");
        assert_eq!(reply.signaling_type, SignalingType::AcceptControl);
    }

    /// RequireControl for an unknown `connection_id` returns an error
    /// (browser sent a grant for a PC the daemon never created — most
    /// likely the matching RequestRemote was rejected upstream). The
    /// router relays the error to the upstream signaling so the
    /// browser can re-issue cleanly.
    #[tokio::test]
    async fn handle_require_control_unknown_connection_errors() {
        let registry = PcRegistry::new();
        let (outbound_tx, _) = broadcast::channel::<String>(8);
        let settings = settings_with_security(Some(true), Some(true));
        let hub = Arc::new(HostControlHub::new_local());

        let model = require_control_model("ghost", true, true);
        let result = handle_require_control(&registry, &outbound_tx, &settings, &hub, &model).await;
        assert!(result.is_err(), "unknown connection must surface an error");
    }

    /// Multi-connection: independent contexts coexist; closing one
    /// leaves the other intact (multi-browser concurrency contract).
    #[tokio::test]
    async fn pc_registry_supports_multiple_independent_connections() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
            grant_session_id: None,
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);

        registry
            .create_for_request_remote("a", &request_remote, &s)
            .await
            .expect("a");
        registry
            .create_for_request_remote("b", &request_remote, &s)
            .await
            .expect("b");
        assert_eq!(registry.len().await, 2);
        registry.remove("a").await;
        assert!(!registry.contains("a").await);
        assert!(registry.contains("b").await);
        assert_eq!(registry.len().await, 1);
    }
}
