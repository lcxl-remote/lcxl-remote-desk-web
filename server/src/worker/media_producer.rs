//! # Worker-side media producer
//!
//! Owns the screen capture loop and the per-`connection_id` video
//! encoder pool. Replaces the in-`service::signaling`-mod
//! `capture_screen_task` that used to run one capture pipeline per peer
//! connection in the worker; the daemon now owns the
//! `RTCPeerConnection` and the worker pushes encoded
//! [`MediaFrame`](desk_ipc_protocol::message::MediaFrame)s over a
//! dedicated [media transport](desk_ipc_protocol::dual_transport::MediaSender).
//!
//! ## Video capture + encode
//!
//! - Per-`connection_id` capture + encoder pair, each driven by a
//!   dedicated OS thread with its own current-thread Tokio runtime
//!   (DXGI / WASAPI handles are COM-bound and thread-affine, so a
//!   dedicated thread per pipeline is the safe choice).
//! - `StartMedia` / `StopMedia` / `ForceKeyframe` / `UpdateMediaSettings`
//!   handlers driven from the worker event loop.
//! - `MediaCapabilities` snapshot constructor used by `worker::session`
//!   to send a one-shot `WorkerToService::Capabilities` to the daemon
//!   on Init.
//!
//! ## Audio + cursor sync
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
//! ## Live settings updates
//!
//! - **`UpdateMediaSettings` live-apply** — fps / quality changes
//!   surface through `update_settings` → per-pipeline mpsc, drained
//!   on the next encode tick, encoder rebuilt in place without
//!   restarting capture. `bitrate_kbps` rides the same channel as a
//!   runtime bitrate-cap directive (`Some(0)` clears, `Some(k)` caps
//!   at k kbps) applied through `VideoEncoder::set_bitrate_cap`
//!   *without* rebuilding the encoder — the daemon's REMB controller
//!   emits these at ~1 Hz and a rebuild per directive would cause an
//!   IDR storm. The active cap is replayed onto every freshly rebuilt
//!   encoder (settings / keyframe / resolution rebuilds).
//!
//! ## Capture sharing across connections
//!
//! Each per-connection `video_pipeline_loop` subscribes to the
//! worker-wide `SharedCaptureRegistry` (see `worker::shared_capture`)
//! keyed by `(backend, output_index)`. Connections asking for the
//! same key reuse one capture loop and one OS-level capture
//! instance; the broadcast channel fans frames out to each encoder
//! thread. This is the **correctness** layer for the multi-browser
//! scenario, not just an optimisation: an earlier design assumed
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    list_effective_image_output, list_image_capture,
};
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
use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc};

use crate::worker::shared_capture::{CaptureKey, SharedCaptureRegistry};

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
    /// Audio pipeline handle. `None` when the worker did not build an
    /// audio pipeline for this connection — currently always spawned
    /// alongside video, but kept Optional so audio can later be disabled
    /// per connection without changing the field shape.
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
    /// Per-connection effective `CaptureKey`, populated by the video
    /// pipeline thread as soon as `capture_registry.subscribe`
    /// returns. The `SetVirtualDisplayMode` path reads this to decide
    /// (a) whether the connection is on WGC backend at all and (b)
    /// which `CaptureKey` to invalidate at the shared-capture
    /// registry. Cleared by a per-thread `CaptureKeyGuard` so every
    /// exit path (normal, error, panic) leaves no stale entry.
    capture_keys: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>>,
    /// Monotonic generation counter for `CaptureKeyRecord` entries.
    /// Bumped once per successful `capture_registry.subscribe` so the
    /// `CaptureKeyGuard` drop check can distinguish "I wrote this
    /// entry" from "someone overwrote it after me". See the docstring
    /// on `CaptureKeyGuard` for the race this defeats.
    capture_key_generation: Arc<AtomicU64>,
    inner: StdMutex<HashMap<String, ConnectionTask>>,
}

/// `(CaptureKey, generation)` record stored in the producer's
/// `capture_keys` map. The generation tag exists solely so the
/// `CaptureKeyGuard` Drop impl can avoid removing an entry that a
/// later pipeline overwrote — see the doc on `CaptureKeyGuard`.
#[derive(Clone, Debug)]
struct CaptureKeyRecord {
    key: CaptureKey,
    generation: u64,
}

/// RAII guard that drops a `(connection_id, CaptureKey)` entry from
/// the producer's `capture_keys` map on every exit path of the video
/// pipeline thread — normal return, `?`-propagated error, or panic
/// unwind. Without this guard a subscribe error mid-spawn would leak
/// the previous entry (or absence of one) into the next
/// `SetVirtualDisplayMode` decision.
///
/// **Generation token** — `stop_media` is fire-and-forget: it only
/// sets the old pipeline's stop flag, it does not join the thread.
/// `start_media` immediately spawns a new pipeline that may finish
/// `subscribe()` and write `capture_keys[conn_id]` before the *old*
/// pipeline's stack has unwound. If the old guard removed by
/// `connection_id` alone it would erase the new pipeline's freshly
/// written entry, causing the next `SetVirtualDisplayMode` to look up
/// `connection_capture_key` and get `None` → silent WGC restart skip
/// → the user's next browser resize freezes the frame again. The
/// guard stores the generation that *its* pipeline wrote and only
/// removes the entry if the current record still matches.
struct CaptureKeyGuard {
    map: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>>,
    connection_id: String,
    generation: u64,
}

impl Drop for CaptureKeyGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.map.lock()
            && m.get(&self.connection_id)
                .is_some_and(|r| r.generation == self.generation)
        {
            m.remove(&self.connection_id);
        }
    }
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
            capture_keys: Arc::new(StdMutex::new(HashMap::new())),
            capture_key_generation: Arc::new(AtomicU64::new(0)),
            inner: StdMutex::new(HashMap::new()),
        }
    }

    /// Effective `CaptureKey` for an active connection, or `None` if the
    /// connection's video pipeline hasn't reached the post-subscribe
    /// recording step (e.g. data-channel-only `StartMedia`, or a
    /// subscribe error that aborted the thread). Used by
    /// `session.rs::SetVirtualDisplayMode` to filter restart candidates
    /// by backend.
    pub fn connection_capture_key(&self, connection_id: &str) -> Option<CaptureKey> {
        self.capture_keys
            .lock()
            .ok()?
            .get(connection_id)
            .map(|r| r.key.clone())
    }

    /// Force-evict a shared-capture registry slot. Thin delegate to
    /// `SharedCaptureRegistry::invalidate_key` so `session.rs` does not
    /// need a direct reference to the registry (which is a private
    /// field of this producer).
    pub fn invalidate_capture_key(&self, key: &CaptureKey) -> bool {
        self.capture_registry.invalidate_key(key)
    }

    /// Cached `DisplayInfo` of the surface a connection is capturing, or
    /// `None` if it has no live capture slot. On Wayland this carries the
    /// portal stream's real position/size, which the display-change
    /// refresh uses as a geometry anchor.
    pub fn connection_display_info(&self, connection_id: &str) -> Option<DisplayInfo> {
        let key = self.connection_capture_key(connection_id)?;
        self.capture_registry.display_info_for_key(&key)
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
                Arc::clone(&self.capture_keys),
                Arc::clone(&self.capture_key_generation),
            ))
        } else {
            // Drain the receiver end so settings updates targeted at this
            // connection don't accumulate unbounded; closing it here is
            // symmetric with not spawning a consumer.
            drop(settings_rx);
            debug!("[MediaProducer] {connection_id}: skipping video pipeline (start_video=false)");
            None
        };
        // Audio pipeline runs in its own dedicated thread (WASAPI
        // / PipeWire / SCKit handles are COM/system-thread-bound the
        // same way as the video capture, so a separate thread + a
        // current-thread Tokio runtime is the right shape).
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

    /// Live-update knobs (fps / quality / bitrate cap). The video
    /// pipeline thread owns the encoder + ticker, so we deliver via the
    /// per-connection `settings_tx` mpsc channel; the loop's `try_recv`
    /// drains all pending updates on the next tick, retuning the frame
    /// interval (fps) and rebuilding the encoder (quality).
    ///
    /// `bitrate_kbps` does *not* go through the rebuild path: it is
    /// folded into a tri-state cap directive and applied in place via
    /// [`VideoEncoder::set_bitrate_cap`], because cap updates arrive at
    /// REMB cadence (~1 Hz) and rebuilding per update would cause an
    /// IDR storm. Codecs that do not implement `set_bitrate_cap` fall
    /// back to the trait default and ignore the cap.
    ///
    /// No-op on unknown connection_id.
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
        // grouping it had in the legacy worker-owned-PC path.
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

/// Outcome of draining the live-settings channel on one encode tick.
struct SettingsDrainOutcome {
    /// At least one knob that requires an encoder rebuild (fps /
    /// quality) actually changed.
    needs_rebuild: bool,
    /// Latest bitrate-cap directive in the drained batch, if any:
    /// `Some(Some(k))` caps at `k` kbps, `Some(None)` clears the cap
    /// (wire sentinel `bitrate_kbps == Some(0)`), `None` means the
    /// batch carried no cap directive. Applied to the encoder via
    /// `VideoEncoder::set_bitrate_cap` — never by rebuilding, since
    /// cap updates arrive at REMB cadence (~1 Hz) and a rebuild per
    /// update would cause an IDR storm.
    cap_directive: Option<Option<u32>>,
}

/// Drains every pending `UpdateMediaSettingsPayload` and folds the
/// encoder-relevant knobs into `merged_settings`, coalescing a burst
/// of updates into a single outcome. fps changes also retune the
/// frame interval (the ticker can't be adjusted in place).
///
/// `needs_rebuild` is only set when a knob actually changed — we
/// compare to the *current* `merged_settings` rather than the IPC
/// payload directly so coalesced updates that converge to the same
/// value as the live state are no-ops (the daemon fans out on every
/// `UpdateDeskSettings`, including ones that don't move encoder-
/// relevant fields).
fn drain_settings_updates(
    connection_id: &str,
    settings_rx: &mut mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    merged_settings: &mut DeskSettings,
    frame_interval: &mut Duration,
    frame_duration_ns: &mut u64,
) -> SettingsDrainOutcome {
    let mut changed = false;
    let mut cap_directive: Option<Option<u32>> = None;
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
        if let Some(kbps) = payload.bitrate_kbps {
            // Tri-state wire semantics (see the IPC field's doc):
            // Some(0) clears the cap, Some(k>0) caps at k kbps. Keep
            // only the newest directive in the batch — the daemon's
            // controller already rate-limits, and only the latest
            // value matters.
            cap_directive = Some(if kbps == 0 { None } else { Some(kbps) });
            debug!(
                "[MediaProducer:{connection_id}] UpdateMediaSettings.bitrate_kbps={kbps} → cap \
                 directive {:?}",
                cap_directive
            );
        }
    }
    SettingsDrainOutcome {
        needs_rebuild: changed,
        cap_directive,
    }
}

/// Re-applies the connection's current bitrate cap onto a freshly
/// rebuilt encoder (rebuilds reset codec state, dropping any cap that
/// was applied at runtime). No-op when no cap is active.
fn replay_bitrate_cap(
    encoder: &mut Box<dyn VideoEncoder>,
    current_cap_kbps: Option<u32>,
    connection_id: &str,
) {
    if let Some(kbps) = current_cap_kbps
        && !encoder.set_bitrate_cap(Some(kbps))
    {
        debug!(
            "[MediaProducer:{connection_id}] encoder does not support bitrate caps; {kbps} kbps \
             cap not re-applied after rebuild"
        );
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
    // v4 capture-selection fix: the IPC field carries the exact
    // `\\.\DISPLAYn` string the browser selected from the dropdown
    // (sourced via daemon → `StartMediaPayload.video_device`).
    // Overriding the worker's base setting here lets a second browser
    // pick a different monitor than the first without colliding on
    // shared capture state. `None` (legacy daemons or callers that
    // really want the worker default) leaves `s.video_device_name`
    // untouched — the capture-engine surfaces INVALID_PARAMS at
    // `new()` time if it is still empty, which the daemon prevents
    // by gating on the browser dialog's form validation.
    if let Some(name) = payload.video_device.as_deref() {
        s.video_device_name = name.to_string();
    }
    s
}

/// `Some(new)` if the encoder must be torn down and rebuilt because
/// the frame dimensions diverge from what the encoder was constructed
/// with. `None` if dimensions match or the frame is the no-content
/// sentinel (width=0 or height=0).
///
/// The (0,0) short-circuit covers `EmptyImageInfo` placeholders
/// emitted by WGC's `WAIT_TIMEOUT` branch, WGC's frame-pool-resize
/// branch, and DXGI's `NoContentChange` branch — every backend
/// surfaces width=0,height=0 for "no real frame this tick", and
/// re-creating the encoder against 0x0 would either error or feed an
/// invalid configuration to libvpx / x264.
fn should_recreate_for_resolution(init: (u32, u32), frame: (u32, u32)) -> Option<(u32, u32)> {
    if frame.0 == 0 || frame.1 == 0 {
        return None;
    }
    if init != frame { Some(frame) } else { None }
}

/// Build a synthetic `DisplayInfo` carrying `(width, height)` but
/// preserving every other field from `base` (device_name, resolutions,
/// rotation, attached_to_desktop, display_device_name). Used to feed
/// `create_video_encoder` the *current* encoder size; the encoder only
/// consumes `desktop_coordinates.width()/height()`, so left/top stay
/// as in `base` and right/bottom are derived.
fn display_info_for_size(base: &DisplayInfo, size: (u32, u32)) -> DisplayInfo {
    let mut di = base.clone();
    let left = di.desktop_coordinates.left;
    let top = di.desktop_coordinates.top;
    di.desktop_coordinates = DisplayRect {
        left,
        top,
        right: left + size.0 as i32,
        bottom: top + size.1 as i32,
    };
    di
}

/// Returns a live, capturable device name for `requested` given `live` (the
/// display list for the effective capture backend): the requested name if it
/// is attached and capturable, otherwise the primary (origin 0,0), otherwise
/// the first usable display. Only `attached_to_desktop` displays with a
/// non-zero surface are considered — mirroring the input dispatcher
/// (`enumerate_attached_displays` + `geometry_for_device_in`). Returns `None`
/// when no substitution should happen: empty `requested` (preserve the
/// downstream "no display selected" hard error) or no usable display (leave
/// the name untouched and let the capture backend surface its own error).
fn capturable_device_name(live: &[DisplayInfo], requested: &str) -> Option<String> {
    if requested.is_empty() {
        return None;
    }
    let usable: Vec<&DisplayInfo> = live
        .iter()
        .filter(|d| {
            d.attached_to_desktop
                && d.desktop_coordinates.width() > 0
                && d.desktop_coordinates.height() > 0
        })
        .collect();
    if usable.is_empty() {
        return None;
    }
    if usable.iter().any(|d| d.device_name == requested) {
        return Some(requested.to_string());
    }
    let primary = usable
        .iter()
        .find(|d| d.desktop_coordinates.left == 0 && d.desktop_coordinates.top == 0);
    Some(primary.unwrap_or(&usable[0]).device_name.clone())
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
    capture_keys: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>>,
    capture_key_generation: Arc<AtomicU64>,
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
                    capture_keys,
                    capture_key_generation,
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

/// Spawn the dedicated thread that owns one connection's audio
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
/// frame behaviour: on a static desktop emit one cached frame per
/// second so the receiver does not stall.
async fn video_pipeline_loop(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
    keyframe_requested: Arc<AtomicBool>,
    mut settings_rx: mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    capture_registry: Arc<SharedCaptureRegistry>,
    capture_keys: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>>,
    capture_key_generation: Arc<AtomicU64>,
) -> Result<(), String> {
    let connection_id = payload.connection_id.clone();
    let codec = payload.video_codec;
    let mut merged_settings = payload_overrides(&base_settings, &payload);

    info!(
        "[MediaProducer:{connection_id}] Starting pipeline: codec={codec:?}, fps={}, \
         enable_dirty_rect={}",
        merged_settings.video_fps, merged_settings.enable_dirty_rect
    );

    // Guard against a stale capability snapshot: an IDD virtual display can be
    // advertised in INIT (and chosen by the client) yet be gone by capture
    // time. Substitute a live, capturable display instead of hard-erroring,
    // mirroring the input dispatcher's geometry fallback.
    if !merged_settings.video_device_name.is_empty()
        && let Ok(live) = list_effective_image_output(&merged_settings)
        && let Some(name) = capturable_device_name(&live, &merged_settings.video_device_name)
        && name != merged_settings.video_device_name
    {
        warn!(
            "[MediaProducer:{connection_id}] requested display {:?} not live; \
             falling back to {:?}",
            merged_settings.video_device_name, name
        );
        merged_settings.video_device_name = name;
    }

    // Subscribe to the shared capture loop for this `(backend,
    // output)`. If no loop exists the registry spawns one;
    // otherwise the existing loop's broadcast sender hands us a
    // fresh receiver. `display_info` is published by the registry
    // (the capture instance owns it) so we don't need our own
    // capture handle to re-derive resolution.
    let capture_handle = capture_registry
        .subscribe(&merged_settings)
        .map_err(|e| format!("{e}"))?;

    // Publish the effective `CaptureKey` so `SetVirtualDisplayMode`
    // can decide whether this connection's backend is WGC (needs a
    // forced rebuild after IddCx remount) or one that self-adapts
    // (DXGI / GDI). The RAII guard below ensures we clean up on every
    // exit path: normal return, `?`-propagated encoder error, or panic
    // unwind — without it a subscribe-time success followed by a later
    // failure would leak the entry past the connection's lifetime.
    //
    // The generation tag is what makes the cleanup safe in the face
    // of a Stop+Start race. `stop_media` does not block-join the
    // outgoing thread, so the *next* `start_media` for the same
    // connection_id may spawn a new pipeline that finishes subscribe
    // and overwrites this entry before the old thread's stack
    // unwinds. Tagging the record with the generation we just bumped
    // — and re-checking it in `CaptureKeyGuard::drop` — lets the old
    // guard recognise "the slot no longer belongs to me, leave it
    // alone." Without this token the old guard would erase the new
    // pipeline's freshly recorded key, and the next
    // `SetVirtualDisplayMode` would silently skip the WGC restart.
    let generation = capture_key_generation.fetch_add(1, Ordering::Relaxed);
    capture_keys
        .lock()
        .expect("media producer capture_keys lock poisoned")
        .insert(
            connection_id.clone(),
            CaptureKeyRecord {
                key: capture_handle.key().clone(),
                generation,
            },
        );
    let _capture_key_guard = CaptureKeyGuard {
        map: Arc::clone(&capture_keys),
        connection_id: connection_id.clone(),
        generation,
    };

    let mut frame_rx = capture_handle.subscribe();
    let display_info = capture_handle.display_info().clone();

    // `encoder_init_size` is the *only* authoritative source of the
    // encoder's current width/height. Every `create_video_encoder`
    // call below feeds through `display_info_for_size(&display_info,
    // encoder_init_size)` so settings_changed / keyframe_requested
    // rebuilds never accidentally drop back to the (stale) subscribe-
    // time resolution after a mid-session display mode change.
    let mut encoder_init_size: (u32, u32) = (
        display_info.desktop_coordinates.width() as u32,
        display_info.desktop_coordinates.height() as u32,
    );
    let mut encoder: Box<dyn VideoEncoder> = create_video_encoder(
        &merged_settings,
        &display_info_for_size(&display_info, encoder_init_size),
    )
    .map_err(|e| format!("{e}"))?;
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
    // Connection-transient bitrate cap (kbps) driven by the daemon's
    // REMB controller via `UpdateMediaSettings.bitrate_kbps`. Not part
    // of `merged_settings` — it is runtime state, never persisted, and
    // must be replayed onto every freshly rebuilt encoder. `None` =
    // encoder runs at its initial ceiling.
    let mut current_cap_kbps: Option<u32> = None;

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
        let drain_outcome = drain_settings_updates(
            &connection_id,
            &mut settings_rx,
            &mut merged_settings,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        if drain_outcome.needs_rebuild {
            info!(
                "[MediaProducer:{connection_id}] Live settings changed; recreating encoder \
                 (fps={}, video_quality={}, enable_dirty_rect={})",
                merged_settings.video_fps,
                merged_settings.video_quality,
                merged_settings.enable_dirty_rect
            );
            encoder = create_video_encoder(
                &merged_settings,
                &display_info_for_size(&display_info, encoder_init_size),
            )
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
        // Bitrate-cap directives apply *after* a potential rebuild so
        // a batch carrying both a quality change and a cap lands on
        // the new encoder. Without a fresh directive, a rebuild
        // replays the connection's current cap (the new encoder
        // starts at its initial ceiling).
        match drain_outcome.cap_directive {
            Some(directive) => {
                if !encoder.set_bitrate_cap(directive) {
                    debug!(
                        "[MediaProducer:{connection_id}] bitrate cap directive {directive:?} not \
                         applied (encoder unsupported or reconfig failed)"
                    );
                }
                current_cap_kbps = directive;
            }
            None if drain_outcome.needs_rebuild => {
                replay_bitrate_cap(&mut encoder, current_cap_kbps, &connection_id);
            }
            None => {}
        }

        if keyframe_requested.swap(false, Ordering::Relaxed) {
            info!(
                "[MediaProducer:{connection_id}] Keyframe requested; recreating encoder so the \
                 next encode pass emits an IDR"
            );
            encoder = create_video_encoder(
                &merged_settings,
                &display_info_for_size(&display_info, encoder_init_size),
            )
            .map_err(|e| format!("{e}"))?;
            replay_bitrate_cap(&mut encoder, current_cap_kbps, &connection_id);
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

        // Resolution change detection: only consult on real content
        // frames (we already passed the heartbeat / no-content guard
        // above). `should_recreate_for_resolution` additionally
        // short-circuits (0,0) defensively in case any backend leaks
        // an EmptyImageInfo placeholder past the content_changed flag.
        if let Some((new_w, new_h)) = should_recreate_for_resolution(
            encoder_init_size,
            (shared_frame.width, shared_frame.height),
        ) {
            info!(
                "[MediaProducer:{connection_id}] Frame resolution changed {:?} -> {:?}; \
                 recreating encoder",
                encoder_init_size,
                (new_w, new_h)
            );
            // Update encoder_init_size FIRST so the synthetic
            // DisplayInfo built below carries the new dimensions.
            encoder_init_size = (new_w, new_h);
            encoder = create_video_encoder(
                &merged_settings,
                &display_info_for_size(&display_info, encoder_init_size),
            )
            .map_err(|e| format!("{e}"))?;
            replay_bitrate_cap(&mut encoder, current_cap_kbps, &connection_id);
            next_pass_is_idr = true;
            rebuild_pending = true;
            last_emit_for_throttle = std::time::Instant::now()
                .checked_sub(frame_interval)
                .unwrap_or_else(std::time::Instant::now);
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

/// Inner async loop for audio. Mirrors the legacy `capture_audio_task`:
/// 5 ms ticker drives an inner buffer-drain loop
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

    // 5 ms outer tick + inner drain loop sets the pacing —
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
                    // change, sleep/resume). Recreate the capture and
                    // continue from the next tick.
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
            // to the ticker without sending.
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
    for (idx, (off, _)) in starts.iter().enumerate() {
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
mod tests;
