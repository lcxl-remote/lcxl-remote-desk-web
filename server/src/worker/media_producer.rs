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
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
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
    MediaFrameKind, MediaPipelineStatePayload, StartMediaPayload, StopMediaPayload,
    UpdateMediaSettingsPayload, WorkerToService,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect, Resolution};
use desk_signal_facade::model::media_capability::{
    EncoderCompatibility, EncoderCompatibilityError, VideoEncoderCapability, VideoEncoderId,
    capabilities_for_encoder_names, check_encoder_input, compatible_encoders,
};
use desk_signal_facade::model::media_pipeline::{MediaPipelinePhase, MediaPipelineStateData};
use desk_utils::error::DeskErrorCode;
use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc, watch};

use crate::worker::shared_capture::{CaptureKey, SharedCaptureRegistry};
#[cfg(target_os = "linux")]
use desk_wayland_portal::WaylandPortalBroker;

/// Per-connection media context. Holds the dedicated threads running
/// the capture + encode loops (one for video, one for audio) plus the
/// flags the event loop flips to drive them. Both pipelines share the
/// same `stop_flag` so `StopMedia` cleanly tears down both halves at
/// once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartMediaResult {
    Accepted(u64),
    AlreadyRunning,
    Cancelled(u64),
}

type GeometryUpdateHandler = dyn Fn(&str, u64, (i32, i32, i32, i32)) + Send + Sync + 'static;

struct ConnectionTask {
    generation: u64,
    start_payload: StartMediaPayload,
    stop_flag: Arc<AtomicBool>,
    stop_tx: watch::Sender<bool>,
    keyframe_requested: Arc<AtomicBool>,
    /// Live-update channel feeding fresh `UpdateMediaSettingsPayload`
    /// values into the video pipeline thread. `update_settings` posts
    /// here; the loop drains via `try_recv` on every tick and rebuilds
    /// ticker / encoder when the relevant knobs differ from the cached
    /// ones. Audio pipeline does not subscribe today (Opus owns its own
    /// frame size + bitrate and a runtime change would require a
    /// separate IPC variant).
    settings_tx: mpsc::UnboundedSender<UpdateMediaSettingsPayload>,
    /// Updated by the video thread. Only the deterministic `Blocked` value is
    /// eligible for an OS display-change retry; prepare/runtime failures stay
    /// user-driven so one event cannot create a restart loop.
    video_state: Arc<AtomicU8>,
    /// Held so the video task can be joined on `shutdown()`. None
    /// after the thread exits naturally on stop_flag observation.
    video_handle: Option<thread::JoinHandle<()>>,
    /// Audio pipeline handle. `None` when the worker did not build an
    /// audio pipeline for this connection — currently always spawned
    /// alongside video, but kept Optional so audio can later be disabled
    /// per connection without changing the field shape.
    audio_handle: Option<thread::JoinHandle<()>>,
}

const VIDEO_STATE_STARTING: u8 = 0;
const VIDEO_STATE_STREAMING: u8 = 1;
const VIDEO_STATE_BLOCKED: u8 = 2;
const VIDEO_STATE_FAILED: u8 = 3;
const VIDEO_STATE_DISABLED: u8 = 4;

fn should_retry_blocked_video(state: u8, start_video: bool, thread_finished: bool) -> bool {
    state == VIDEO_STATE_BLOCKED && start_video && thread_finished
}

impl ConnectionTask {
    fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        self.stop_tx.send_replace(true);
    }
}

impl Drop for ConnectionTask {
    fn drop(&mut self) {
        // Belt-and-braces: setting stop_flag here guarantees both
        // threads observe a stop request even if the caller forgot to
        // call `stop_media` (e.g. supervisor unwinding on a panic).
        self.request_stop();
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
    capture_key_generation: AtomicU64,
    geometry_update_handler: StdMutex<Option<Arc<GeometryUpdateHandler>>>,
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
        #[cfg(target_os = "linux")]
        {
            Self::new_impl(desk_settings, media_sender, error_tx, None)
        }
        #[cfg(not(target_os = "linux"))]
        Self::new_impl(desk_settings, media_sender, error_tx)
    }

    #[cfg(target_os = "linux")]
    pub fn new_with_portal(
        desk_settings: DeskSettings,
        media_sender: Arc<dyn MediaSender>,
        error_tx: mpsc::UnboundedSender<WorkerToService>,
        portal_broker: Arc<WaylandPortalBroker>,
    ) -> Self {
        Self::new_impl(desk_settings, media_sender, error_tx, Some(portal_broker))
    }

    fn new_impl(
        desk_settings: DeskSettings,
        media_sender: Arc<dyn MediaSender>,
        error_tx: mpsc::UnboundedSender<WorkerToService>,
        #[cfg(target_os = "linux")] portal_broker: Option<Arc<WaylandPortalBroker>>,
    ) -> Self {
        Self {
            desk_settings,
            media_sender,
            error_tx,
            capture_registry: {
                #[cfg(target_os = "linux")]
                {
                    SharedCaptureRegistry::new_with_portal(portal_broker)
                }
                #[cfg(not(target_os = "linux"))]
                {
                    SharedCaptureRegistry::new()
                }
            },
            capture_keys: Arc::new(StdMutex::new(HashMap::new())),
            capture_key_generation: AtomicU64::new(0),
            geometry_update_handler: StdMutex::new(None),
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

    pub fn set_geometry_update_handler(&self, handler: Arc<GeometryUpdateHandler>) {
        *self
            .geometry_update_handler
            .lock()
            .expect("media producer geometry handler lock poisoned") = Some(handler);
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
    pub fn start_media(&self, payload: StartMediaPayload) -> StartMediaResult {
        self.start_media_with(payload, |_| {})
    }

    pub fn start_media_with<F>(
        &self,
        payload: StartMediaPayload,
        on_accepted: F,
    ) -> StartMediaResult
    where
        F: FnOnce(u64),
    {
        let connection_id = payload.connection_id.clone();
        let (generation, stop_flag, stop_tx, stop_rx, keyframe_requested, video_state, settings_rx) = {
            let mut map = self.inner.lock().expect("media producer lock poisoned");
            if map.contains_key(&connection_id) {
                warn!(
                    "[MediaProducer] StartMedia for already-running connection {connection_id}; ignoring"
                );
                return StartMediaResult::AlreadyRunning;
            }
            let generation = self
                .capture_key_generation
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            let stop_flag = Arc::new(AtomicBool::new(false));
            let (stop_tx, stop_rx) = watch::channel(false);
            let keyframe_requested = Arc::new(AtomicBool::new(false));
            let video_state = Arc::new(AtomicU8::new(if payload.start_video {
                VIDEO_STATE_STARTING
            } else {
                VIDEO_STATE_DISABLED
            }));
            let (settings_tx, settings_rx) =
                mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
            map.insert(
                connection_id.clone(),
                ConnectionTask {
                    generation,
                    start_payload: payload.clone(),
                    stop_flag: Arc::clone(&stop_flag),
                    stop_tx: stop_tx.clone(),
                    keyframe_requested: Arc::clone(&keyframe_requested),
                    settings_tx,
                    video_state: Arc::clone(&video_state),
                    video_handle: None,
                    audio_handle: None,
                },
            );
            (
                generation,
                stop_flag,
                stop_tx,
                stop_rx,
                keyframe_requested,
                video_state,
                settings_rx,
            )
        };

        // The reservation above makes duplicate StartMedia atomic, while
        // releasing `inner` here keeps input setup (which may wait on a
        // Wayland RemoteDesktop Portal response) outside the producer lock.
        // The video thread is started only after the callback, so its first
        // geometry event cannot outrun input generation registration.
        on_accepted(generation);
        let geometry_update_handler = self
            .geometry_update_handler
            .lock()
            .expect("media producer geometry handler lock poisoned")
            .clone();
        if !payload.start_video && !payload.start_audio {
            info!(
                "[MediaProducer] StartMedia for {connection_id} requests neither video nor audio (DataChannel-only connection); skipping capture pipelines"
            );
        }
        let mut video_handle = if payload.start_video {
            Some(spawn_video_pipeline_thread(
                self.desk_settings.clone(),
                payload.clone(),
                Arc::clone(&self.media_sender),
                self.error_tx.clone(),
                Arc::clone(&stop_flag),
                stop_rx,
                Arc::clone(&keyframe_requested),
                settings_rx,
                Arc::clone(&self.capture_registry),
                Arc::clone(&self.capture_keys),
                generation,
                geometry_update_handler,
                video_state,
            ))
        } else {
            // Drain the receiver end so settings updates targeted at this
            // connection don't accumulate unbounded; closing it here is
            // symmetric with not spawning a consumer.
            drop(settings_rx);
            drop(stop_rx);
            debug!("[MediaProducer] {connection_id}: skipping video pipeline (start_video=false)");
            None
        };
        // The audio pipeline runs in its own dedicated thread (WASAPI
        // / PipeWire / SCKit handles are COM/system-thread-bound the
        // same way as the video capture, so a separate thread + a
        // current-thread Tokio runtime is the right shape).
        let mut audio_handle = if payload.start_audio {
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

        let handles_installed = {
            let mut map = self.inner.lock().expect("media producer lock poisoned");
            match map.get_mut(&connection_id) {
                Some(task) if task.generation == generation => {
                    task.video_handle = video_handle.take();
                    task.audio_handle = audio_handle.take();
                    true
                }
                _ => false,
            }
        };
        if !handles_installed {
            // StopMedia may run from the callback itself, or another StartMedia
            // may reserve the same id after a concurrent StopMedia. The
            // generation fence prevents old thread handles from being attached
            // to the replacement task.
            stop_flag.store(true, Ordering::Relaxed);
            stop_tx.send_replace(true);
            drop(video_handle);
            drop(audio_handle);
            debug!(
                "[MediaProducer] {connection_id}: start reservation was removed or replaced before pipeline handles were installed"
            );
            return StartMediaResult::Cancelled(generation);
        }
        StartMediaResult::Accepted(generation)
    }

    /// Restart only video pipelines that exited after deterministic dimension
    /// preflight. The existing audio task, peer connection, and data channels
    /// stay untouched. This is invoked by the low-frequency OS display watcher
    /// so returning from an unsupported mode (for example DCI 4K) to a supported
    /// mode can recover without keeping capture alive while blocked.
    pub fn retry_blocked_video_after_display_change<F>(&self, mut on_accepted: F) -> usize
    where
        F: FnMut(&str, u64),
    {
        let geometry_update_handler = self
            .geometry_update_handler
            .lock()
            .expect("media producer geometry handler lock poisoned")
            .clone();
        let mut accepted = Vec::new();
        let mut map = self.inner.lock().expect("media producer lock poisoned");
        for (connection_id, task) in map.iter_mut() {
            if !should_retry_blocked_video(
                task.video_state.load(Ordering::Acquire),
                task.start_payload.start_video,
                task.video_handle
                    .as_ref()
                    .is_some_and(thread::JoinHandle::is_finished),
            ) {
                continue;
            }

            let generation = self
                .capture_key_generation
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            let (settings_tx, settings_rx) =
                mpsc::unbounded_channel::<UpdateMediaSettingsPayload>();
            task.generation = generation;
            task.settings_tx = settings_tx;
            task.video_state
                .store(VIDEO_STATE_STARTING, Ordering::Release);
            task.keyframe_requested.store(true, Ordering::Relaxed);
            drop(task.video_handle.take());
            task.video_handle = Some(spawn_video_pipeline_thread(
                self.desk_settings.clone(),
                task.start_payload.clone(),
                Arc::clone(&self.media_sender),
                self.error_tx.clone(),
                Arc::clone(&task.stop_flag),
                task.stop_tx.subscribe(),
                Arc::clone(&task.keyframe_requested),
                settings_rx,
                Arc::clone(&self.capture_registry),
                Arc::clone(&self.capture_keys),
                generation,
                geometry_update_handler.clone(),
                Arc::clone(&task.video_state),
            ));
            accepted.push((connection_id.clone(), generation));
        }
        drop(map);

        for (connection_id, generation) in &accepted {
            on_accepted(connection_id, *generation);
            info!(
                "[MediaProducer] Retrying blocked video for {connection_id} after display change (generation={generation})"
            );
        }
        accepted.len()
    }

    /// Stop a per-connection pipeline. No-op on unknown id.
    pub fn stop_media(&self, payload: &StopMediaPayload) {
        let mut map = self.inner.lock().expect("media producer lock poisoned");
        if let Some(mut task) = map.remove(&payload.connection_id) {
            task.request_stop();
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

    /// Stop every active per-connection pipeline and return the affected ids.
    /// Used when the desktop Portal revokes the shared screen session: no
    /// connection may keep a stale capture or input pipeline alive.
    pub fn stop_all_media(&self) -> Vec<String> {
        let connection_ids = {
            let map = self.inner.lock().expect("media producer lock poisoned");
            map.keys().cloned().collect::<Vec<_>>()
        };
        for connection_id in &connection_ids {
            self.stop_media(&StopMediaPayload {
                connection_id: connection_id.clone(),
            });
        }
        connection_ids
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
            task.request_stop();
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
            video_encoder_capabilities: capabilities_for_encoder_names(&video_encoders),
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

mod audio;
mod frame;
mod pipeline;
mod settings;
mod video;

use audio::audio_pipeline_loop;
use frame::{build_media_frame, log_post_rebuild_emit, send_frame, send_frame_or_stop};
use pipeline::{spawn_audio_pipeline_thread, spawn_video_pipeline_thread};
use settings::{
    capturable_device_name, classify_video_frame_kind, codec_from_str, compute_emit_duration_ns,
    display_info_for_size, drain_settings_updates, handle_broadcast_lag, payload_overrides,
    replay_bitrate_cap, should_recreate_for_resolution,
};
use video::video_pipeline_loop;

#[cfg(test)]
use frame::h264_walk_nals;
#[cfg(test)]
use settings::video_codec_name;

#[cfg(test)]
mod tests;
