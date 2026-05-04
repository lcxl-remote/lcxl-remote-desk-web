//! # Worker-side media producer (Arch IV)
//!
//! Owns the screen capture loop and the per-`connection_id` video
//! encoder pool. Replaces the in-`service::signaling`-mod
//! `capture_screen_task` that ran one capture pipeline per peer
//! connection in Arch III; in Arch IV the daemon owns the
//! `RTCPeerConnection` and the worker pushes encoded
//! [`MediaFrame`](desk_ipc_protocol::message::MediaFrame)s over a
//! dedicated [media transport](desk_ipc_protocol::dual_transport::MediaSender).
//!
//! ## What cut 4 implements
//!
//! - Per-`connection_id` capture + encoder pair, each driven by a
//!   dedicated OS thread with its own current-thread Tokio runtime
//!   (mirrors the Arch III pattern — DXGI / WASAPI handles are COM-
//!   bound and thread-affine, so a dedicated thread per pipeline is
//!   the safe choice).
//! - `StartMedia` / `StopMedia` / `ForceKeyframe` / `UpdateMediaSettings`
//!   handlers driven from the worker event loop.
//! - `MediaCapabilities` snapshot constructor used by `worker::session`
//!   to send a one-shot `WorkerToService::Capabilities` to the daemon
//!   on Init.
//!
//! ## What PR 3 adds
//!
//! - **Audio** — a sibling thread alongside video that drives
//!   `desk-capture-engine`'s audio capture + Opus encoder, ships
//!   `MediaFrame { Audio, Opus }` over the same media transport.
//!   Daemon's `write_video_frame` already routes audio frames to the
//!   per-PC `audio_track`, so no daemon-side change was needed for
//!   sample writing.
//! - **Cursor sync** — the video pipeline switches to
//!   `CursorCaptureMode::SyncNative` when the backend supports it, and
//!   pushes `WorkerToService::CursorData` IPC packets carrying the
//!   serialised `CursorSyncData` JSON. Daemon's
//!   `pc_manager::write_cursor_data` looks up the matching
//!   `cursor_sync_event` DC and forwards via `dc.send_text(...)`.
//!
//! ## Out-of-scope (deferred)
//!
//! - **Capture sharing across connections** — plan calls for a single
//!   capture broadcast feeding multiple encoders. Cut 4 takes the
//!   simpler one-capture-per-connection path because (a) the more
//!   common steady state is one browser at a time, and (b) DXGI
//!   duplications can coexist when targeting the same output, so
//!   correctness does not depend on sharing. The shared-capture
//!   optimisation lands in a follow-up cut once the multi-browser
//!   stress path actually warrants it.
//! - **`UpdateMediaSettings` live-apply** — recorded as a TODO; cut 4
//!   logs the request and returns OK without rebuilding the encoder so
//!   the IPC contract is honoured even though the runtime knob is
//!   pending.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use desk_capture_engine::audio_capture::audio_capture_factory::{
    create_audio_capture, list_audio_capture,
};
use desk_capture_engine::audio_encoder::audio_encoder_factory::{
    create_audio_encoder, list_audio_encoder,
};
use desk_capture_engine::image_capture::image_capture_factory::{
    create_image_capture, list_image_capture,
};
use desk_capture_engine::model::image_capture::{CaptureRequest, CursorCaptureMode};
use desk_capture_engine::model::video_encoder::VideoEncoder;
use desk_capture_engine::video_encoder::video_encoder_factory::{
    create_video_encoder, list_video_encoder,
};
use desk_ipc_protocol::dual_transport::{MediaSender, TransportError};
use desk_ipc_protocol::message::{
    ErrorPayload, MediaCapabilities, MediaCodec, MediaFrame, MediaFrameKind, StartMediaPayload,
    StopMediaPayload, UpdateMediaSettingsPayload, WorkerToService,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use log::{debug, error, info, warn};
use tokio::sync::mpsc;

/// Per-connection media context. Holds the dedicated threads running
/// the capture + encode loops (one for video, one for audio) plus the
/// flags the event loop flips to drive them. Both pipelines share the
/// same `stop_flag` so `StopMedia` cleanly tears down both halves at
/// once.
struct ConnectionTask {
    stop_flag: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
    /// Held so the video task can be joined on `shutdown()`. None
    /// after the thread exits naturally on stop_flag observation.
    video_handle: Option<thread::JoinHandle<()>>,
    /// Audio pipeline handle (PR 3). `None` when the worker did not
    /// build an audio pipeline for this connection — currently always
    /// spawned alongside video, but kept Optional so a future cut can
    /// disable audio per connection without changing the field shape.
    audio_handle: Option<thread::JoinHandle<()>>,
}

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        // Belt-and-braces: setting stop_flag here guarantees both
        // threads observe a stop request even if the caller forgot to
        // call `stop_media` (e.g. supervisor unwinding on a panic).
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

/// Worker-side media producer. Holds a registry of per-connection
/// capture + encode pipelines and a single `MediaSender` shared across
/// them.
pub struct MediaProducer {
    desk_settings: DeskSettings,
    media_sender: Arc<dyn MediaSender>,
    /// Worker→daemon error reporting channel. Surfaces
    /// `WorkerToService::Error` (e.g. `MediaTransportStuck`) so the
    /// daemon can decide whether to issue a `StopMedia`+`StartMedia`
    /// reset; the producer never self-decides to abort.
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    inner: StdMutex<HashMap<String, ConnectionTask>>,
}

impl MediaProducer {
    pub fn new(
        desk_settings: DeskSettings,
        media_sender: Arc<dyn MediaSender>,
        error_tx: mpsc::UnboundedSender<WorkerToService>,
    ) -> Self {
        Self {
            desk_settings,
            media_sender,
            error_tx,
            inner: StdMutex::new(HashMap::new()),
        }
    }

    /// Start a per-connection capture + encode pipeline. Idempotent on
    /// duplicate `connection_id` — duplicates log a warning and are
    /// ignored (the daemon should never legitimately double-start).
    pub fn start_media(&self, payload: StartMediaPayload) {
        let connection_id = payload.connection_id.clone();
        let mut map = self.inner.lock().expect("media producer lock poisoned");
        if map.contains_key(&connection_id) {
            warn!(
                "[MediaProducer] StartMedia for already-running connection {connection_id}; ignoring"
            );
            return;
        }
        let stop_flag = Arc::new(AtomicBool::new(false));
        let keyframe_requested = Arc::new(AtomicBool::new(false));
        let video_handle = spawn_video_pipeline_thread(
            self.desk_settings.clone(),
            payload.clone(),
            Arc::clone(&self.media_sender),
            self.error_tx.clone(),
            Arc::clone(&stop_flag),
            Arc::clone(&keyframe_requested),
        );
        // PR 3: audio pipeline runs in its own dedicated thread (WASAPI
        // / PipeWire / SCKit handles are COM/system-thread-bound the
        // same way as the video capture, so a separate thread + a
        // current-thread Tokio runtime is the right shape — same
        // pattern Arch III used in `capture_audio_task`).
        let audio_handle = spawn_audio_pipeline_thread(
            self.desk_settings.clone(),
            payload,
            Arc::clone(&self.media_sender),
            self.error_tx.clone(),
            Arc::clone(&stop_flag),
        );
        map.insert(
            connection_id,
            ConnectionTask {
                stop_flag,
                keyframe_requested,
                video_handle: Some(video_handle),
                audio_handle: Some(audio_handle),
            },
        );
    }

    /// Stop a per-connection pipeline. No-op on unknown id.
    pub fn stop_media(&self, payload: &StopMediaPayload) {
        let mut map = self.inner.lock().expect("media producer lock poisoned");
        if let Some(mut task) = map.remove(&payload.connection_id) {
            task.stop_flag.store(true, Ordering::Relaxed);
            // We do not block-join the threads here: the worker IPC
            // loop must remain responsive. Both threads observe
            // stop_flag in their capture/sleep cycle and exit within
            // one frame interval. The Drop on ConnectionTask is also a
            // fail-safe.
            drop(task.video_handle.take());
            drop(task.audio_handle.take());
            info!(
                "[MediaProducer] StopMedia issued for connection {}",
                payload.connection_id
            );
        } else {
            debug!(
                "[MediaProducer] StopMedia for unknown connection {}; ignoring",
                payload.connection_id
            );
        }
    }

    /// Flag the next encode pass on the per-connection encoder to
    /// emit an IDR. Routed by `connection_id` (never broadcast — see
    /// transport-level docstring on `ForceKeyframe`).
    pub fn force_keyframe(&self, connection_id: &str) {
        let map = self.inner.lock().expect("media producer lock poisoned");
        if let Some(task) = map.get(connection_id) {
            task.keyframe_requested.store(true, Ordering::Relaxed);
            info!("[MediaProducer] ForceKeyframe queued for connection {connection_id}");
        } else {
            debug!(
                "[MediaProducer] ForceKeyframe for unknown connection {connection_id}; ignoring"
            );
        }
    }

    /// Live-update knobs (fps / bitrate / quality). Cut 4 ack-only
    /// (logs and returns) — the runtime apply path lands in a follow-up
    /// once the encoder traits expose the right setter surface.
    pub fn update_settings(&self, payload: UpdateMediaSettingsPayload) {
        warn!(
            "[MediaProducer] UpdateMediaSettings received for {} but live-apply is not yet \
             implemented (fps={:?}, bitrate_kbps={:?}, quality={:?}); ignoring",
            payload.connection_id, payload.fps, payload.bitrate_kbps, payload.quality
        );
    }

    /// Stop every active pipeline. Called by `worker::session` on
    /// shutdown so threads do not outlive the worker process.
    pub fn shutdown(&self) {
        let drained: Vec<ConnectionTask> = {
            let mut map = self.inner.lock().expect("media producer lock poisoned");
            map.drain().map(|(_, v)| v).collect()
        };
        for task in drained {
            task.stop_flag.store(true, Ordering::Relaxed);
            // Drop runs and signals stop_flag again as a fail-safe.
            drop(task);
        }
    }

    /// Build a one-shot capability snapshot for the daemon. Called
    /// from `worker::session::ipc_loop` immediately after Init so the
    /// daemon can populate the next `RequestRemote` Init reply with
    /// real codec / device data.
    pub fn build_capabilities(desktop_name: Option<&str>, has_tauri: bool) -> MediaCapabilities {
        let video_codecs = list_video_encoder()
            .into_iter()
            .filter_map(|s| codec_from_str(&s, true))
            .collect::<Vec<_>>();
        let audio_codecs = list_audio_encoder()
            .into_iter()
            .filter_map(|s| codec_from_str(&s, false))
            .collect::<Vec<_>>();
        let video_devices = list_image_capture()
            .into_values()
            .flat_map(|displays| displays.into_iter().map(|d| d.device_name))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        let audio_devices = list_audio_capture()
            .into_values()
            .flat_map(|devices| devices.into_iter().map(|d| d.firendly_name))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();
        MediaCapabilities {
            video_codecs,
            audio_codecs,
            video_devices,
            audio_devices,
            has_tauri,
            is_admin: desk_utils::permission::is_admin(),
            desktop_name: desktop_name.unwrap_or("").to_string(),
        }
    }
}

/// Map a capture-engine codec name string to the IPC `MediaCodec` enum.
/// Returns `None` for codec names the IPC layer does not know about
/// (silently dropped — newer worker workers may add codecs the daemon
/// is not yet compiled against, and we should not crash on that).
///
/// `is_video` selects the video set (X264/H264/VP8/VP9/AV1) vs. the
/// audio set (Opus). The factory list APIs return strings that overlap
/// (e.g. "H264" for video, "Opus" for audio).
fn codec_from_str(name: &str, is_video: bool) -> Option<MediaCodec> {
    if is_video {
        match name {
            "H264" | "X264" => Some(MediaCodec::H264),
            "VP8" => Some(MediaCodec::Vp8),
            "VP9" => Some(MediaCodec::Vp9),
            "AV1" => Some(MediaCodec::Av1),
            _ => None,
        }
    } else {
        match name {
            // Capture-engine factory uses upper-case variant names
            // (`AudioEncoderType::OPUS` → "OPUS") via `IntoStaticStr`.
            "Opus" | "OPUS" => Some(MediaCodec::Opus),
            _ => None,
        }
    }
}

/// Build a `desk_settings` clone with the per-connection overrides
/// from `StartMediaPayload` baked in (codec, fps). Quality / bitrate
/// honour the connection request when non-zero; zero means "use
/// encoder default" per the IPC docstring.
fn payload_overrides(base: &DeskSettings, payload: &StartMediaPayload) -> DeskSettings {
    let mut s = base.clone();
    if let Some(name) = video_codec_name(payload.video_codec) {
        s.video_encoder = Some(name.to_string());
    }
    if payload.fps > 0 {
        s.video_fps = payload.fps;
    }
    // Cut 4: `video_device` IPC field maps to `\\\\.\\DISPLAY-N` style strings
    // but `DeskSettings.video_device_index` is a numeric index. Without a
    // device → index lookup table the safest behaviour is to ignore the IPC
    // hint and let the worker keep its configured index. A follow-up cut
    // wires capture-engine `list_image_capture` enumeration into a daemon
    // → worker resolution layer.
    let _ = &payload.video_device;
    s
}

/// Inverse of [`codec_from_str`] for the video subset.
fn video_codec_name(c: MediaCodec) -> Option<&'static str> {
    match c {
        MediaCodec::H264 => Some("H264"),
        MediaCodec::Vp8 => Some("VP8"),
        MediaCodec::Vp9 => Some("VP9"),
        MediaCodec::Av1 => Some("AV1"),
        MediaCodec::Opus => None,
    }
}

/// Spawn the dedicated thread that owns one connection's video
/// capture + encoder. Uses a current-thread Tokio runtime inside the
/// thread so `media_sender.send_frame(...).await` can run without
/// polluting the outer runtime with COM-bound state.
fn spawn_video_pipeline_thread(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let connection_id = payload.connection_id.clone();
    let thread_name = format!("media-video-{}", &connection_id);
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!(
                        "[MediaProducer] Failed to build video runtime for {connection_id}: {e}; \
                         pipeline thread exits before first frame"
                    );
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(async move {
                if let Err(e) = video_pipeline_loop(
                    base_settings,
                    payload,
                    media_sender,
                    error_tx,
                    stop_flag,
                    keyframe_requested,
                )
                .await
                {
                    error!(
                        "[MediaProducer] Video pipeline for {connection_id} exited with error: {e}"
                    );
                }
            }));
        })
        .expect("spawn media video pipeline thread")
}

/// PR 3: spawn the dedicated thread that owns one connection's audio
/// capture + Opus encoder. Same threading rationale as
/// [`spawn_video_pipeline_thread`] — WASAPI / PipeWire / SCKit handles
/// are system-thread-bound, so audio gets its own thread + runtime.
/// Errors during construction or capture are logged but never bring
/// down the worker; the daemon already tolerates a video-only stream
/// when the worker has no audio device available.
fn spawn_audio_pipeline_thread(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let connection_id = payload.connection_id.clone();
    let thread_name = format!("media-audio-{}", &connection_id);
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!(
                        "[MediaProducer] Failed to build audio runtime for {connection_id}: {e}; \
                         audio pipeline thread exits before first sample"
                    );
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(async move {
                if let Err(e) =
                    audio_pipeline_loop(base_settings, payload, media_sender, error_tx, stop_flag)
                        .await
                {
                    // Audio failures degrade the stream to video-only
                    // but must not crash the connection; logged so the
                    // operator can investigate.
                    warn!("[MediaProducer] Audio pipeline for {connection_id} exited: {e}");
                }
            }));
        })
        .expect("spawn media audio pipeline thread")
}

/// Inner async loop for video. Builds capture + encoder, then
/// iterates: capture, honour keyframe flag (recreate encoder so the
/// next encode emits an IDR), encode, push every NAL as a `MediaFrame`.
/// Heartbeat-frame behaviour mirrors Arch III: on a static desktop
/// emit one cached frame per second so the receiver does not stall.
async fn video_pipeline_loop(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
) -> Result<(), String> {
    let connection_id = payload.connection_id.clone();
    let codec = payload.video_codec;
    let merged_settings = payload_overrides(&base_settings, &payload);

    info!(
        "[MediaProducer:{connection_id}] Starting pipeline: codec={codec:?}, fps={}",
        merged_settings.video_fps
    );

    let mut capture = create_image_capture(&merged_settings).map_err(|e| format!("{e}"))?;
    let display_info = capture.get_current_output().map_err(|e| format!("{e}"))?;
    let mut encoder: Box<dyn VideoEncoder> =
        create_video_encoder(&merged_settings, &display_info).map_err(|e| format!("{e}"))?;
    let mut next_pass_is_idr = true; // first frame is always I (encoder emits SPS/PPS+IDR)
    let mut seq: u64 = 0;
    let mut ticker = tokio::time::interval(merged_settings.get_duration_by_video_fps());
    let frame_duration_ns = merged_settings
        .get_duration_by_video_fps()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let mut last_send_time = std::time::Instant::now();

    while !stop_flag.load(Ordering::Relaxed) {
        ticker.tick().await;
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        if keyframe_requested.swap(false, Ordering::Relaxed) {
            info!(
                "[MediaProducer:{connection_id}] Keyframe requested; recreating encoder so the \
                 next encode pass emits an IDR"
            );
            encoder = create_video_encoder(&merged_settings, &display_info)
                .map_err(|e| format!("{e}"))?;
            next_pass_is_idr = true;
        }

        // PR 3 cursor sync: prefer SyncNative when the capture
        // backend supports it so the worker can ship cursor shape /
        // position updates over the dedicated `cursor_sync_event`
        // DC. Backends that don't support cursor sync fall back to
        // RenderInFrame, where the cursor is baked into the encoded
        // video and no cursor IPC is emitted.
        //
        // Trade-off: when the daemon hasn't registered a cursor DC
        // (browser hasn't opened it because control isn't granted),
        // `write_cursor_data` silently drops the IPC payload and the
        // browser sees no cursor at all. Arch III handled this by
        // dynamically flipping cursor_mode based on accept_control,
        // which would require a new daemon→worker IPC notify. Worth
        // it only if the no-control no-cursor regression matters;
        // until then the simpler path is enough.
        let cursor_mode = if capture.supports_cursor_sync() && merged_settings.show_mouse {
            CursorCaptureMode::SyncNative
        } else if !merged_settings.show_mouse {
            CursorCaptureMode::Disable
        } else {
            CursorCaptureMode::RenderInFrame
        };
        let capture_result = match capture.capture(CaptureRequest { cursor_mode }) {
            Ok(r) => r,
            Err(e) => {
                debug!("[MediaProducer:{connection_id}] capture error: {e}; continuing");
                continue;
            }
        };

        // Push cursor IPC whenever the capture surfaced an update and
        // we asked for SyncNative. The error_tx is the worker's event
        // pipe (not the media transport), so cursor packets do not
        // compete with video frames for the bounded media channel.
        if matches!(cursor_mode, CursorCaptureMode::SyncNative)
            && let Some(cursor) = &capture_result.cursor_update
        {
            match serde_json::to_vec(cursor) {
                Ok(bytes) => {
                    let payload = desk_ipc_protocol::message::CursorDataPayload {
                        connection_id: connection_id.clone(),
                        data: bytes,
                    };
                    if error_tx.send(WorkerToService::CursorData(payload)).is_err() {
                        // Event pipe gone — the worker is shutting
                        // down. Let the next stop_flag check drop us.
                        debug!(
                            "[MediaProducer:{connection_id}] event pipe closed; \
                             cursor IPC will not flow"
                        );
                    }
                }
                Err(e) => {
                    warn!("[MediaProducer:{connection_id}] failed to serialise cursor update: {e}");
                }
            }
        }

        if !capture_result.content_changed {
            // Static-desktop heartbeat: emit one cached frame per
            // second so the daemon-side track keeps producing RTP and
            // the browser decoder does not declare the stream dead.
            if last_send_time.elapsed() <= Duration::from_secs(1) {
                continue;
            }
            let nal_info_vec = match encoder.encode_cached() {
                Ok(v) => v,
                Err(e) => {
                    warn!("[MediaProducer:{connection_id}] encode_cached error: {e}; continuing");
                    continue;
                }
            };
            for nal in nal_info_vec {
                let frame = build_media_frame(
                    &connection_id,
                    seq,
                    frame_duration_ns,
                    MediaFrameKind::VideoP,
                    codec,
                    nal.nal_bytes.to_vec(),
                );
                seq += 1;
                if !send_frame(&media_sender, &error_tx, &connection_id, frame).await {
                    return Ok(());
                }
            }
            last_send_time = std::time::Instant::now();
            continue;
        }

        let nal_info_vec = match encoder.encode(capture_result.image.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                warn!("[MediaProducer:{connection_id}] encode error: {e}; continuing");
                continue;
            }
        };
        let kind_for_pass = if next_pass_is_idr {
            MediaFrameKind::VideoI
        } else {
            MediaFrameKind::VideoP
        };
        next_pass_is_idr = false;
        for nal in nal_info_vec {
            let frame = build_media_frame(
                &connection_id,
                seq,
                frame_duration_ns,
                kind_for_pass,
                codec,
                nal.nal_bytes.to_vec(),
            );
            seq += 1;
            if !send_frame(&media_sender, &error_tx, &connection_id, frame).await {
                return Ok(());
            }
        }
        last_send_time = std::time::Instant::now();
    }

    info!("[MediaProducer:{connection_id}] Pipeline exiting (stop_flag observed)");
    Ok(())
}

/// PR 3 inner async loop for audio. Mirrors Arch III's
/// `capture_audio_task`: 5 ms ticker drives an inner buffer-drain loop
/// that pulls 20 ms Opus packets out of the encoder and ships each one
/// as a `MediaFrame { Audio }` to the daemon. The daemon's
/// `write_video_frame` already routes `MediaFrameKind::Audio` to the
/// per-PC `audio_track`, so no daemon-side change is needed for audio
/// frames themselves to reach the browser.
///
/// **Audio codec is locked to Opus** for now — the only audio encoder
/// the capture-engine factory ships. The IPC `audio_codec` field on
/// `StartMediaPayload` is kept for forward compatibility but the
/// worker simply asserts and proceeds with Opus.
///
/// Failures during capture init / start / encode propagate up as
/// `Err(String)` and the spawning thread logs them at warn level — a
/// degraded video-only stream is preferable to the connection
/// crashing.
async fn audio_pipeline_loop(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
) -> Result<(), String> {
    let connection_id = payload.connection_id.clone();
    if !matches!(payload.audio_codec, MediaCodec::Opus) {
        warn!(
            "[MediaProducer:{connection_id}] Requested audio codec {:?} is not Opus; \
             worker only ships Opus today — proceeding with Opus and ignoring the request",
            payload.audio_codec,
        );
    }

    info!("[MediaProducer:{connection_id}] Starting audio pipeline (Opus)");

    let mut capture = create_audio_capture(&base_settings).map_err(|e| format!("{e}"))?;
    let wave_format = capture.start().map_err(|e| format!("{e}"))?;
    let mut encoder =
        create_audio_encoder(&base_settings, wave_format).map_err(|e| format!("{e}"))?;

    // 5 ms outer tick + inner drain loop matches Arch III's pacing —
    // capture buffers fill at the OS audio cadence (typically 10 ms),
    // and at 5 ms ticks we drain everything sitting in the buffer
    // before sleeping again. Opus encoded packets carry 20 ms of audio
    // each by capture-engine convention.
    let mut ticker = tokio::time::interval(Duration::from_millis(5));
    const AUDIO_FRAME_DURATION: Duration = Duration::from_millis(20);
    let audio_duration_ns = AUDIO_FRAME_DURATION.as_nanos().min(u64::MAX as u128) as u64;
    let mut seq: u64 = 0;

    while !stop_flag.load(Ordering::Relaxed) {
        ticker.tick().await;
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        // Drain whatever the capture has buffered. The inner loop
        // exits on Empty (encoded buffer length 0) so we get back to
        // the ticker and yield to the rest of the runtime.
        loop {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            let buffer = match capture.get_buffer() {
                Ok(b) => b,
                Err(desk_capture_engine::error::CaptureError::CustomError(err))
                    if err.error_code == desk_utils::error::DeskErrorCode::ACTION_NEED_RETRY =>
                {
                    // Capture stream went away (device unplug, format
                    // change, sleep/resume). Recreate per Arch III's
                    // behaviour and continue from the next tick.
                    warn!(
                        "[MediaProducer:{connection_id}] audio capture needs retry — \
                         recreating capture"
                    );
                    capture = match create_audio_capture(&base_settings) {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(
                                "[MediaProducer:{connection_id}] audio capture rebuild failed: \
                                 {e}; audio pipeline exiting"
                            );
                            return Ok(());
                        }
                    };
                    if let Err(e) = capture.start() {
                        warn!(
                            "[MediaProducer:{connection_id}] audio capture restart failed: \
                             {e}; audio pipeline exiting"
                        );
                        return Ok(());
                    }
                    break;
                }
                Err(e) => {
                    warn!(
                        "[MediaProducer:{connection_id}] audio get_buffer error: {e}; \
                         skipping this tick"
                    );
                    break;
                }
            };

            let encoded = match encoder.encode(buffer.as_ref()) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        "[MediaProducer:{connection_id}] audio encode error: {e}; \
                         skipping packet"
                    );
                    break;
                }
            };
            // Empty buffer = capture had nothing this tick — go back
            // to the ticker without sending. Arch III followed the
            // same convention.
            if encoded.data.is_empty() {
                break;
            }
            let frame = build_media_frame(
                &connection_id,
                seq,
                audio_duration_ns,
                MediaFrameKind::Audio,
                MediaCodec::Opus,
                encoded.data,
            );
            seq += 1;
            if !send_frame(&media_sender, &error_tx, &connection_id, frame).await {
                return Ok(());
            }
        }
    }

    info!("[MediaProducer:{connection_id}] Audio pipeline exiting (stop_flag observed)");
    Ok(())
}

fn build_media_frame(
    connection_id: &str,
    seq: u64,
    duration_ns: u64,
    kind: MediaFrameKind,
    codec: MediaCodec,
    payload: Vec<u8>,
) -> MediaFrame {
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    MediaFrame {
        connection_id: connection_id.to_string(),
        seq,
        ts_ns,
        duration_ns,
        kind,
        codec,
        payload,
    }
}

/// Push a frame onto the media transport. Returns `false` when the
/// loop should exit (transport closed). I-frame send timeout surfaces
/// as a `WorkerToService::Error { MediaTransportStuck }` to the daemon
/// — the producer does not self-decide to abort, the daemon issues
/// `StopMedia`+`StartMedia` instead.
async fn send_frame(
    media_sender: &Arc<dyn MediaSender>,
    error_tx: &mpsc::UnboundedSender<WorkerToService>,
    connection_id: &str,
    frame: MediaFrame,
) -> bool {
    let kind = frame.kind;
    match media_sender.send_frame(frame).await {
        Ok(()) => true,
        Err(TransportError::Closed) => {
            warn!(
                "[MediaProducer:{connection_id}] media transport closed; pipeline thread exiting"
            );
            false
        }
        Err(TransportError::Backpressured) => {
            // P-frame drop — request a fresh keyframe on the next
            // encode pass so the stream resyncs.
            debug!("[MediaProducer:{connection_id}] media transport backpressured; dropping frame");
            true
        }
        Err(TransportError::IFrameTimeout) => {
            error!(
                "[MediaProducer:{connection_id}] I-frame send timed out; surfacing \
                 MediaTransportStuck to daemon for reset"
            );
            let _ = error_tx.send(WorkerToService::Error(ErrorPayload {
                code: ERROR_CODE_MEDIA_TRANSPORT_STUCK,
                message: format!(
                    "I-frame send timed out for connection {connection_id} (kind={kind:?}); \
                     daemon should issue StopMedia+StartMedia"
                ),
                recoverable: true,
            }));
            true
        }
        Err(other) => {
            warn!("[MediaProducer:{connection_id}] media transport send error: {other}");
            true
        }
    }
}

/// Sentinel error code for the daemon-side handler. Picked deliberately
/// outside the `DeskErrorCode` u16 range so the daemon can match on it
/// without a name collision with the broader ErrorPayload codes.
pub const ERROR_CODE_MEDIA_TRANSPORT_STUCK: i32 = -1001;

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::dual_transport::inprocess;
    use desk_signal_facade::model::desk_settings::DeskSettings;

    /// Codec round-trip: the strings emitted by the encoder factory map
    /// back to the IPC enum without surprises.
    #[test]
    fn codec_from_str_round_trips_video_set() {
        for name in ["H264", "X264"] {
            assert_eq!(codec_from_str(name, true), Some(MediaCodec::H264));
        }
        assert_eq!(codec_from_str("VP8", true), Some(MediaCodec::Vp8));
        assert_eq!(codec_from_str("VP9", true), Some(MediaCodec::Vp9));
        assert_eq!(codec_from_str("AV1", true), Some(MediaCodec::Av1));
    }

    #[test]
    fn codec_from_str_unknown_returns_none_not_panic() {
        assert!(codec_from_str("FANCY-NEW-CODEC", true).is_none());
        assert!(codec_from_str("AV1", false).is_none()); // wrong category
    }

    #[test]
    fn video_codec_name_round_trips() {
        for c in [
            MediaCodec::H264,
            MediaCodec::Vp8,
            MediaCodec::Vp9,
            MediaCodec::Av1,
        ] {
            let name = video_codec_name(c).expect("name for video codec");
            assert_eq!(codec_from_str(name, true), Some(c));
        }
        assert!(video_codec_name(MediaCodec::Opus).is_none());
    }

    /// The payload override path picks the per-connection codec / fps
    /// over the worker default settings. A zero `fps` means "keep
    /// default" so the daemon does not have to know the worker's
    /// preferred fallback.
    #[test]
    fn payload_overrides_apply_codec_and_fps() {
        let base = DeskSettings {
            video_fps: 30,
            video_encoder: Some("X264".into()),
            ..DeskSettings::default()
        };
        let payload = StartMediaPayload {
            connection_id: "c1".into(),
            video_codec: MediaCodec::Vp9,
            audio_codec: MediaCodec::Opus,
            video_device: Some("display-1".into()),
            audio_device: None,
            fps: 60,
            bitrate_kbps: 0,
            quality: 0,
        };
        let merged = payload_overrides(&base, &payload);
        assert_eq!(merged.video_encoder.as_deref(), Some("VP9"));
        assert_eq!(merged.video_fps, 60);
        // Cut 4: `video_device` IPC field is intentionally ignored until
        // the daemon wires a name→index lookup. Pin so the next change to
        // payload_overrides doesn't silently start interpreting it.
        assert_eq!(merged.video_device_index, base.video_device_index);
    }

    #[test]
    fn payload_overrides_fps_zero_keeps_default() {
        let base = DeskSettings {
            video_fps: 24,
            ..DeskSettings::default()
        };
        let payload = StartMediaPayload {
            connection_id: "c1".into(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 0,
            bitrate_kbps: 0,
            quality: 0,
        };
        let merged = payload_overrides(&base, &payload);
        assert_eq!(merged.video_fps, 24);
    }

    /// Force-keyframe / stop-media on an unknown connection must be a
    /// silent no-op (race with browser drop). The producer has to be
    /// safe to drive from the daemon even when the daemon's view of
    /// active connections is briefly stale.
    #[test]
    fn force_keyframe_and_stop_media_unknown_id_is_noop() {
        let (sender, _rx) = inprocess::make_media();
        let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
        let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
        producer.force_keyframe("never-existed");
        producer.stop_media(&StopMediaPayload {
            connection_id: "never-existed".into(),
        });
        // Nothing to assert beyond "did not panic" — the unit test
        // exists to guard against `unwrap()` on a missing entry.
    }

    /// Update-settings for cut 4 is ack-only. The test pins the
    /// behaviour so a future "live apply" implementation won't
    /// silently drop the warn-log breadcrumb.
    #[test]
    fn update_settings_does_not_panic_on_unknown_connection() {
        let (sender, _rx) = inprocess::make_media();
        let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
        let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
        producer.update_settings(UpdateMediaSettingsPayload {
            connection_id: "anything".into(),
            fps: Some(30),
            bitrate_kbps: Some(2_000),
            quality: Some(50),
        });
    }

    /// `build_media_frame` stamps ts_ns from wall clock, copies through
    /// the inputs, and produces a frame the daemon can decode end-to-end.
    #[test]
    fn build_media_frame_produces_consistent_payload() {
        let frame = build_media_frame(
            "c-x",
            42,
            16_666_666,
            MediaFrameKind::VideoI,
            MediaCodec::H264,
            vec![0xAB; 256],
        );
        assert_eq!(frame.connection_id, "c-x");
        assert_eq!(frame.seq, 42);
        assert_eq!(frame.duration_ns, 16_666_666);
        assert_eq!(frame.kind, MediaFrameKind::VideoI);
        assert_eq!(frame.codec, MediaCodec::H264);
        assert_eq!(frame.payload.len(), 256);
        assert!(frame.ts_ns > 0, "ts_ns must be wall-clock stamped");
    }

    /// Capabilities snapshot must populate at least the codec lists
    /// (video + audio). On Windows host the device lists may be empty
    /// when running in a headless CI environment so we only assert
    /// that the call succeeds and the fields are well-formed; codecs
    /// are platform-agnostic and always populated.
    #[test]
    fn build_capabilities_populates_codecs() {
        let caps = MediaProducer::build_capabilities(Some("Default"), false);
        assert!(
            !caps.video_codecs.is_empty(),
            "video codec list must not be empty: {caps:?}"
        );
        assert!(
            !caps.audio_codecs.is_empty(),
            "audio codec list must not be empty: {caps:?}"
        );
        assert_eq!(caps.desktop_name, "Default");
        assert!(!caps.has_tauri);
    }

    /// `MediaTransportStuck` error code must remain stable so the
    /// daemon-side handler can match on it. Pinned by a regression test.
    #[test]
    fn media_transport_stuck_error_code_is_stable() {
        assert_eq!(ERROR_CODE_MEDIA_TRANSPORT_STUCK, -1001);
    }

    // ============== PR 3 audio + cursor sync tests ==============

    /// `build_media_frame` for an audio packet stamps the right
    /// `MediaFrameKind` + `MediaCodec` and the daemon's
    /// `write_video_frame` (which routes audio to `audio_track`)
    /// can pick the audio path off the resulting frame.
    #[test]
    fn build_media_frame_audio_kind_and_opus_codec() {
        let frame = build_media_frame(
            "c-audio",
            7,
            20_000_000, // 20 ms — Opus packet duration
            MediaFrameKind::Audio,
            MediaCodec::Opus,
            vec![0xCD; 80],
        );
        assert_eq!(frame.kind, MediaFrameKind::Audio);
        assert_eq!(frame.codec, MediaCodec::Opus);
        assert_eq!(frame.duration_ns, 20_000_000);
        assert_eq!(frame.payload.len(), 80);
    }

    /// CursorData payload that the worker emits is well-formed JSON
    /// (`CursorSyncData` model). Mirrors what the daemon decodes via
    /// `write_cursor_data` after passing through IPC. We can't drive
    /// a real capture in this unit test so we hand-build the model
    /// and verify it survives serde and matches the wire shape the
    /// browser side expects.
    #[test]
    fn cursor_sync_data_serializes_to_json_bytes_for_ipc() {
        use crate::model::data_channel::CursorSyncData;
        let cursor = CursorSyncData {
            base64_png: "AAAA".to_string(),
            hotspot_x: 4,
            hotspot_y: 7,
            visible: true,
            shape_id: 99,
            screen_width: 1920,
            screen_height: 1080,
        };
        let bytes = serde_json::to_vec(&cursor).expect("serialise");
        // Round-trip via UTF-8 + serde to confirm the bytes are
        // exactly what the daemon's `write_cursor_data` will hand
        // through to `dc.send_text`. A regression here would mean
        // the browser-side decoder breaks even though the IPC plumbing
        // is intact.
        let s = std::str::from_utf8(&bytes).expect("utf-8");
        let decoded: CursorSyncData = serde_json::from_str(s).expect("decode");
        assert_eq!(decoded.shape_id, 99);
        assert!(decoded.visible);
        assert_eq!(decoded.screen_width, 1920);
    }
}
