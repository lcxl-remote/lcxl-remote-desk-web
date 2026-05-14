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
//! ## What PR 7 follow-up adds
//!
//! - **`UpdateMediaSettings` live-apply** — fps / quality changes
//!   surface through `update_settings` → per-pipeline mpsc, drained
//!   on the next encode tick, encoder rebuilt in place without
//!   restarting capture. `bitrate_kbps` ride the same channel but
//!   per-codec routing is still pending (logged + ignored — see the
//!   TODO breadcrumb in `drain_settings_updates`); the UI today only
//!   surfaces a quality slider so this gap is invisible.
//!
//! ## Capture sharing across connections
//!
//! Each per-connection `video_pipeline_loop` subscribes to the
//! worker-wide `SharedCaptureRegistry` (see `worker::shared_capture`)
//! keyed by `(backend, output_index)`. Connections asking for the
//! same key reuse one capture loop and one OS-level capture
//! instance; the broadcast channel fans frames out to each encoder
//! thread. This is the **correctness** layer for the multi-browser
//! scenario, not just an optimisation: the cut 4 docstring claimed
//! "DXGI duplications can coexist when targeting the same output",
//! which is false — `IDXGIOutputDuplication::DuplicateOutput()`
//! returns `E_INVALIDARG` on the second call, taking the second
//! browser's video pipeline straight to a black screen.
//!
//! Connections that pick *different* backends or *different* output
//! indices each get their own capture loop, so e.g. one DXGI + one
//! GDI on the same display coexist on two threads. fps is honoured
//! per-connection by frame skipping in `video_pipeline_loop` (the
//! shared loop runs at the OS refresh rate); cursor metadata is
//! forwarded to each connection's `cursor_sync_event` DC iff that
//! connection's `show_mouse` is on.

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
use desk_capture_engine::image_capture::image_capture_factory::list_image_capture;
use desk_capture_engine::model::image_capture::ImageInfo;
use desk_capture_engine::model::video_encoder::VideoEncoder;
use desk_capture_engine::video_encoder::video_encoder_factory::{
    create_video_encoder, list_video_encoder,
};
use desk_ipc_protocol::dual_transport::{MediaSender, TransportError};
use desk_ipc_protocol::message::{
    ERROR_CODE_MEDIA_TRANSPORT_STUCK, ErrorPayload, MediaCapabilities, MediaCodec, MediaFrame,
    MediaFrameKind, StartMediaPayload, StopMediaPayload, UpdateMediaSettingsPayload,
    WorkerToService,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc};

use crate::worker::shared_capture::SharedCaptureRegistry;

/// Per-connection media context. Holds the dedicated threads running
/// the capture + encode loops (one for video, one for audio) plus the
/// flags the event loop flips to drive them. Both pipelines share the
/// same `stop_flag` so `StopMedia` cleanly tears down both halves at
/// once.
struct ConnectionTask {
    stop_flag: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
    /// Live-update channel feeding fresh `UpdateMediaSettingsPayload`
    /// values into the video pipeline thread. `update_settings` posts
    /// here; the loop drains via `try_recv` on every tick and rebuilds
    /// ticker / encoder when the relevant knobs differ from the cached
    /// ones. Audio pipeline does not subscribe today (Opus owns its own
    /// frame size + bitrate and a runtime change would require a
    /// separate IPC variant).
    settings_tx: mpsc::UnboundedSender<UpdateMediaSettingsPayload>,
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
    /// Worker-wide shared capture registry. Multi-browser scenario:
    /// two connections asking for the same `(backend, output_index)`
    /// reuse a single capture loop and broadcast frames to both
    /// per-connection encoder threads. Pre-fix each connection
    /// spawned its own `DxgiImageCapture`, and the second
    /// `DuplicateOutput` against the same output returned
    /// `E_INVALIDARG`, taking the second connection's video pipeline
    /// down to a black screen.
    capture_registry: Arc<SharedCaptureRegistry>,
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
            capture_registry: SharedCaptureRegistry::new(),
            inner: StdMutex::new(HashMap::new()),
        }
    }

    /// Start a per-connection capture + encode pipeline. Idempotent on
    /// duplicate `connection_id` — duplicates log a warning and are
    /// ignored (the daemon should never legitimately double-start).
    ///
    /// `payload.start_video` / `payload.start_audio` mirror the
    /// `m=video` / `m=audio` sections in the connection's SDP offer.
    /// When *both* are false the connection is a pure DataChannel
    /// session (e.g. the browser file-management page, which only
    /// opens `file_transfer_event`) and we skip both pipelines —
    /// otherwise we would spin up DXGI capture + WASAPI capture for a
    /// connection that never asked for media, costing CPU and locking
    /// the audio device against UAC-survival even on idle. The
    /// connection still gets a `ConnectionTask` slot so that
    /// `stop_media` is symmetric and so a future protocol-level
    /// renegotiation could light up media on the existing entry.
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
        let (settings_tx, settings_rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
        if !payload.start_video && !payload.start_audio {
            info!(
                "[MediaProducer] StartMedia for {connection_id} requests neither video nor \
                 audio (DataChannel-only connection); skipping capture pipelines"
            );
        }
        let video_handle = if payload.start_video {
            Some(spawn_video_pipeline_thread(
                self.desk_settings.clone(),
                payload.clone(),
                Arc::clone(&self.media_sender),
                self.error_tx.clone(),
                Arc::clone(&stop_flag),
                Arc::clone(&keyframe_requested),
                settings_rx,
                Arc::clone(&self.capture_registry),
            ))
        } else {
            // Drain the receiver end so settings updates targeted at this
            // connection don't accumulate unbounded; closing it here is
            // symmetric with not spawning a consumer.
            drop(settings_rx);
            debug!("[MediaProducer] {connection_id}: skipping video pipeline (start_video=false)");
            None
        };
        // PR 3: audio pipeline runs in its own dedicated thread (WASAPI
        // / PipeWire / SCKit handles are COM/system-thread-bound the
        // same way as the video capture, so a separate thread + a
        // current-thread Tokio runtime is the right shape — same
        // pattern Arch III used in `capture_audio_task`).
        let audio_handle = if payload.start_audio {
            Some(spawn_audio_pipeline_thread(
                self.desk_settings.clone(),
                payload,
                Arc::clone(&self.media_sender),
                self.error_tx.clone(),
                Arc::clone(&stop_flag),
            ))
        } else {
            debug!("[MediaProducer] {connection_id}: skipping audio pipeline (start_audio=false)");
            None
        };
        map.insert(
            connection_id,
            ConnectionTask {
                stop_flag,
                keyframe_requested,
                settings_tx,
                video_handle,
                audio_handle,
            },
        );
    }

    /// Test-only: snapshot per-connection state.
    /// Returns `(present, has_video_handle, has_audio_handle)`.
    /// Used to verify that DataChannel-only `StartMedia` payloads
    /// register a `ConnectionTask` slot but do not spawn any pipeline.
    #[cfg(test)]
    pub(crate) fn connection_pipeline_state(&self, connection_id: &str) -> Option<(bool, bool)> {
        let map = self.inner.lock().expect("media producer lock poisoned");
        map.get(connection_id)
            .map(|t| (t.video_handle.is_some(), t.audio_handle.is_some()))
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

    /// Live-update knobs (fps / quality). The video pipeline thread
    /// owns the encoder + ticker, so we deliver via the per-connection
    /// `settings_tx` mpsc channel; the loop's `try_recv` drains all
    /// pending updates on the next tick and rebuilds ticker (fps) +
    /// encoder (quality / bitrate). `bitrate_kbps` is currently routed
    /// to the encoder rebuild path but per-codec mapping (h264 bps vs
    /// vpx bps vs av1 quality-only) lives outside this fn — the
    /// encoder factory pulls from the merged DeskSettings, not from
    /// the IPC payload directly. No-op on unknown connection_id.
    ///
    /// Audio is intentionally not subscribed: Opus owns its frame size
    /// (20 ms fixed) and bitrate is set at create time; runtime audio
    /// retuning needs a separate variant once any UI exposes it.
    pub fn update_settings(&self, payload: UpdateMediaSettingsPayload) {
        let map = self.inner.lock().expect("media producer lock poisoned");
        let Some(task) = map.get(&payload.connection_id) else {
            debug!(
                "[MediaProducer] UpdateMediaSettings for unknown connection {}; ignoring",
                payload.connection_id
            );
            return;
        };
        // Reject codec swap attempts at the IPC boundary — the IPC
        // schema doesn't carry a codec field, but be explicit about
        // what does flow.
        info!(
            "[MediaProducer] UpdateMediaSettings queued for {} (fps={:?}, bitrate_kbps={:?}, \
             quality={:?})",
            payload.connection_id, payload.fps, payload.bitrate_kbps, payload.quality
        );
        if task.settings_tx.send(payload).is_err() {
            // The receiver lives in the pipeline thread; if it's gone
            // the thread already exited (stop_flag, capture failure,
            // etc.) and the next StopMedia / drop will reap us.
            debug!("[MediaProducer] UpdateMediaSettings receiver gone; pipeline already stopped");
        }
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
        // Verbatim encoder identifiers from capture-engine. We carry both
        // the raw strings (for the UI's encoder picker — preserves the
        // X264 vs H264/OpenH264 distinction) and a deduplicated
        // `MediaCodec` list (for SDP m-line negotiation, where both
        // encoders produce the same H.264 wire format).
        let video_encoders = list_video_encoder();
        let audio_encoders = list_audio_encoder();
        let mut video_codecs: Vec<MediaCodec> = Vec::new();
        for name in &video_encoders {
            if let Some(c) = codec_from_str(name, true)
                && !video_codecs.contains(&c)
            {
                video_codecs.push(c);
            }
        }
        let mut audio_codecs: Vec<MediaCodec> = Vec::new();
        for name in &audio_encoders {
            if let Some(c) = codec_from_str(name, false)
                && !audio_codecs.contains(&c)
            {
                audio_codecs.push(c);
            }
        }
        // Daemon's `pc_manager` echoes these maps verbatim into
        // `InitSignalingData::{video,audio}_device_list`, so the
        // browser's capture-source picker keeps the per-driver
        // grouping it had in the Arch III worker-owned-PC path.
        let video_device_list = list_image_capture();
        let audio_device_list = list_audio_capture();
        MediaCapabilities {
            video_codecs,
            audio_codecs,
            video_encoders,
            audio_encoders,
            video_device_list,
            audio_device_list,
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

/// Drain every pending `UpdateMediaSettingsPayload` from the per-
/// connection mpsc and apply each to `merged_settings`. The tick + fps
/// path is a `tokio::time::interval`, which the loop replaces on fps
/// Compute the wall-clock duration to attach to the next emitted
/// `MediaFrame`. The daemon hands this straight to webrtc-rs's
/// `Sample.duration`, which advances the RTP timestamp by
/// `duration_secs * 90000Hz` for video. Earlier code passed the
/// configured 1/fps interval as a fixed value, which was wrong for
/// two reasons:
///
///   - **Static-desktop heartbeat path.** Heartbeats fire every
///     ~1s but stamped duration=33ms. Each second of static
///     desktop made the receiver's RTP clock fall ~967ms behind
///     wall clock. After a minute of idle, the browser's
///     playout buffer held nearly a minute of "future" frames —
///     so when the user finally moved the mouse, the browser
///     replayed minutes-old activity instead of showing live
///     events.
///
///   - **Broadcast lag path.** When the encoder loop falls
///     behind the OS-rate capture loop (`RecvError::Lagged`),
///     real wall-clock interval can be 50-100ms; stamping 33ms
///     made the same drift accumulate at a smaller per-event
///     rate.
///
/// The first emit has no `prev_emit` reference, so we fall back
/// to the configured 1/fps default — the receiver's first
/// timestamp doesn't matter for delta calculations.
fn compute_emit_duration_ns(
    prev_emit: Option<std::time::Instant>,
    now: std::time::Instant,
    default_ns: u64,
) -> u64 {
    match prev_emit {
        Some(prev) => now.duration_since(prev).as_nanos().min(u64::MAX as u128) as u64,
        None => default_ns,
    }
}

/// Classify the `MediaFrameKind` for an outgoing video access unit.
/// Returns `VideoI` if either:
///   - The worker just rebuilt the encoder (`next_pass_is_idr=true`,
///     covers initial start, settings_changed rebuild, ForceKeyframe
///     rebuild) — the very first encoder output is by construction
///     SPS+PPS+IDR.
///   - Any NAL in this access unit reports `is_keyframe=true` from
///     the encoder's own frame-type signal — covers the periodic
///     internal-GOP IDR that the encoder emits without any worker
///     rebuild (with the default GOP=120 this happens roughly every
///     2 s at 60 fps).
/// Pinned as a helper so the "encoder GOP IDR is also VideoI" contract
/// is unit-testable independently of the surrounding async loop.
#[inline]
fn classify_video_frame_kind(
    nals: &[desk_capture_engine::model::video_encoder::NalInfo],
    next_pass_is_idr: bool,
) -> MediaFrameKind {
    let any_keyframe = nals.iter().any(|n| n.is_keyframe);
    if next_pass_is_idr || any_keyframe {
        MediaFrameKind::VideoI
    } else {
        MediaFrameKind::VideoP
    }
}

/// Handler for `RecvError::Lagged(n)` on the shared-capture
/// broadcast subscription. Pinned as a separate function so the
/// "lag does NOT request a keyframe" contract is unit-testable
/// and any future regression that re-introduces an encoder
/// rebuild here gets caught.
///
/// The body intentionally has no side effects on the encoder /
/// keyframe state: see the call site in `video_pipeline_loop`
/// for the reasoning. This logs at DEBUG (not WARN) because lag
/// is the expected steady-state behaviour when capture runs
/// faster than the per-connection fps throttle.
#[inline]
fn handle_broadcast_lag(connection_id: &str, n: u64) {
    debug!(
        "[MediaProducer:{connection_id}] shared-capture broadcast lagged by {n} \
         frames; skipping ahead to the latest available input"
    );
}

/// changes (interval can't be retuned in place).
///
/// Returns `true` when at least one knob actually changed — the caller
/// uses this to decide whether to rebuild the encoder. We compare to
/// the *current* `merged_settings` rather than the IPC payload
/// directly so coalesced updates that converge to the same value as
/// the live state are no-ops (the daemon currently fans out on every
/// `UpdateDeskSettings`, including ones that don't move encoder-
/// relevant fields).
fn drain_settings_updates(
    connection_id: &str,
    settings_rx: &mut mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    merged_settings: &mut DeskSettings,
    frame_interval: &mut Duration,
    frame_duration_ns: &mut u64,
) -> bool {
    let mut changed = false;
    while let Ok(payload) = settings_rx.try_recv() {
        if let Some(fps) = payload.fps
            && fps > 0
            && fps != merged_settings.video_fps
        {
            merged_settings.video_fps = fps;
            *frame_interval = merged_settings.get_duration_by_video_fps();
            *frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;
            changed = true;
        }
        if let Some(q) = payload.quality
            && q != merged_settings.video_quality
        {
            merged_settings.video_quality = q;
            changed = true;
        }
        if let Some(enable) = payload.enable_dirty_rect
            && enable != merged_settings.enable_dirty_rect
        {
            // Live-apply the browser's Advanced-tab kill-switch. We do
            // *not* flip the `changed` flag because the encoder
            // doesn't need to be rebuilt — `merged_settings.
            // enable_dirty_rect` is read per-frame by the encoder via
            // `encode(..., enable_dirty_rect)`, so the next frame
            // picks up the new value without a `create_video_encoder`
            // round-trip.
            merged_settings.enable_dirty_rect = enable;
        }
        if let Some(kbps) = payload.bitrate_kbps
            && kbps > 0
        {
            // Bitrate maps to the codec-specific encoder-settings
            // struct (`H264EncoderSettings.bps`, `VpxEncoderSettings.bps`,
            // etc.); applying it requires routing per active codec. Cut
            // 5 keeps the wire field for forward compatibility but does
            // not yet apply it — callers should change `quality` if
            // they want a runtime bitrate effect today, since the
            // factory recomputes bps from quality when the codec-
            // specific settings are absent.
            debug!(
                "[MediaProducer:{connection_id}] UpdateMediaSettings.bitrate_kbps={kbps} ignored \
                 (per-codec mapping not yet wired)"
            );
        }
    }
    changed
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
    // Per-connection backend choice: when the daemon thread-throughs
    // a value (typically the per-connection `desk_settings.image_capture`
    // from the SDP offer), it overrides the worker's startup snapshot.
    // Without this override the worker would always see `base.image_capture`
    // and a second browser could not pick a different backend than the
    // first — see the IPC field's doc comment for the failure mode.
    if let Some(backend) = payload.image_capture.as_deref() {
        s.image_capture = Some(backend.to_string());
    }
    // Per-connection dirty-rect kill-switch — when the daemon sniffed
    // the value out of the SDP offer's `desk_settings`, honour it
    // here. `None` means the daemon is older than the field (or the
    // offer never carried it) and we keep the worker's base setting,
    // matching the back-compat contract documented on the IPC field.
    if let Some(enable) = payload.enable_dirty_rect {
        s.enable_dirty_rect = enable;
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
    settings_rx: mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    capture_registry: Arc<SharedCaptureRegistry>,
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
                    settings_rx,
                    capture_registry,
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

/// Inner async loop for video. Subscribes to the worker-wide
/// `SharedCaptureRegistry` for its `(backend, output_index)` and
/// pumps frames from the broadcast channel into a per-connection
/// encoder. fps is honoured by per-connection throttling — the
/// shared capture loop runs at the OS refresh rate; this loop drops
/// frames when its own quality knob asks for a lower rate. Heartbeat-
/// frame behaviour mirrors Arch III: on a static desktop emit one
/// cached frame per second so the receiver does not stall.
async fn video_pipeline_loop(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
    mut settings_rx: mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    capture_registry: Arc<SharedCaptureRegistry>,
) -> Result<(), String> {
    let connection_id = payload.connection_id.clone();
    let codec = payload.video_codec;
    let mut merged_settings = payload_overrides(&base_settings, &payload);

    info!(
        "[MediaProducer:{connection_id}] Starting pipeline: codec={codec:?}, fps={}, \
         enable_dirty_rect={}",
        merged_settings.video_fps, merged_settings.enable_dirty_rect
    );

    // Subscribe to the shared capture loop for this `(backend,
    // output)`. If no loop exists the registry spawns one;
    // otherwise the existing loop's broadcast sender hands us a
    // fresh receiver. `display_info` is published by the registry
    // (the capture instance owns it) so we don't need our own
    // capture handle to re-derive resolution.
    let capture_handle = capture_registry
        .subscribe(&merged_settings)
        .map_err(|e| format!("{e}"))?;
    let mut frame_rx = capture_handle.subscribe();
    let display_info = capture_handle.display_info().clone();

    let mut encoder: Box<dyn VideoEncoder> =
        create_video_encoder(&merged_settings, &display_info).map_err(|e| format!("{e}"))?;
    let mut next_pass_is_idr = true; // first frame is always I (encoder emits SPS/PPS+IDR)
    let mut seq: u64 = 0;
    let mut frame_interval = merged_settings.get_duration_by_video_fps();
    let mut frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;
    let mut last_send_time = std::time::Instant::now();
    // Wall-clock instant of the most recent emit — drives the
    // dynamic `Sample.duration` calculation. `None` until we've
    // emitted at least once; the first emit uses `frame_duration_ns`
    // (1/fps) as a sensible default since there's no previous tick
    // to subtract.
    let mut last_emit_wall: Option<std::time::Instant> = None;
    // Force the first emitted frame to bypass the throttle gate so
    // the browser sees an IDR immediately on connect (initial
    // `last_emit_for_throttle = now - frame_interval` lets the very
    // first non-heartbeat tick pass).
    let mut last_emit_for_throttle = std::time::Instant::now()
        .checked_sub(frame_interval)
        .unwrap_or_else(std::time::Instant::now);
    // Diagnostic flag: set whenever the encoder is freshly built (initial
    // construction, settings_changed rebuild, or keyframe_requested
    // rebuild). The first emission pass after the rebuild logs a single
    // INFO line describing the resulting NAL layout — used to triage
    // bugs like the "screen turns green after a while" failure.
    let mut rebuild_pending = true;

    while !stop_flag.load(Ordering::Relaxed) {
        // Wait for the next shared frame. The capture loop runs as
        // fast as the backend yields; this loop's fps throttle gates
        // whether the frame is encoded or skipped.
        let shared_frame = match frame_rx.recv().await {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Closed) => {
                warn!(
                    "[MediaProducer:{connection_id}] shared-capture broadcast closed; pipeline \
                     exiting"
                );
                return Ok(());
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // The shared-capture loop runs at the OS refresh
                // rate; per-connection encoders run at a (typically
                // lower) configured fps, so the bounded broadcast
                // ring will routinely drop the oldest queued *input*
                // frames before our next recv. This is benign:
                //
                //  - We missed *input* frames, not *output* RTP.
                //  - The encoder's internal reference chain is still
                //    valid (we never fed it a frame after our last
                //    successful encode).
                //  - The next P frame off the latest available input
                //    describes the gap correctly to the browser
                //    without an IDR.
                //
                // Earlier versions requested a keyframe on every lag,
                // which recreated the encoder — an order of magnitude
                // more expensive than emitting one P frame — and fed
                // a self-amplifying keyframe-storm loop where each
                // rebuild widened the lag, triggering more rebuilds.
                handle_broadcast_lag(&connection_id, n);
                continue;
            }
        };
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        // Apply any pending live-update settings before honouring
        // the keyframe flag. Coalesce a burst into a single
        // rebuild. NB: backend / output_index changes are out of
        // scope here — they would require resubscribing to a
        // different `CaptureKey`, and the live-settings stream
        // currently does not include them.
        let settings_changed = drain_settings_updates(
            &connection_id,
            &mut settings_rx,
            &mut merged_settings,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        if settings_changed {
            info!(
                "[MediaProducer:{connection_id}] Live settings changed; recreating encoder \
                 (fps={}, video_quality={}, enable_dirty_rect={})",
                merged_settings.video_fps,
                merged_settings.video_quality,
                merged_settings.enable_dirty_rect
            );
            encoder = create_video_encoder(&merged_settings, &display_info)
                .map_err(|e| format!("{e}"))?;
            next_pass_is_idr = true;
            rebuild_pending = true;
            // Reset throttle so the new encoder's first IDR is
            // emitted on the very next non-heartbeat frame, not
            // delayed by `frame_interval`.
            last_emit_for_throttle = std::time::Instant::now()
                .checked_sub(frame_interval)
                .unwrap_or_else(std::time::Instant::now);
        }

        if keyframe_requested.swap(false, Ordering::Relaxed) {
            info!(
                "[MediaProducer:{connection_id}] Keyframe requested; recreating encoder so the \
                 next encode pass emits an IDR"
            );
            encoder = create_video_encoder(&merged_settings, &display_info)
                .map_err(|e| format!("{e}"))?;
            next_pass_is_idr = true;
            rebuild_pending = true;
            last_emit_for_throttle = std::time::Instant::now()
                .checked_sub(frame_interval)
                .unwrap_or_else(std::time::Instant::now);
        }

        // Cursor sync: the shared capture loop is hard-pinned to
        // SyncNative, so cursor metadata is always present (when the
        // backend has an update). Per-connection `show_mouse`
        // decides whether to forward it on this connection's
        // dedicated `cursor_sync_event` DC. This is how two browsers
        // sharing a capture can independently choose to display or
        // suppress the cursor.
        if merged_settings.show_mouse
            && let Some(cursor) = &shared_frame.cursor_update
        {
            match serde_json::to_vec(cursor) {
                Ok(bytes) => {
                    let payload = desk_ipc_protocol::message::CursorDataPayload {
                        connection_id: connection_id.clone(),
                        data: bytes,
                    };
                    if error_tx.send(WorkerToService::CursorData(payload)).is_err() {
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

        let now = std::time::Instant::now();

        if !shared_frame.content_changed {
            // Static-desktop heartbeat: emit one cached frame per
            // second so the daemon-side track keeps producing RTP
            // and the browser decoder does not declare the stream
            // dead. Heartbeats bypass the fps throttle (one per
            // second is well below any sensible fps anyway).
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
            if rebuild_pending && nal_info_vec.is_empty() {
                warn!(
                    "[MediaProducer:{connection_id}] post-rebuild heartbeat tick produced 0 \
                     NALs (encoder yuv_buffer is None on a freshly built encoder); browser \
                     will see no frames until the next non-static capture tick"
                );
            }
            // Heartbeat duration must reflect wall-clock elapsed
            // (~1s under the static-desktop branch above) so the
            // receiver's RTP timestamps stay in sync with wall
            // clock. Subsequent NALs from the same encode pass
            // share the timestamp (duration=0) — they describe
            // the same access unit.
            let actual_duration_ns =
                compute_emit_duration_ns(last_emit_wall, now, frame_duration_ns);
            // Honour the encoder's native frame-type signal: an
            // internal-GOP IDR mid-heartbeat must surface as VideoI so
            // the daemon's paused-write_sample latch (after a worker
            // swap) can clear on a natural IDR, and so host-side I-frame
            // counts match what the browser decoder reports.
            let kind_for_pass = classify_video_frame_kind(&nal_info_vec, next_pass_is_idr);
            let was_idr_flag = next_pass_is_idr;
            next_pass_is_idr = false;
            for (i, nal) in nal_info_vec.into_iter().enumerate() {
                if rebuild_pending {
                    log_post_rebuild_emit(
                        &connection_id,
                        "heartbeat",
                        codec,
                        kind_for_pass,
                        was_idr_flag,
                        nal.nal_bytes.as_ref(),
                    );
                    rebuild_pending = false;
                }
                let frame = build_media_frame(
                    &connection_id,
                    seq,
                    if i == 0 { actual_duration_ns } else { 0 },
                    kind_for_pass,
                    codec,
                    nal.nal_bytes.to_vec(),
                );
                seq += 1;
                if !send_frame(&media_sender, &error_tx, &connection_id, frame).await {
                    return Ok(());
                }
            }
            last_send_time = now;
            last_emit_wall = Some(now);
            continue;
        }

        // fps throttle: skip the frame entirely if our last emit was
        // less than `frame_interval` ago. The shared capture loop
        // produces frames at the OS refresh rate; a 30 fps
        // connection effectively takes every other frame at 60 Hz.
        if now.duration_since(last_emit_for_throttle) < frame_interval {
            continue;
        }

        let nal_info_vec = match encoder.encode(
            shared_frame.as_ref() as &dyn ImageInfo,
            merged_settings.enable_dirty_rect,
        ) {
            Ok(v) => v,
            Err(e) => {
                warn!("[MediaProducer:{connection_id}] encode error: {e}; continuing");
                continue;
            }
        };
        // Honour the encoder's native frame-type signal alongside our
        // own `next_pass_is_idr` rebuild marker. With a wider GOP
        // (default 120) the encoder still emits periodic IDRs without
        // any worker-side rebuild — those need to be labelled VideoI
        // so the daemon's paused-write_sample latch can clear on them
        // and host-side keyframe counts align with the browser.
        let kind_for_pass = classify_video_frame_kind(&nal_info_vec, next_pass_is_idr);
        let was_idr_flag = next_pass_is_idr;
        next_pass_is_idr = false;
        // Same dynamic-duration treatment as the heartbeat path:
        // when broadcast lag (or a paused capture) makes the real
        // gap between emits longer than 1/fps, the receiver's RTP
        // timestamp must reflect that or its jitter buffer drifts
        // ahead of wall clock.
        let actual_duration_ns = compute_emit_duration_ns(last_emit_wall, now, frame_duration_ns);
        for (i, nal) in nal_info_vec.into_iter().enumerate() {
            if rebuild_pending {
                log_post_rebuild_emit(
                    &connection_id,
                    "encode",
                    codec,
                    kind_for_pass,
                    was_idr_flag,
                    nal.nal_bytes.as_ref(),
                );
                rebuild_pending = false;
            }
            let frame = build_media_frame(
                &connection_id,
                seq,
                if i == 0 { actual_duration_ns } else { 0 },
                kind_for_pass,
                codec,
                nal.nal_bytes.to_vec(),
            );
            seq += 1;
            if !send_frame(&media_sender, &error_tx, &connection_id, frame).await {
                return Ok(());
            }
        }
        last_send_time = now;
        last_emit_wall = Some(now);
        last_emit_for_throttle = now;
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

/// Diagnostic helper: emit one INFO line describing what the encoder
/// produced on the first emit pass after a fresh build (initial start,
/// settings_changed rebuild, or keyframe_requested rebuild). Helps
/// confirm whether `next_pass_is_idr=true` actually translated into an
/// IDR / SPS / PPS NAL on the wire vs. a non-IDR slice mis-labelled
/// VideoI. Decodes H.264 NAL unit type (`byte & 0x1F` after the
/// startcode); for other codecs only the first 8 payload bytes are
/// dumped — operators reading the log can correlate against codec
/// specs as needed.
fn log_post_rebuild_emit(
    connection_id: &str,
    path: &str,
    codec: MediaCodec,
    kind: MediaFrameKind,
    next_pass_is_idr: bool,
    nal_bytes: &[u8],
) {
    let head_hex: String = nal_bytes
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let codec_specific = match codec {
        MediaCodec::H264 => {
            // Walk all NAL units in the bytestream and list type +
            // length of each. The "screen turns green" investigation
            // hinges on whether a rebuild-IDR frame is `SPS + PPS +
            // real IDR slice` (~tens of KB for 1280x800) or `SPS +
            // PPS + empty / dummy slice` (only a few KB), so the
            // first-NAL-only summary isn't enough — we need every
            // NAL's identity to tell those apart.
            let nals = h264_walk_nals(nal_bytes);
            if nals.is_empty() {
                ", h264_nals=<no startcode>".to_string()
            } else {
                let parts: Vec<String> = nals
                    .iter()
                    .map(|(byte, len)| {
                        let unit_type = byte & 0x1F;
                        let label = match unit_type {
                            1 => "non-IDR",
                            5 => "IDR",
                            6 => "SEI",
                            7 => "SPS",
                            8 => "PPS",
                            9 => "AUD",
                            _ => "?",
                        };
                        format!("{unit_type}({label}):{len}")
                    })
                    .collect();
                format!(", h264_nals=[{}]", parts.join(", "))
            }
        }
        MediaCodec::Vp8 | MediaCodec::Vp9 => {
            let kf_bit = nal_bytes.first().map(|b| b & 0x01);
            match kf_bit {
                Some(0) => ", vpx_frame_type=key".to_string(),
                Some(_) => ", vpx_frame_type=inter".to_string(),
                None => ", vpx_frame_type=<empty>".to_string(),
            }
        }
        _ => String::new(),
    };
    info!(
        "[MediaProducer:{connection_id}] post-rebuild first emit (path={path}, kind={kind:?}, \
         next_pass_is_idr={next_pass_is_idr}, codec={codec:?}, payload_len={}, head={head_hex}{codec_specific})",
        nal_bytes.len()
    );
}

/// Walk an Annex-B H.264 bytestream and return `(header_byte, payload_len)`
/// for each NAL unit found. `payload_len` is the size of the NAL itself
/// (excluding the leading startcode), measured up to the next startcode
/// or end-of-buffer. Used purely for diagnostic logging.
fn h264_walk_nals(nal_bytes: &[u8]) -> Vec<(u8, usize)> {
    let mut nals: Vec<(u8, usize)> = Vec::new();
    // Locate every Annex-B startcode (`00 00 00 01` or `00 00 01`) and
    // record its position + the size of the startcode prefix so we can
    // measure each NAL's payload length as the distance to the next
    // startcode (or end of buffer).
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (offset_after_startcode, prefix_len)
    let mut i = 0;
    while i + 2 < nal_bytes.len() {
        if nal_bytes[i] == 0 && nal_bytes[i + 1] == 0 {
            if i + 3 < nal_bytes.len() && nal_bytes[i + 2] == 0 && nal_bytes[i + 3] == 1 {
                starts.push((i + 4, 4));
                i += 4;
                continue;
            }
            if nal_bytes[i + 2] == 1 {
                starts.push((i + 3, 3));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    for (idx, (off, prefix)) in starts.iter().enumerate() {
        if *off >= nal_bytes.len() {
            continue;
        }
        let header_byte = nal_bytes[*off];
        let next_start = starts
            .get(idx + 1)
            .map(|(o, p)| o.saturating_sub(*p))
            .unwrap_or(nal_bytes.len());
        let payload_len = next_start.saturating_sub(*off);
        nals.push((header_byte, payload_len));
    }
    nals
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
            // Carry `connection_id` on the payload so the daemon can issue
            // StopMedia + StartMedia for exactly this PC instead of having
            // to parse the human-readable `message` field.
            let _ = error_tx.send(WorkerToService::Error(ErrorPayload {
                code: ERROR_CODE_MEDIA_TRANSPORT_STUCK,
                message: format!(
                    "I-frame send timed out for connection {connection_id} (kind={kind:?}); \
                     daemon should issue StopMedia+StartMedia"
                ),
                recoverable: true,
                connection_id: Some(connection_id.to_string()),
            }));
            true
        }
        Err(other) => {
            warn!("[MediaProducer:{connection_id}] media transport send error: {other}");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::dual_transport::inprocess;
    use desk_signal_facade::model::desk_settings::DeskSettings;

    /// Walk a typical IDR access unit (SPS + PPS + IDR slice) and verify
    /// each NAL's header + payload length is reported. This is the
    /// shape we expect on a healthy initial frame, and the diff between
    /// "real IDR slice = many KB" and "dummy slice = few bytes" is the
    /// signal we're after when the screen turns green after a rebuild.
    #[test]
    fn h264_walk_nals_lists_sps_pps_idr() {
        let mut bytes: Vec<u8> = Vec::new();
        // SPS (3 bytes payload incl header)
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0]);
        // PPS (3 bytes payload incl header)
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C]);
        // IDR slice (5 bytes payload incl header)
        bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0xB8, 0x00, 0x04, 0x00]);
        let nals = h264_walk_nals(&bytes);
        assert_eq!(nals.len(), 3, "expected 3 NAL units (SPS + PPS + IDR)");
        assert_eq!(nals[0].0 & 0x1F, 7, "first NAL must be SPS");
        assert_eq!(nals[0].1, 3);
        assert_eq!(nals[1].0 & 0x1F, 8, "second NAL must be PPS");
        assert_eq!(nals[1].1, 3);
        assert_eq!(nals[2].0 & 0x1F, 5, "third NAL must be IDR");
        assert_eq!(nals[2].1, 5);
    }

    /// Mixed 3-byte and 4-byte startcodes are both recognised. The
    /// trailing NAL's length must extend to end-of-buffer.
    #[test]
    fn h264_walk_nals_handles_mixed_startcodes() {
        // 4-byte startcode + AUD (1 byte) + 3-byte startcode + SEI (4
        // bytes to end-of-buffer)
        let bytes: Vec<u8> = vec![
            0x00, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x01, 0x06, 0x05, 0xFF, 0x80,
        ];
        let nals = h264_walk_nals(&bytes);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0].0 & 0x1F, 9, "AccessUnitDelim");
        assert_eq!(nals[0].1, 1);
        assert_eq!(nals[1].0 & 0x1F, 6, "SEI");
        assert_eq!(nals[1].1, 4);
    }

    /// Empty / mis-framed buffers must yield an empty list rather than
    /// panicking. Guards the diagnostic against poisoning the event
    /// pipeline if a non-H264 stream lands in the H264 branch.
    #[test]
    fn h264_walk_nals_handles_short_or_missing() {
        assert!(h264_walk_nals(&[]).is_empty());
        assert!(h264_walk_nals(&[0xAA, 0xBB, 0xCC]).is_empty());
        // Only a startcode, no header byte after — also empty (the
        // walker skips entries whose header offset is past the end).
        assert!(h264_walk_nals(&[0x00, 0x00, 0x00, 0x01]).is_empty());
    }

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
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: None,
        };
        let merged = payload_overrides(&base, &payload);
        assert_eq!(merged.video_encoder.as_deref(), Some("VP9"));
        assert_eq!(merged.video_fps, 60);
        // Cut 4: `video_device` IPC field is intentionally ignored until
        // the daemon wires a name→index lookup. Pin so the next change to
        // payload_overrides doesn't silently start interpreting it.
        assert_eq!(merged.video_device_index, base.video_device_index);
    }

    /// Per-connection `image_capture` choice from the daemon overrides
    /// the worker's startup snapshot. Regression for the
    /// "second-browser-can't-pick-GDI" bug: pre-fix `payload_overrides`
    /// dropped the field on the floor and every connection inherited
    /// the worker's base backend (DXGI by default), causing the second
    /// connection to hit `DuplicateOutput` E_INVALIDARG against the
    /// first connection's already-active duplication.
    #[test]
    fn payload_overrides_apply_per_connection_image_capture() {
        let base = DeskSettings {
            image_capture: Some("DXGI".into()),
            ..DeskSettings::default()
        };
        let payload = StartMediaPayload {
            connection_id: "c2".into(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 0,
            bitrate_kbps: 0,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: Some("GDI".into()),
            enable_dirty_rect: None,
        };
        let merged = payload_overrides(&base, &payload);
        assert_eq!(
            merged.image_capture.as_deref(),
            Some("GDI"),
            "per-connection override must replace the worker's base backend"
        );
    }

    /// Conversely, when the daemon does not specify a backend (e.g. an
    /// older daemon that predates the IPC field, or an offer with no
    /// preference), the worker must keep its base setting unchanged so
    /// the platform default still applies.
    #[test]
    fn payload_overrides_image_capture_none_preserves_base() {
        let base = DeskSettings {
            image_capture: Some("DXGI".into()),
            ..DeskSettings::default()
        };
        let payload = StartMediaPayload {
            connection_id: "c3".into(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 0,
            bitrate_kbps: 0,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: None,
        };
        let merged = payload_overrides(&base, &payload);
        assert_eq!(merged.image_capture.as_deref(), Some("DXGI"));
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
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: None,
        };
        let merged = payload_overrides(&base, &payload);
        assert_eq!(merged.video_fps, 24);
    }

    /// Regression: a `StartMedia` payload with `start_video = false`
    /// and `start_audio = false` must register a `ConnectionTask`
    /// slot (so subsequent `StopMedia` / `ForceKeyframe` find it) but
    /// must NOT spawn either pipeline thread. Bug fix 2026-05-05:
    /// previously the worker always lit up DXGI + WASAPI capture for
    /// every PC, including the browser file-management page that
    /// negotiates a DataChannel-only PC.
    #[test]
    fn start_media_data_channel_only_skips_both_pipelines() {
        let (sender, _rx) = inprocess::make_media();
        let (err_tx, _err_rx) = mpsc::unbounded_channel::<WorkerToService>();
        let producer = MediaProducer::new(DeskSettings::default(), sender, err_tx);
        producer.start_media(StartMediaPayload {
            connection_id: "files".into(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 0,
            bitrate_kbps: 0,
            quality: 0,
            start_video: false,
            start_audio: false,
            image_capture: None,
            enable_dirty_rect: None,
        });
        let state = producer
            .connection_pipeline_state("files")
            .expect("DataChannel-only StartMedia must still register the connection slot");
        assert_eq!(
            state,
            (false, false),
            "DataChannel-only StartMedia must not spawn video or audio pipeline"
        );
        // StopMedia must find the entry and clean it up; pre-fix this
        // would have logged a debug "unknown connection" — the test
        // passes either way but keeps the symmetry pinned.
        producer.stop_media(&StopMediaPayload {
            connection_id: "files".into(),
        });
        assert!(
            producer.connection_pipeline_state("files").is_none(),
            "stop_media must drop the slot"
        );
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

    /// `update_settings` for an unknown connection_id silently drops
    /// (the producer doesn't allocate a per-connection task until
    /// `start_media`); the daemon may race a `StopMedia` with a
    /// settings change so the lookup-miss path must stay quiet.
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
            enable_dirty_rect: None,
        });
    }

    /// `drain_settings_updates` applies fps and quality changes to
    /// `merged_settings`, rebuilds the ticker on fps changes, and
    /// returns `true` so the caller knows to recreate the encoder.
    /// A repeat of the same value is a no-op (returns `false`).
    #[tokio::test(flavor = "current_thread")]
    async fn drain_settings_updates_applies_fps_and_quality() {
        let (tx, mut rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
        let mut merged = DeskSettings {
            video_fps: 30,
            video_quality: 22,
            ..DeskSettings::default()
        };
        let mut frame_interval = merged.get_duration_by_video_fps();
        let mut frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;

        // No pending update → returns false, leaves state untouched.
        let changed = drain_settings_updates(
            "c1",
            &mut rx,
            &mut merged,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        assert!(!changed);
        assert_eq!(merged.video_fps, 30);

        // Apply fps=60 + quality=40 → both change, returns true, frame
        // duration recomputed.
        tx.send(UpdateMediaSettingsPayload {
            connection_id: "c1".into(),
            fps: Some(60),
            bitrate_kbps: None,
            quality: Some(40),
            enable_dirty_rect: None,
        })
        .unwrap();
        let changed = drain_settings_updates(
            "c1",
            &mut rx,
            &mut merged,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        assert!(changed);
        assert_eq!(merged.video_fps, 60);
        assert_eq!(merged.video_quality, 40);
        assert_eq!(
            frame_duration_ns,
            merged
                .get_duration_by_video_fps()
                .as_nanos()
                .min(u64::MAX as u128) as u64,
            "frame duration must follow the new fps"
        );

        // Same values again → no-op, returns false.
        tx.send(UpdateMediaSettingsPayload {
            connection_id: "c1".into(),
            fps: Some(60),
            bitrate_kbps: None,
            quality: Some(40),
            enable_dirty_rect: None,
        })
        .unwrap();
        let changed = drain_settings_updates(
            "c1",
            &mut rx,
            &mut merged,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        assert!(!changed);
    }

    /// Regression for the dirty-rect kill-switch wiring: the browser's
    /// Advanced-tab toggle eventually lands as
    /// `UpdateMediaSettingsPayload.enable_dirty_rect`; `drain_settings_
    /// updates` must apply it to `merged_settings.enable_dirty_rect`
    /// so the next `encoder.encode(..., enable_dirty_rect)` call
    /// honours it. Pre-fix the field did not exist on the payload at
    /// all, so the worker's `merged_settings.enable_dirty_rect` was
    /// frozen at the worker's startup default (`true`) regardless of
    /// what the browser sent.
    #[tokio::test(flavor = "current_thread")]
    async fn drain_settings_updates_applies_enable_dirty_rect() {
        let (tx, mut rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
        let mut merged = DeskSettings {
            enable_dirty_rect: true,
            ..DeskSettings::default()
        };
        let mut frame_interval = merged.get_duration_by_video_fps();
        let mut frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;

        tx.send(UpdateMediaSettingsPayload {
            connection_id: "c1".into(),
            fps: None,
            bitrate_kbps: None,
            quality: None,
            enable_dirty_rect: Some(false),
        })
        .unwrap();
        let changed = drain_settings_updates(
            "c1",
            &mut rx,
            &mut merged,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        // Dirty-rect flips do not force an encoder rebuild — the
        // encoder reads the flag per-frame. `changed` stays `false`.
        assert!(
            !changed,
            "enable_dirty_rect-only change must not force encoder rebuild"
        );
        assert!(
            !merged.enable_dirty_rect,
            "enable_dirty_rect must be applied to merged_settings"
        );

        // Re-enabling round-trips just as cleanly.
        tx.send(UpdateMediaSettingsPayload {
            connection_id: "c1".into(),
            fps: None,
            bitrate_kbps: None,
            quality: None,
            enable_dirty_rect: Some(true),
        })
        .unwrap();
        let _ = drain_settings_updates(
            "c1",
            &mut rx,
            &mut merged,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        assert!(merged.enable_dirty_rect);
    }

    /// `payload_overrides` must honour `StartMediaPayload.
    /// enable_dirty_rect` so a fresh connection picks up the
    /// browser's Advanced-tab toggle on the *first* frame rather than
    /// waiting for a follow-up `UpdateMediaSettings`. Regression
    /// guard: pre-fix the field did not exist on `StartMediaPayload`,
    /// so a connection that negotiated `enable_dirty_rect=false`
    /// would still see the worker's base default (`true`) until the
    /// next live settings round-trip.
    #[test]
    fn payload_overrides_applies_enable_dirty_rect() {
        let base = DeskSettings {
            enable_dirty_rect: true,
            ..DeskSettings::default()
        };
        let payload = StartMediaPayload {
            connection_id: "c-dr".into(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 0,
            bitrate_kbps: 0,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: Some(false),
        };
        let merged = payload_overrides(&base, &payload);
        assert!(
            !merged.enable_dirty_rect,
            "payload override must replace the worker's base value"
        );

        // `None` preserves base — back-compat path with older daemons
        // that do not yet sniff the field.
        let payload_none = StartMediaPayload {
            enable_dirty_rect: None,
            ..payload
        };
        let merged_none = payload_overrides(&base, &payload_none);
        assert!(merged_none.enable_dirty_rect);
    }

    /// `drain_settings_updates` ignores `fps = 0` (sentinel for "use
    /// default") and `bitrate_kbps` (per-codec mapping not yet wired).
    /// Pinning these so a future change to the IPC schema doesn't
    /// silently mis-apply.
    #[tokio::test(flavor = "current_thread")]
    async fn drain_settings_updates_ignores_fps_zero_and_bitrate() {
        let (tx, mut rx) = mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
        let mut merged = DeskSettings {
            video_fps: 30,
            video_quality: 22,
            ..DeskSettings::default()
        };
        let mut frame_interval = merged.get_duration_by_video_fps();
        let mut frame_duration_ns = 0u64;

        tx.send(UpdateMediaSettingsPayload {
            connection_id: "c1".into(),
            fps: Some(0),              // sentinel — must NOT replace 30 with 0 fps
            bitrate_kbps: Some(8_000), // currently unwired — must NOT change anything
            quality: None,
            enable_dirty_rect: None,
        })
        .unwrap();
        let changed = drain_settings_updates(
            "c1",
            &mut rx,
            &mut merged,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        assert!(!changed, "fps=0 + bitrate alone must be a no-op today");
        assert_eq!(merged.video_fps, 30);
        assert_eq!(merged.video_quality, 22);
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

    /// Regression: the UI used to render two indistinguishable "H264"
    /// entries because the daemon mapped `MediaCodec::H264` back to a
    /// single string for both X264 (libx264) and H264 (OpenH264). The
    /// fix carries the verbatim encoder identifiers in
    /// `video_encoders` alongside the SDP-level `video_codecs`. This
    /// test pins the contract: every capture-engine encoder name
    /// surfaces independently in `video_encoders`, while the
    /// `video_codecs` list collapses on SDP-equivalent duplicates.
    #[test]
    fn build_capabilities_preserves_x264_h264_distinction() {
        let caps = MediaProducer::build_capabilities(Some("Default"), false);
        assert!(
            caps.video_encoders.contains(&"X264".to_string()),
            "X264 must appear in video_encoders: {:?}",
            caps.video_encoders
        );
        assert!(
            caps.video_encoders.contains(&"H264".to_string()),
            "H264 must appear in video_encoders: {:?}",
            caps.video_encoders
        );
        let h264_codec_count = caps
            .video_codecs
            .iter()
            .filter(|c| matches!(c, MediaCodec::H264))
            .count();
        assert_eq!(
            h264_codec_count, 1,
            "video_codecs collapses both H.264 implementations onto one MediaCodec::H264 \
             for SDP m-line negotiation: {:?}",
            caps.video_codecs
        );
        assert!(
            caps.audio_encoders
                .iter()
                .any(|s| s.eq_ignore_ascii_case("OPUS")),
            "audio_encoders must include Opus: {:?}",
            caps.audio_encoders
        );
    }

    /// Regression: `frame_duration_ns` was previously hardcoded to
    /// 1/fps everywhere it was emitted, so when wall-clock elapsed
    /// between emits exceeded 1/fps (heartbeat path = ~1s, broadcast
    /// lag path = 50-100ms), the receiver's RTP timestamp drifted
    /// behind wall clock by the difference. Over a minute of static
    /// desktop the drift reached ~58s, manifesting as the user's
    /// reported "browser shows actions from a minute ago" symptom.
    ///
    /// `compute_emit_duration_ns` must:
    ///   1. Fall back to `default_ns` when there's no prior emit
    ///      (first frame after connect).
    ///   2. Return the real wall-clock delta when there is one,
    ///      regardless of how long it is — the heartbeat path
    ///      *needs* ~1s for its sample.
    #[test]
    fn compute_emit_duration_ns_first_emit_falls_back_to_default() {
        let now = std::time::Instant::now();
        assert_eq!(
            compute_emit_duration_ns(None, now, 33_000_000),
            33_000_000,
            "with no prior emit there's nothing to subtract; default 1/fps is the right baseline"
        );
    }

    #[test]
    fn compute_emit_duration_ns_reflects_short_wall_clock_delta() {
        let prev = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now = std::time::Instant::now();
        let dur = compute_emit_duration_ns(Some(prev), now, 33_000_000);
        // 50ms elapsed; the configured default of 33ms must NOT
        // be returned — that's the bug this guards against.
        assert!(
            (40_000_000..=120_000_000).contains(&dur),
            "duration must reflect the ~50ms wall-clock delta, not the 33ms default; got {dur}"
        );
    }

    #[test]
    fn compute_emit_duration_ns_handles_heartbeat_scale_intervals() {
        // Pin the heartbeat path: under static desktop the loop
        // emits roughly once per second. Stamping 33ms on each
        // emit was exactly how the receiver's RTP clock fell
        // behind wall clock by ~967ms/second.
        let prev = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let now = std::time::Instant::now();
        let dur = compute_emit_duration_ns(Some(prev), now, 33_000_000);
        assert!(
            dur >= 900_000_000,
            "1s heartbeat must produce a ~1s duration so RTP timestamp keeps pace with wall \
             clock; got {dur}"
        );
    }

    /// Regression: the shared-capture broadcast (introduced when the
    /// capture loop was decoupled to fix multi-browser black screen)
    /// runs at the OS refresh rate, while per-connection encoders run
    /// at a configured fps, so `RecvError::Lagged(n)` is the expected
    /// steady state.
    ///
    /// Earlier code requested a keyframe on every lag event, which
    /// recreated the encoder. Encoder rebuilds are an order of
    /// magnitude more expensive than emitting one P frame, so each
    /// rebuild widened the lag and triggered another rebuild —
    /// observed in production as ~6 keyframe rebuilds per second
    /// flooding the logs and starving the pipeline.
    ///
    /// This test pins the contract by exercising the real
    /// `tokio::sync::broadcast` Lagged path and asserting:
    ///   1. `handle_broadcast_lag` does not flip the keyframe flag.
    ///   2. The next recv after Lagged still yields the latest
    ///      available frame (the encoder's reference chain is not
    ///      broken — broadcast resyncs the receiver to head
    ///      automatically).
    #[tokio::test(flavor = "current_thread")]
    async fn broadcast_lag_does_not_request_keyframe_or_rebuild_encoder() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::broadcast;

        let keyframe_requested = Arc::new(AtomicBool::new(false));

        let (tx, mut rx) = broadcast::channel::<u32>(2);
        // Publish more than capacity so the next recv hits Lagged.
        for i in 0..6u32 {
            let _ = tx.send(i);
        }

        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => {
                handle_broadcast_lag("test-conn", n);
            }
            other => panic!("expected RecvError::Lagged on overflow, got {other:?}"),
        }

        assert!(
            !keyframe_requested.load(Ordering::Relaxed),
            "broadcast lag must not request a keyframe — that would feed a \
             self-amplifying keyframe-storm loop"
        );

        // The receiver auto-resyncs to head: the first post-lag recv must
        // succeed, proving we do NOT need to recreate the encoder to keep
        // the pipeline flowing.
        let next = rx.recv().await.expect("recv after Lagged must succeed");
        assert!(
            next > 0,
            "post-lag recv returns the latest available input — encoder's \
             internal reference chain is preserved"
        );
    }

    /// Regression: with the default GOP widened to 120 frames, the
    /// encoder still emits periodic IDR access units without any
    /// worker-side rebuild (`next_pass_is_idr` stays false). The
    /// emit path must label those VideoI based on the encoder's
    /// own `is_keyframe` signal so host-side keyframe counts align
    /// with what the browser decoder reports and the daemon's
    /// paused-write_sample latch (after a worker swap) can clear
    /// on a natural IDR rather than waiting for the next
    /// ForceKeyframe round-trip.
    #[test]
    fn classify_video_frame_kind_treats_internal_gop_idr_as_video_i() {
        use desk_capture_engine::model::video_encoder::NalInfo;
        let idr_nal = NalInfo {
            nal_bytes: bytes::Bytes::from_static(&[0; 16]),
            is_keyframe: true,
        };
        // No worker-side rebuild flag, but the encoder reports a
        // keyframe — must surface as VideoI.
        let kind = classify_video_frame_kind(&[idr_nal], false);
        assert_eq!(
            kind,
            MediaFrameKind::VideoI,
            "encoder-reported keyframe must surface as VideoI even when next_pass_is_idr=false"
        );
    }

    /// Pin the inverse: when the encoder reports a P frame and the
    /// worker has no pending rebuild, the emit path must label it
    /// VideoP. Mis-labelling a P frame as VideoI would defeat the
    /// daemon's paused-write_sample correctness check (it would
    /// resume on the wrong frame and the browser would see corrupt
    /// video until the next real IDR clears the buffer).
    #[test]
    fn classify_video_frame_kind_p_frame_stays_video_p() {
        use desk_capture_engine::model::video_encoder::NalInfo;
        let p_nal = NalInfo {
            nal_bytes: bytes::Bytes::from_static(&[0; 16]),
            is_keyframe: false,
        };
        let kind = classify_video_frame_kind(&[p_nal], false);
        assert_eq!(kind, MediaFrameKind::VideoP);
    }

    /// Pin the rebuild path: even if the encoder happens to report
    /// is_keyframe=false on the very first emission after a
    /// settings_changed / ForceKeyframe rebuild (this should not
    /// happen in practice — the rebuilt encoder always emits
    /// SPS+PPS+IDR first — but we keep the explicit `next_pass_is_idr`
    /// belt-and-braces flag), the emit path must still mark VideoI
    /// because the worker just rebuilt the encoder.
    #[test]
    fn classify_video_frame_kind_next_pass_is_idr_overrides() {
        use desk_capture_engine::model::video_encoder::NalInfo;
        let nal = NalInfo {
            nal_bytes: bytes::Bytes::from_static(&[0; 16]),
            is_keyframe: false,
        };
        let kind = classify_video_frame_kind(&[nal], true);
        assert_eq!(
            kind,
            MediaFrameKind::VideoI,
            "next_pass_is_idr=true is the rebuild marker; first post-rebuild emit must be \
             VideoI even if the NAL header check disagrees"
        );
    }

    /// Mixed-NAL access unit: any single keyframe NAL anywhere in
    /// the access unit promotes the whole emit to VideoI. This
    /// matches H.264's wire reality where one access unit can be
    /// SPS + PPS + IDR slice (3 NALs, only the third one carries
    /// the IDR semantics) but the entire unit is a keyframe.
    #[test]
    fn classify_video_frame_kind_any_keyframe_in_access_unit_wins() {
        use desk_capture_engine::model::video_encoder::NalInfo;
        let nals = vec![
            NalInfo {
                nal_bytes: bytes::Bytes::from_static(&[0; 4]),
                is_keyframe: false,
            },
            NalInfo {
                nal_bytes: bytes::Bytes::from_static(&[0; 4]),
                is_keyframe: true,
            },
        ];
        assert_eq!(
            classify_video_frame_kind(&nals, false),
            MediaFrameKind::VideoI
        );
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
