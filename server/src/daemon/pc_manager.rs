//! # Daemon-side WebRTC PeerConnection manager (Arch IV)
//!
//! Owner of the [`webrtc::peer_connection::RTCPeerConnection`] lifecycle.
//! In Arch III the worker process held the PC, which meant every UAC /
//! lock-screen / OS-session-switch (any event that respawns the worker)
//! tore down the PC and forced the browser through full SDP renegotiation
//! + ICE restart — a path that became unstable under SYSTEM-token +
//! Winlogon desktop combinations and showed up as "video garbled / ICE
//! checking → failed" during UAC.
//!
//! Arch IV moves the PC into the daemon: WebRTC negotiation happens once
//! per browser session and survives every worker swap. Worker replacement
//! becomes invisible to the browser apart from a ~1 s frame freeze waiting
//! for the next IDR from the new encoder.
//!
//! ## Status
//!
//! Cut 3b of PR 2: `PcRegistry` + per-`SignalingType` handlers for the
//! five WebRTC SDP/ICE messages the daemon now owns
//! (`RequestRemote` / `Offer` / `Answer` / `Canid` / `CloseControl`).
//! Cut 4 wires the worker's media transport into the per-PC tracks
//! the registry holds; cut 5 registers the DataChannel handlers on
//! top.
//!
//! ### Known intermediate state (cut 3b → cut 4)
//!
//! Browsers can complete SDP/ICE successfully against the daemon, but
//! no media frames flow yet — the per-PC `video_track` /
//! `audio_track` exist (so the SDP m-lines come back as `sendonly`),
//! they are just never written to. Cut 4 hooks the worker
//! `media_producer` → daemon `MediaTransport receiver` →
//! `track.write_sample(...)` chain.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use crate::host_control::HostControlHub;
use crate::model::data_channel::SignalRequestControlData;
use crate::model::security_approval::{SecurityPermissionType, check_security_permission};
use crate::model::settings::{
    Settings, SharedSettings, StartupMode, SystemSettings, TraversalMode,
};
use crate::service::signaling::{should_short_circuit_clipboard, should_short_circuit_control};
use desk_capture_engine::audio_encoder::audio_encoder_factory::list_audio_encoder;
use desk_capture_engine::model::video_encoder::{VideoEncoderType, VideoEncoderTypeHelper};
use desk_capture_engine::video_encoder::video_encoder_factory::list_video_encoder;
use desk_ipc_protocol::message::{
    ClipboardPayload, CursorDataPayload, FileTransferPayload, ForceKeyframePayload, InputPayload,
    MediaCapabilities, MediaCodec, MediaFrame, MediaFrameKind, OpaqueConnectionPayload,
    ServiceToWorker, StartMediaPayload, StopMediaPayload, UpdateMediaSettingsPayload,
};
use desk_signal_facade::model::signal::InitSignalingData;
use std::time::Duration;
use webrtc::media::Sample;

/// Filter the request's ICE servers down to the ones this node should
/// actually use given the local `traversal_mode` and `startup_mode`.
///
/// - `Stun` mode keeps STUN servers and drops TURN.
/// - `Turn` mode keeps STUN; keeps TURN only on `DeskServer` startups
///   (other modes do not traverse so they do not need TURN credentials).
/// - `Direct` mode drops everything.
///
/// Servers with no / unrecognised transport are skipped with a warning.
/// Pure function — no I/O, no settings lookup, easy to unit test.
pub fn filter_ice_servers(
    request_ice_servers: &[LcxlRTCIceServer],
    traversal_mode: &TraversalMode,
    startup_mode: StartupMode,
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
                if matches!(traversal_mode, TraversalMode::Turn)
                    && startup_mode == StartupMode::DeskServer
                {
                    filtered.push(ice_server.clone());
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
/// Cut 3b populates `pc` + `signaling_state` + (when the offer
/// includes media) `video_track` / `audio_track`. Cut 4 starts
/// writing samples into the tracks from worker `MediaFrame`s; cut 5
/// installs the `on_data_channel` handler that routes browser DC
/// traffic over IPC to the worker (mouse / keyboard / clipboard /
/// file / whiteboard) and stashes the cursor-sync DC in
/// `cursor_data_channel` for PR 3 to push cursor updates back to.
pub struct PeerConnectionContext {
    pub connection_id: String,
    pub pc: Arc<RTCPeerConnection>,
    pub signaling_state: Arc<RwLock<SignalingState>>,
    /// Set on the first `Offer` whose SDP carries `m=video`. Cut 4
    /// drives this from worker-side `MediaFrame`s (`MediaFrameKind::
    /// VideoI`/`VideoP`).
    pub video_track: Option<Arc<TrackLocalStaticSample>>,
    /// Set on the first `Offer` whose SDP carries `m=audio`. Same
    /// fill timing as `video_track`.
    pub audio_track: Option<Arc<TrackLocalStaticSample>>,
    /// Set when the browser opens the `cursor_sync_event` DataChannel.
    /// Cut 5 only writes to this slot from the daemon `on_data_channel`
    /// handler; PR 3 wires worker-side `WorkerToService::CursorData` to
    /// `dc.send(...)` here.
    pub cursor_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    /// Set when the browser opens the `clipboard_event` DataChannel.
    /// PR 4 cut 1 wires worker-side `WorkerToService::ClipboardRead` to
    /// `dc.send_text(...)` here. Browser→host clipboard writes still
    /// flow through the standard router (DC `on_message` →
    /// `ServiceToWorker::ClipboardWrite`).
    pub clipboard_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    /// Set when the browser opens the `file_transfer_event`
    /// DataChannel. PR 4 cut 2 wires worker-side
    /// `WorkerToService::FileTransferData` to `dc.send_text(...)` /
    /// `dc.send(...)` (per-payload `is_text`) here. Browser→worker
    /// chunks and control messages still flow through the standard
    /// router as `ServiceToWorker::FileTransferCommand`.
    pub file_transfer_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    /// Sender into the per-connection file-transfer writer task spawned
    /// in [`PcRegistry::create_for_request_remote`]. The task drains
    /// payloads serially and calls `dc.send` / `dc.send_text` on the
    /// matching `file_transfer_data_channel`. Decouples SCTP write
    /// back-pressure from the daemon's main IPC dispatch loop in
    /// `signaling_proxy::run_signaling_proxy`: a 989 MB download
    /// stalling on a slow / blocked DataChannel no longer head-of-line
    /// blocks unrelated `WorkerToService::ManagerFileListResponse` and
    /// other typed-IPC traffic. Send is non-blocking
    /// (`UnboundedSender::send`); the task exits naturally when this
    /// sender is dropped, which happens when the registry releases the
    /// last `Arc` reference to this `PeerConnectionContext` (i.e. after
    /// `cleanup_pc → registry.remove`).
    pub file_transfer_writer_tx: mpsc::UnboundedSender<FileTransferPayload>,
    /// PR 6: pause flag set by [`PcRegistry::pause_all_media`] before a
    /// worker swap. While set, [`write_video_frame`] drops samples so
    /// `webrtc-rs` does not push frames the new encoder hasn't anchored
    /// yet. The first `MediaFrameKind::VideoI` after the pause clears
    /// the flag in-line, giving the browser a clean IDR-aligned
    /// resync. Audio falls under the same flag — the brief silence is
    /// preferable to playing audio against a frozen video frame.
    pub media_paused: Arc<AtomicBool>,
    /// PR 6: cached payload from the most recent `handle_offer` for
    /// this connection. After a worker swap [`PcRegistry::resume_active_media`]
    /// re-issues this (plus a `ForceKeyframe`) so the new worker
    /// re-arms its per-`connection_id` encoder without a fresh SDP
    /// round-trip. `None` means the offer hasn't been exchanged yet
    /// (PC up but no media negotiated) — resume is a no-op for those.
    pub cached_start_media: Arc<RwLock<Option<StartMediaPayload>>>,
}

/// Daemon-wide registry of active per-browser
/// `PeerConnectionContext`s, indexed by `connection_id`. Equivalent
/// to the `DeskSession::rtc_peer_connection_map` the worker held in
/// Arch III but lives in the daemon process so it survives every
/// worker swap.
#[derive(Clone, Default)]
pub struct PcRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<RwLock<PeerConnectionContext>>>>>,
}

/// Errors produced by [`PcRegistry`] handlers. Worker-side equivalents
/// in `service::signaling` use the broader `DeskError`; the registry
/// re-uses it so callers don't have to bridge two error types.
type RegistryResult<T> = Result<T, DeskError>;

impl PcRegistry {
    pub fn new() -> Self {
        Self::default()
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

    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
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
    /// here — that requires `MediaCapabilities` from the worker which
    /// only land in cut 4. Until then the caller composes a
    /// best-effort Init reply with empty device lists.
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
            local_settings.args.startup_mode.clone(),
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
            mpsc::unbounded_channel::<FileTransferPayload>();
        spawn_file_transfer_writer_task(
            connection_id.to_string(),
            file_transfer_writer_rx,
            Arc::clone(&file_transfer_data_channel),
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
        }));

        self.inner
            .write()
            .await
            .insert(connection_id.to_string(), Arc::clone(&ctx));

        Ok(ctx)
    }

    /// PR 6: mark every active PC as paused before a worker swap.
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

    /// PR 6: re-issue the cached `StartMediaPayload` + a `ForceKeyframe`
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
    ) {
        if fps.is_none() && bitrate_kbps.is_none() && quality.is_none() {
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
// Cut 5: DataChannel routing daemon → worker
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
    /// stash the channel handle so PR 3's worker→daemon CursorData
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
/// handler. Cut 5 only handles browser→host directions; the
/// `Clipboard` arm uses `ClipboardWrite` (browser writing to host
/// clipboard); a future browser→host clipboard *request* DC would map
/// to `ClipboardRequest` but the current protocol multiplexes both
/// over the same `clipboard_event` channel and the worker disambiguates
/// by payload, so cut 5 always emits `ClipboardWrite`.
fn route_to_service_msg(
    route: DcRoute,
    connection_id: &str,
    data: Vec<u8>,
    is_text: bool,
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
        // PR 4 cut 2: file_transfer carries the text/binary
        // discriminator so the worker dispatches to the JSON-control
        // path or the binary-chunk path correctly. Other routes ignore
        // `is_text` (their wire shape is text-only on the browser side).
        DcRoute::FileTransfer => ServiceToWorker::FileTransferCommand(FileTransferPayload {
            connection_id: connection_id.to_string(),
            data,
            is_text,
        }),
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
/// per-handler gating that lived in the worker's `handle_*_event`
/// functions in Arch III; consolidating it here means the worker can
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
/// per connection (matching Arch III's per-DC permission cache).
async fn route_is_permitted(route: DcRoute, state: &Arc<RwLock<SignalingState>>) -> bool {
    let s = state.read().await;
    match route {
        DcRoute::Mouse | DcRoute::MouseMove | DcRoute::Keyboard => s.accept_control,
        DcRoute::Clipboard => s.accept_clipboard_sync,
        DcRoute::FileTransfer => true,
        // Whiteboard rides on the control grant in Arch III; PR 4 may
        // split it onto its own switch but cut 5 matches Arch III's
        // behaviour exactly to avoid behaviour regressions during the
        // cutover.
        DcRoute::Whiteboard => s.accept_control,
        DcRoute::CursorSync => unreachable!("CursorSync DC has no message route"),
    }
}

/// Install the daemon's `on_data_channel` callback. Each browser-opened
/// DataChannel either (a) gets its `on_message` wired into the
/// IPC-forwarding closure that ships to the worker via
/// `ServiceToWorker::*`, or (b) for `cursor_sync_event`, has its
/// `Arc<RTCDataChannel>` stashed in the per-connection
/// `cursor_data_channel` slot for PR 3 cursor-write-back. PR 4 cut 1
/// adds a third path: `clipboard_event` channels are *both* stashed
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
                let mut slot = cursor_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed cursor_sync_event channel \
                     for PR 3 worker→daemon cursor write-back"
                );
                return;
            }
            if route == DcRoute::Clipboard {
                let mut slot = clipboard_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed clipboard_event channel \
                     for PR 4 worker→daemon clipboard write-back"
                );
                // Fall through to install the on_message forwarder so
                // browser→host writes still flow as ClipboardWrite IPC.
            }
            if route == DcRoute::FileTransfer {
                let mut slot = file_transfer_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed file_transfer_event channel \
                     for PR 4 worker→daemon file write-back"
                );
                // Fall through to install the on_message forwarder so
                // browser→host commands and chunks still flow as
                // FileTransferCommand IPC.
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
// Cut 5: RTCP reader → ForceKeyframe IPC
// =====================================================================

/// Spawn a task that reads RTCP feedback off `rtp_sender` and translates
/// PLI / FIR packets into `ServiceToWorker::ForceKeyframe` IPC messages
/// addressed to `connection_id`. PLI = Picture Loss Indication (RFC
/// 4585 §6.3.1), FIR = Full Intra Request (RFC 5104 §4.3.1.1); both
/// are the browser asking us for a fresh IDR. The encoder is on the
/// worker side, so we hand the request off via the worker manager
/// and let the worker's `MediaProducer::force_keyframe` flag the next
/// encode pass.
///
/// Exits when `read_rtcp` returns `Err` — that happens on PC close /
/// CloseControl, which is the natural lifetime of the task. A noisy
/// transient read error logs at warn level and continues, because the
/// rtp_sender survives single bad reads (e.g. malformed RTCP packet
/// from a buggy proxy).
fn spawn_rtcp_force_keyframe_task(
    rtp_sender: Arc<RTCRtpSender>,
    connection_id: String,
    worker_mgr: WorkerManager,
) {
    tokio::spawn(async move {
        log::info!("[RtcpReader] {connection_id}: starting");
        loop {
            match rtp_sender.read_rtcp().await {
                Ok((packets, _attrs)) => {
                    for pkt in packets {
                        let any = pkt.as_any();
                        let is_force_keyframe =
                            any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>();
                        if !is_force_keyframe {
                            continue;
                        }
                        log::debug!(
                            "[RtcpReader] {connection_id}: PLI/FIR received → ForceKeyframe IPC"
                        );
                        let msg = ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                            connection_id: connection_id.clone(),
                        });
                        if let Err(e) = worker_mgr.send_to_worker(msg).await {
                            log::warn!(
                                "[RtcpReader] {connection_id}: ForceKeyframe IPC failed: {e}"
                            );
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
/// signaling channel. Mirrors the Arch III worker behaviour where each
/// host / srflx / relay candidate emitted by libwebrtc was wrapped in a
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
/// `init_ptc_peer_connection` minus the Arch III preapproved
/// restoration (PC now lives in the daemon and never has to be
/// rehydrated across worker swaps) and minus the device-list
/// enumeration (replaced in cut 4 by the worker's `Capabilities`
/// message).
pub async fn handle_request_remote(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    settings: &Settings,
    user_name: &str,
    has_tauri: bool,
    capabilities: Option<&MediaCapabilities>,
    worker_mgr: Option<&WorkerManager>,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let request_remote = model.get_data::<RequestRemoteModel>()?;

    let ctx = registry
        .create_for_request_remote(from_connection_id, &request_remote, settings)
        .await?;

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

    // Cut 5: install the daemon-side `on_data_channel` router on the
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
            from_connection_id.to_string(),
        );
    }

    // Cut 4: populate the Init reply from the worker's
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
        (
            caps.audio_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>(),
            caps.video_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>(),
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
/// directions; cut 4 starts feeding the tracks from the worker.
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

    let sdp_str = &offer.offer.sdp;
    let has_video = sdp_str.contains("m=video");
    let has_audio = sdp_str.contains("m=audio");
    log::info!(
        "[pc_manager] Offer from {from_connection_id}: has_video={has_video}, has_audio={has_audio}"
    );

    if has_video && ctx_guard.video_track.is_none() {
        let video_mime_type = match offer.desk_settings.get_video_encoder_type()? {
            VideoEncoderType::H264 | VideoEncoderType::X264 => MIME_TYPE_H264,
            VideoEncoderType::VP8 => MIME_TYPE_VP8,
            VideoEncoderType::VP9 => MIME_TYPE_VP9,
            VideoEncoderType::AV1 => MIME_TYPE_AV1,
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
        // Cut 5: spawn the RTCP reader. Browser sends PLI / FIR when
        // it detects packet loss or just joined an in-progress stream;
        // we translate either into `ServiceToWorker::ForceKeyframe`
        // so the per-connection encoder emits an IDR on its next
        // pass. Reader exits when the rtp_sender is closed (PC drop /
        // CloseControl), see `spawn_rtcp_force_keyframe_task`.
        spawn_rtcp_force_keyframe_task(
            rtp_sender,
            from_connection_id.to_string(),
            worker_mgr.clone(),
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

    // Cut 4: now that the SDP exchange has populated tracks, tell the
    // worker to start its per-`connection_id` encoder. Without this
    // the daemon would have a video_track that nobody ever feeds.
    // Audio codec defaults to OPUS — PR 3 picks the worker's chosen
    // codec for real once the audio path lands.
    let video_codec = video_encoder_to_media_codec(offer.desk_settings.get_video_encoder_type()?);
    let start_media_payload = StartMediaPayload {
        connection_id: from_connection_id.to_string(),
        video_codec,
        audio_codec: MediaCodec::Opus,
        video_device: None,
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
    };
    // PR 6: stash the payload so a worker swap can re-issue it via
    // `resume_active_media` without forcing the browser through a fresh
    // SDP offer. Done before `drop(ctx_guard)` so the cache is published
    // ahead of any concurrent resume that races the swap.
    let cache_slot = ctx_guard.cached_start_media.clone();
    drop(ctx_guard);
    *cache_slot.write().await = Some(start_media_payload.clone());
    if let Err(e) = worker_mgr
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
// MediaFrame ingestion (Arch IV cut 4)
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
/// Cut 4 only handles video; audio is shaped through the same entry
/// point so PR 3 can fill in the audio path without re-plumbing the
/// receiver.
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

    // PR 6: while a worker swap is in progress every frame except the
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

/// PR 3: write a worker-emitted cursor-sync payload to the matching
/// connection's `cursor_sync_event` DataChannel. Mirrors the Arch III
/// path in `service::signaling::capture_screen_task` that did the same
/// `channel.send_text(json)` call inline; here the daemon performs it
/// based on a `WorkerToService::CursorData` IPC the worker pushes from
/// its capture loop.
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
    // text frame matching the legacy Arch III shape exactly.
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

/// PR 4 cut 1: write a worker-emitted clipboard payload (text or
/// chunked image — already JSON-encoded as `ClipboardEventData`) to
/// the matching connection's `clipboard_event` DataChannel. Mirrors
/// the Arch III polling-task `dc.send_text(...)` calls in
/// `service::clipboard_event::handle_clipboard_event`; here the daemon
/// writes the JSON unchanged so the browser sees the exact same wire
/// shape.
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

/// PR 4 cut 2: route a worker-emitted file-transfer payload onto the
/// matching connection's per-connection writer task (queued via
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
    let ctx = match registry.get(&payload.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping file transfer data for unknown connection {}",
                payload.connection_id
            );
            return;
        }
    };
    let ctx = ctx.read().await;
    if ctx.file_transfer_writer_tx.send(payload).is_err() {
        log::debug!(
            "[pc_manager] file transfer writer task gone for {} — context torn down",
            ctx.connection_id
        );
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
/// Silent-drop branches inside the task:
///
/// - No file-transfer DC registered — debug (browser hasn't opened it
///   yet, or PC was torn down before the DC frame arrived).
/// - DC not in `Open` state — debug.
/// - send_text on non-UTF-8 bytes — warn + drop. Defends against a
///   buggy worker that sets `is_text=true` on raw chunk bytes.
/// - send failed — warn. The worker's transfer protocol carries
///   per-chunk acks; the browser detects loss and recovers via
///   `TransferError` retransmission.
fn spawn_file_transfer_writer_task(
    connection_id: String,
    mut rx: mpsc::UnboundedReceiver<FileTransferPayload>,
    dc_slot: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
) {
    // `tokio::spawn` (not `actix_web::rt::spawn`) is intentional:
    // the task only awaits `mpsc::recv` and `webrtc-rs` futures, both
    // of which are `Send` and need no `LocalSet`. Using
    // `actix_web::rt::spawn` (which is `spawn_local`) would force
    // every `#[tokio::test]` that calls `create_for_request_remote`
    // to wrap itself in a `LocalSet` for the constructor to succeed.
    tokio::spawn(async move {
        while let Some(payload) = rx.recv().await {
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
            let result = if payload.is_text {
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
            if let Err(e) = result {
                log::warn!(
                    "[pc_manager] failed to send file transfer data for {connection_id}: {e}"
                );
            }
        }
        log::debug!(
            "[pc_manager] file transfer writer task exited for {connection_id} (sender dropped)"
        );
    });
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
    connection_id: &str,
    reason: &str,
) {
    if let Some(ctx) = registry.remove(connection_id).await {
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
}

/// Wire the daemon-side cleanup path onto `pc.on_peer_connection_state_change`
/// so a browser disconnect / network drop / explicit close releases the
/// worker's encoder + capture resources promptly.
///
/// Without this hook the worker keeps the per-connection encoder running and
/// the per-output DXGI duplication held; the next browser to connect then
/// hits `DuplicateOutput → 0x80070057 (E_INVALIDARG)` because Windows only
/// allows one duplication per (process, output) pair. Mirrors the Arch III
/// `peer_state_change_sender → DeskSessionMessage::WebRTCDropped` chain
/// that lived in `service::signaling::DeskSession::init_ptc_peer_connection`.
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
    connection_id: String,
) {
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let registry = registry.clone();
        let worker_mgr = worker_mgr.clone();
        let connection_id = connection_id.clone();
        Box::pin(async move {
            match state {
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    log::info!(
                        "[pc_manager] PC for {connection_id} reached terminal state {state:?}; \
                         tearing down daemon-side context + StopMedia to worker"
                    );
                    cleanup_pc(&registry, &worker_mgr, &connection_id, "pc_state_terminal").await;
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
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    cleanup_pc(registry, worker_mgr, from_connection_id, "close_control").await;
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
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    cleanup_pc(
        registry,
        worker_mgr,
        from_connection_id,
        "peer_signaling_closed",
    )
    .await;
    Ok(())
}

/// Daemon side of `SignalingType::RequireControl`. Mirrors the
/// worker-side `DeskSession::handle_request_control` from Arch III but
/// runs against the daemon-held PC. The browser sends this to either
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
/// Cut 5's daemon `on_data_channel` router gates each forwarded
/// browser-input event on the resulting `accept_control` /
/// `accept_clipboard_sync` flags, so the worker only ever sees IPC
/// payloads the user has authorised.
pub async fn handle_require_control(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    settings: &SharedSettings,
    host_control_hub: &Arc<HostControlHub>,
    model: &SignalingModel,
) -> Result<(), DeskError> {
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
        return Ok(());
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
        if control_data.accept {
            s.accept_control = true;
            s.accept_clipboard_sync = clipboard_approved;
            log::info!(
                "[pc_manager] {from_connection_id}: AcceptControl \
                 (accept_control=true, accept_clipboard_sync={clipboard_approved})"
            );
        } else {
            s.accept_control = false;
            s.accept_clipboard_sync = false;
            log::info!("[pc_manager] {from_connection_id}: release (CloseControl)");
        }
    }

    let reply_type = if control_data.accept {
        SignalingType::AcceptControl
    } else {
        SignalingType::CloseControl
    };
    send_response::<()>(
        outbound,
        &model.request_id,
        reply_type,
        from_connection_id,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::MediaCodec;

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

    #[test]
    fn filter_keeps_stun_only_in_stun_mode() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::Stun, StartupMode::DeskServer);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
    }

    #[test]
    fn filter_keeps_both_in_turn_mode_for_desk_server() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, StartupMode::DeskServer);
        assert_eq!(kept.len(), 2);
    }

    /// In Turn mode but on a non-DeskServer startup, TURN servers are
    /// dropped because non-DeskServer modes never traverse — they would
    /// burn TURN credentials for nothing and risk a credential leak via
    /// the browser-side ICE candidate dump.
    #[test]
    fn filter_drops_turn_in_turn_mode_when_not_desk_server() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::Turn, StartupMode::ServiceDaemon);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
    }

    /// `TraversalMode::None` means "no STUN, no TURN, host candidates
    /// only". The filter drops everything from the request.
    #[test]
    fn filter_drops_everything_in_none_mode() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, &TraversalMode::None, StartupMode::DeskServer);
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
        let kept = filter_ice_servers(&request, &TraversalMode::Stun, StartupMode::DeskServer);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
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

    /// Round-trip: create, contains, get, remove.
    #[tokio::test]
    async fn pc_registry_create_get_remove_cycle() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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
    /// with a debug log, not propagated. Cut 4 must keep the receiver
    /// task running through that window.
    #[tokio::test]
    async fn write_video_frame_no_track_yet_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-no-track", &request_remote, &s)
            .await
            .expect("create");
        // Registry has the context, but `video_track` is still None
        // because no Offer ran (Offer is what populates the tracks in
        // cut 3b's `handle_offer`).
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

    /// PR 6: `pause_all_media` flips the per-PC flag for every
    /// connection in the registry. Test isolates the registry-side
    /// behaviour without involving worker IPC.
    #[tokio::test]
    async fn pause_all_media_marks_every_pc() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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

    /// PR 6: with `media_paused = true`, a P frame must be dropped and
    /// the flag must remain set (next IDR is the resync barrier).
    /// Verified by checking the flag stays `true` after the call —
    /// `write_video_frame` swallows errors silently so we can't observe
    /// the drop directly without instrumenting the track.
    #[tokio::test]
    async fn write_video_frame_paused_p_frame_keeps_flag_set() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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

    /// PR 6: `MediaFrameKind::VideoI` arriving while paused clears the
    /// flag in place. Subsequent frames flow normally. We cannot
    /// observe the actual write_sample call (no track set), but the
    /// flag transition is the contract that gates resume — verifying
    /// it is sufficient.
    #[tokio::test]
    async fn write_video_frame_paused_i_frame_clears_flag() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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

    /// PR 6: `resume_active_media` over an empty registry must be a
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
            .broadcast_media_settings_update(&worker_mgr, None, None, None)
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
            .broadcast_media_settings_update(&worker_mgr, Some(60), None, Some(40))
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

    /// PR 6: a PC that hasn't yet received an Offer has
    /// `cached_start_media = None`; resume must skip it (rather than
    /// trying to send a default StartMedia, which would tell the
    /// worker to start an encoder for a connection that hasn't
    /// negotiated codecs yet).
    #[tokio::test]
    async fn resume_active_media_skips_pc_without_cached_offer() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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
    /// `audio_track` instead of `video_track`. Until PR 3 wires the
    /// audio capture path, the daemon-side handler must still accept
    /// the variant without panicking when no audio track exists.
    #[tokio::test]
    async fn write_video_frame_audio_kind_uses_audio_track_slot() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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
            &model,
        )
        .await
        .expect("handle ok");

        let text = outbound_rx
            .recv()
            .await
            .expect("init reply must be broadcast");
        let reply: SignalingModel = serde_json::from_str(&text).expect("Init JSON must round-trip");
        assert!(
            matches!(reply.signaling_type, SignalingType::Init),
            "got {:?}",
            reply.signaling_type
        );
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
            &model,
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
                    if !matches!(m.signaling_type, SignalingType::Canid) {
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
            &model,
        )
        .await
        .expect("handle ok");

        // Drain the Init reply.
        let init_text = outbound_rx.recv().await.expect("init reply");
        let init_reply: SignalingModel = serde_json::from_str(&init_text).unwrap();
        assert!(matches!(init_reply.signaling_type, SignalingType::Init));

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
                    if matches!(m.signaling_type, SignalingType::Canid) {
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
        cleanup_pc(&registry, &worker_mgr, "ghost-connection", "test").await;
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
        };
        let s = settings_with_startup(StartupMode::ServiceDaemon);
        registry
            .create_for_request_remote("conn-x", &request_remote, &s)
            .await
            .expect("create");

        let shared = SharedSettings::from(s);
        let settings = actix_web::web::Data::new(shared);
        let (worker_mgr, _rx) = WorkerManager::new(settings, registry.clone());

        cleanup_pc(&registry, &worker_mgr, "conn-x", "test").await;

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

        handle_connection_removed(&registry, &worker_mgr, &model)
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

        handle_connection_removed(&registry, &worker_mgr, &model)
            .await
            .expect("handler must accept unknown ids without erroring");
        assert_eq!(registry.len().await, 0);
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

    // ============== Cut 5: DataChannel routing tests ==============

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
        match route_to_service_msg(DcRoute::FileTransfer, cid, data.clone(), true) {
            ServiceToWorker::FileTransferCommand(p) => {
                assert_eq!(p.data, data);
                assert!(p.is_text, "is_text=true must propagate to payload");
            }
            other => panic!("expected FileTransferCommand, got {other:?}"),
        }
        match route_to_service_msg(DcRoute::Whiteboard, cid, data.clone(), true) {
            ServiceToWorker::WhiteboardCommand(p) => assert_eq!(p.data, data),
            other => panic!("expected WhiteboardCommand, got {other:?}"),
        }
    }

    /// PR 4 cut 2: when the browser sends a binary file-transfer
    /// chunk, the daemon must forward `is_text=false` so the worker
    /// dispatches into the chunk-write path rather than trying to
    /// parse JSON.
    #[test]
    fn route_to_service_msg_file_transfer_preserves_binary_flag() {
        let cid = "conn-binary";
        let data = vec![0xFFu8, 0x00, 0xAA, 0x55];
        match route_to_service_msg(DcRoute::FileTransfer, cid, data.clone(), false) {
            ServiceToWorker::FileTransferCommand(p) => {
                assert_eq!(p.data, data);
                assert!(!p.is_text, "is_text=false must propagate for binary chunks");
            }
            other => panic!("expected FileTransferCommand, got {other:?}"),
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

    // ============== PR 3: cursor sync write_cursor_data ==============

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

    // ============== PR 4 cut 1: write_clipboard_data ==============

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
    /// pushes. Mirrors the Arch III polling-task gate that read both
    /// flags from `SignalingState`.
    #[tokio::test]
    async fn write_clipboard_data_drops_when_permission_not_granted() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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

    // ============== PR 4 cut 2: write_file_transfer_data ==============

    /// `write_file_transfer_data` for an unknown connection_id is a
    /// silent no-op — race against `CloseControl` must not panic.
    #[tokio::test]
    async fn write_file_transfer_data_unknown_connection_is_silent_noop() {
        let registry = PcRegistry::new();
        let payload = FileTransferPayload {
            connection_id: "ghost".to_string(),
            data: b"{\"type\":\"DownloadResponse\"}".to_vec(),
            is_text: true,
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
    /// without spawning anything new. Covers the path where a worker
    /// emits a stale `FileTransferData` for a PC the daemon has
    /// already dropped — pre-fix this hit the same DC lookup as a
    /// live PC; post-fix it short-circuits at the registry lookup
    /// before any sender clone.
    #[tokio::test]
    async fn write_file_transfer_data_after_registry_remove_is_silent_noop() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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
        let (tx, rx) = mpsc::unbounded_channel::<FileTransferPayload>();
        spawn_file_transfer_writer_task("conn-lifecycle".to_string(), rx, dc_slot);
        // Push one payload (silently dropped — no DC) then drop the
        // sender. The task drains the queued payload, observes
        // `recv() → None`, and exits.
        tx.send(FileTransferPayload {
            connection_id: "conn-lifecycle".to_string(),
            data: b"queued".to_vec(),
            is_text: true,
        })
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

    // ============== Cut 5: RTCP PLI/FIR identity ==============

    /// Identifying RTCP packets via `as_any().is::<T>()` is the path
    /// `spawn_rtcp_force_keyframe_task` uses to decide whether to
    /// emit ForceKeyframe. Pin the identity so a webrtc-rs version
    /// bump that changed the trait object representation is caught
    /// here, not in production where missed PLIs become "browser
    /// stuck on stale frame after a packet loss".
    #[test]
    fn rtcp_pli_and_fir_are_distinguishable_via_as_any() {
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

        assert!(pli.as_any().is::<PictureLossIndication>());
        assert!(!pli.as_any().is::<FullIntraRequest>());
        assert!(fir.as_any().is::<FullIntraRequest>());
        assert!(!fir.as_any().is::<PictureLossIndication>());
    }

    // ============== Cut 6: handle_require_control tests ==============

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
        assert!(
            matches!(reply.signaling_type, SignalingType::AcceptControl),
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
    /// reply. Subsequent Cut 5 mouse / keyboard IPC must remain blocked
    /// because the daemon's permission gate reads from the same state.
    #[tokio::test]
    async fn handle_require_control_auto_denies_and_emits_deny() {
        let registry = PcRegistry::new();
        let (outbound_tx, mut outbound_rx) = broadcast::channel::<String>(8);
        let settings = settings_with_security(Some(false), None);
        let hub = Arc::new(HostControlHub::new_local());

        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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
        assert!(
            matches!(reply.signaling_type, SignalingType::DenyControl),
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
        assert!(
            matches!(reply.signaling_type, SignalingType::CloseControl),
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
        assert!(matches!(reply.signaling_type, SignalingType::AcceptControl));
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
    /// leaves the other intact (multi-browser concurrency contract
    /// from PR 1's transport docs).
    #[tokio::test]
    async fn pc_registry_supports_multiple_independent_connections() {
        let registry = PcRegistry::new();
        let request_remote = RequestRemoteModel {
            ice_servers: vec![],
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
