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

use desk_signal_facade::model::signal::{
    LcxlRTCIceServer, OfferModel, RequestRemoteModel, SignalingModel, SignalingState,
    SignalingType, TurnTransport,
};
use desk_utils::error::{CustomDeskError, DeskErrorCode};
use tokio::sync::{RwLock, broadcast};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{
    MIME_TYPE_AV1, MIME_TYPE_H264, MIME_TYPE_OPUS, MIME_TYPE_VP8, MIME_TYPE_VP9, MediaEngine,
};
use webrtc::api::setting_engine::{SctpMaxMessageSize, SettingEngine};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::{RTCPeerConnection, configuration::RTCConfiguration};
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::daemon::worker_manager::WorkerManager;
use crate::error::DeskError;
use crate::model::settings::{Settings, StartupMode, TraversalMode};
use desk_capture_engine::audio_encoder::audio_encoder_factory::list_audio_encoder;
use desk_capture_engine::model::video_encoder::{VideoEncoderType, VideoEncoderTypeHelper};
use desk_capture_engine::video_encoder::video_encoder_factory::list_video_encoder;
use desk_ipc_protocol::message::{
    MediaCapabilities, MediaCodec, MediaFrame, MediaFrameKind, ServiceToWorker, StartMediaPayload,
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

/// Build an `RTCPeerConnection` with the lcxl-remote-desk daemon
/// defaults:
///
/// - 127.0.0.1 host candidate is included so loopback browser / Tauri
///   webview connections succeed via the local pair without requiring
///   cross-interface routing.
/// - SCTP `max_message_size_can_send` is set to `Unbounded` so large
///   DataChannel payloads (file-transfer, large clipboard) do not
///   fragment.
/// - Default codec set + default interceptor registry.
///
/// `ice_servers` is the already-filtered list (see
/// [`filter_ice_servers`]); pass `vec![]` for no ICE servers.
pub async fn build_peer_connection(
    ice_servers: Vec<RTCIceServer>,
) -> Result<RTCPeerConnection, DeskError> {
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Unbounded);
    setting_engine.set_include_loopback_candidate(true);

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
/// fills `cursor_data_channel` and the input DCs are wired up by the
/// daemon-side `on_data_channel` handler (also added in cut 5).
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

        let pc = build_peer_connection(filtered.iter().map(Into::into).collect()).await?;

        let ctx = Arc::new(RwLock::new(PeerConnectionContext {
            connection_id: connection_id.to_string(),
            pc: Arc::new(pc),
            signaling_state: Arc::new(RwLock::new(SignalingState::default())),
            video_track: None,
            audio_track: None,
        }));

        self.inner
            .write()
            .await
            .insert(connection_id.to_string(), Arc::clone(&ctx));

        Ok(ctx)
    }
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
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let request_remote = model.get_data::<RequestRemoteModel>()?;

    let _ctx = registry
        .create_for_request_remote(from_connection_id, &request_remote, settings)
        .await?;

    // Cut 4: populate the Init reply from the worker's
    // `WorkerToService::Capabilities` snapshot when available; fall
    // back to capture-engine's static factory enumerations for the
    // codec lists when the worker hasn't reported yet (first-Init
    // race window). Device lists stay empty in the fallback path —
    // those genuinely require a live worker enumeration.
    let (audio_encoder_list, video_encoder_list, is_admin_value) = if let Some(caps) = capabilities
    {
        (
            caps.audio_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>(),
            caps.video_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>(),
            caps.is_admin,
        )
    } else {
        (
            list_audio_encoder(),
            list_video_encoder(),
            desk_utils::permission::is_admin(),
        )
    };
    let init_data = InitSignalingData {
        ice_servers: vec![],
        user_name: user_name.to_string(),
        audio_device_list: std::collections::BTreeMap::new(),
        audio_encoder_list,
        video_device_list: std::collections::BTreeMap::new(),
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
        let _rtp_sender = ctx_guard
            .pc
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        ctx_guard.video_track = Some(video_track);
        // RTCP PLI/FIR reader spawns in cut 4 alongside the IPC
        // ForceKeyframe path — until then a missed keyframe just
        // shows up as a few stale-frame seconds, no IPC needed.
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
    };
    drop(ctx_guard);
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

    // Hold the read guard only as long as we need the track Arc; clone
    // it out before awaiting on `write_sample` so the daemon's offer /
    // canid handlers (which take the write lock) are not blocked while
    // the codec write completes.
    let track_opt = match frame.kind {
        MediaFrameKind::VideoI | MediaFrameKind::VideoP => ctx.read().await.video_track.clone(),
        MediaFrameKind::Audio => ctx.read().await.audio_track.clone(),
    };
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
    if let Some(ctx) = registry.remove(from_connection_id).await {
        let ctx = ctx.read().await;
        if let Err(e) = ctx.pc.close().await {
            log::warn!("[pc_manager] PC close failed for {from_connection_id}: {e}");
        }
        log::info!("[pc_manager] Closed PC for {from_connection_id}");
    } else {
        log::warn!("[pc_manager] CloseControl from {from_connection_id} but no PC in registry");
    }

    if let Err(e) = worker_mgr
        .send_to_worker(ServiceToWorker::StopMedia(
            desk_ipc_protocol::message::StopMediaPayload {
                connection_id: from_connection_id.to_string(),
            },
        ))
        .await
    {
        log::debug!("[pc_manager] StopMedia for {from_connection_id} could not reach worker: {e}");
    }
    Ok(())
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
        let pc = build_peer_connection(vec![]).await.expect("build pc");
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
            video_devices: vec![],
            audio_devices: vec![],
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

        handle_request_remote(&registry, &outbound_tx, &s, "user-x", false, None, &model)
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
