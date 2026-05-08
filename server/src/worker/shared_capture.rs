//! Shared image-capture broadcaster for the worker side of Arch IV.
//!
//! Two browsers connecting to the same desktop both want a frame
//! stream from `(backend, output_index)` — but the OS-level capture
//! sources (DXGI Output Duplication in particular) treat
//! `IDXGIOutputDuplication::DuplicateOutput` as exclusive: the second
//! call against the same output in the same process returns
//! `E_INVALIDARG`. Pre-Arch IV the worker spawned a dedicated
//! `ImageCapture` per connection; the second connection therefore
//! crashed its video pipeline and the user saw a black screen on the
//! second browser tab.
//!
//! This module fixes that by giving the worker a `SharedCaptureRegistry`
//! that hands out `SharedCaptureHandle`s keyed by `(backend,
//! output_index)`. The first handle for a key spawns one capture
//! thread that pumps `Arc<SharedFrame>`s into a `tokio::sync::broadcast`
//! channel; subsequent handles for the same key reuse the existing
//! channel. The capture thread stops automatically when the last
//! handle drops (`Arc<SharedInner>` count → 0 → `Drop` sets the stop
//! flag and removes the registry entry).
//!
//! Concurrency contract:
//! - One capture loop per `CaptureKey`. Different connections that
//!   pick *different* backends or *different* output indices each get
//!   their own loop — e.g. one DXGI on display 0 + one GDI on display
//!   0 coexist on two threads, two DXGI on display 0 share one
//!   thread.
//! - Frames are `Arc<SharedFrame>` so the broadcast channel only
//!   refcount-bumps; the BGRA bytes themselves are copied once at
//!   capture time (the OS staging buffer pointed to by
//!   `CaptureResult::image` is invalidated on the next `capture()`
//!   call, so the broadcast cannot hand out the borrow).
//! - Cursor mode is hard-pinned to `SyncNative` here. Per-encoder
//!   `show_mouse` is honoured downstream (in the per-connection
//!   pipeline) by deciding whether to forward the cursor metadata
//!   over the connection's `cursor_sync_event` DataChannel — the
//!   shared frame never has the cursor baked in. Both DXGI and GDI
//!   advertise `supports_cursor_sync = true` today so this is
//!   universal on Windows; the abstraction is platform-agnostic
//!   regardless.
//! - Backpressure: `broadcast::Sender::send` returns `Err(SendError)`
//!   only when there are zero subscribers, which is fine — there is
//!   always at least one handle alive (otherwise the capture loop
//!   would have stopped). Slow subscribers see `Lagged` errors when
//!   they try to receive; we treat that the same as a dropped P-frame
//!   and request a keyframe upstream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use bytes::Bytes;
use desk_capture_engine::{
    error::CaptureError,
    image_capture::image_capture_factory::create_image_capture,
    model::image_capture::{
        CaptureRequest, CursorCaptureMode, CursorSyncData, DirtyRect, ImageCapture,
        ImageCaptureType, ImageCaptureTypeHelper, ImageInfo, ImageType,
    },
};
use desk_signal_facade::model::{desk_settings::DeskSettings, image_capture::DisplayInfo};
use tokio::sync::broadcast;
use tracing::{debug, info};

/// Identifies one capture instance. Connections sharing the same key
/// reuse the same capture loop; distinct keys get separate loops.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct CaptureKey {
    pub backend: String,
    pub output_index: u32,
}

/// One captured frame, shaped for `Arc` sharing across encoder
/// threads. Implements `ImageInfo` so it can be fed straight into the
/// existing `VideoEncoder::encode` path.
#[derive(Debug)]
pub struct SharedFrame {
    pub data: Bytes,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub image_type: ImageType,
    pub dirty_rects: Option<Vec<DirtyRect>>,
    pub content_changed: bool,
    pub cursor_update: Option<CursorSyncData>,
    pub display_info: DisplayInfo,
}

impl ImageInfo for SharedFrame {
    fn get_type(&self) -> ImageType {
        self.image_type
    }
    fn get_data(&self) -> &[u8] {
        &self.data
    }
    fn get_width(&self) -> u32 {
        self.width
    }
    fn get_height(&self) -> u32 {
        self.height
    }
    fn get_stride(&self) -> u32 {
        self.stride
    }
    fn get_dirty_rects(&self) -> Option<&[DirtyRect]> {
        self.dirty_rects.as_deref()
    }
}

/// Internal state shared between all `SharedCaptureHandle`s for a key.
struct SharedInner {
    sender: broadcast::Sender<Arc<SharedFrame>>,
    stop_flag: Arc<AtomicBool>,
    join_handle: StdMutex<Option<JoinHandle<()>>>,
    display_info: DisplayInfo,
    key: CaptureKey,
    /// Weak so registry can outlive an inner that is being dropped
    /// (and so an inner being dropped can call back into the registry
    /// to remove its stale entry).
    registry: Weak<SharedCaptureRegistry>,
}

impl Drop for SharedInner {
    fn drop(&mut self) {
        // Last handle went away — tell the capture thread to exit and
        // clean up the registry slot. Stop-flag is observed at the
        // top of the capture loop, so the thread exits within one
        // tick (~16ms at the typical 60 Hz refresh rate).
        self.stop_flag.store(true, Ordering::Release);
        if let Some(reg) = self.registry.upgrade() {
            reg.remove_stale_entry(&self.key);
        }
        // Detach the join handle: blocking Drop on the worker IPC
        // path would stall every other connection. The thread
        // observes stop_flag and exits on its own; the OS reclaims it
        // on natural exit.
        let _ = self.join_handle.lock().unwrap().take();
    }
}

/// Cheap clone-able subscription handle. The capture loop runs as long
/// as at least one of these is alive for a given `CaptureKey`.
#[derive(Clone)]
pub struct SharedCaptureHandle {
    inner: Arc<SharedInner>,
}

impl SharedCaptureHandle {
    /// Subscribe to the broadcast stream. Each call returns a fresh
    /// `Receiver` — the underlying ring buffer is shared across all
    /// receivers, so a slow consumer falling behind only manifests on
    /// that consumer (as `RecvError::Lagged`), not on its peers.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SharedFrame>> {
        self.inner.sender.subscribe()
    }

    pub fn display_info(&self) -> &DisplayInfo {
        &self.inner.display_info
    }

    pub fn key(&self) -> &CaptureKey {
        &self.inner.key
    }
}

/// Registry of live capture instances, keyed by `(backend,
/// output_index)`. Construct once per worker; share via `Arc`.
pub struct SharedCaptureRegistry {
    map: StdMutex<HashMap<CaptureKey, Weak<SharedInner>>>,
}

impl SharedCaptureRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            map: StdMutex::new(HashMap::new()),
        })
    }

    /// Subscribe to the capture stream for the given settings. Reuses
    /// an existing capture loop if one is alive for the same key,
    /// otherwise creates one. The resulting `SharedCaptureHandle`
    /// keeps the loop alive — drop it (e.g. when the connection ends)
    /// and the loop stops once no other handle remains.
    pub fn subscribe(
        self: &Arc<Self>,
        settings: &DeskSettings,
    ) -> Result<SharedCaptureHandle, CaptureError> {
        let key = key_for_settings(settings)?;
        // Fast path: existing live entry.
        {
            let mut g = self.map.lock().expect("shared capture registry poisoned");
            if let Some(weak) = g.get(&key) {
                if let Some(inner) = weak.upgrade() {
                    return Ok(SharedCaptureHandle { inner });
                }
                // Stale Weak — last handle just dropped. Remove and
                // fall through to construct fresh.
                g.remove(&key);
            }
        }
        // Slow path: build the capture instance + spawn the loop.
        // The first capture instance is built synchronously so the
        // caller observes `CaptureError` directly (e.g. wrong output
        // index, missing display device).
        let initial_capture = create_image_capture(settings)?;
        let display_info = initial_capture.get_current_output()?;

        let (sender, _) = broadcast::channel::<Arc<SharedFrame>>(8);
        let stop_flag = Arc::new(AtomicBool::new(false));

        let sender_for_thread = sender.clone();
        let stop_for_thread = Arc::clone(&stop_flag);
        let key_for_thread = key.clone();
        let display_info_for_thread = display_info.clone();
        let join = thread::Builder::new()
            .name(format!(
                "shared-capture-{}-{}",
                key.backend, key.output_index
            ))
            .spawn(move || {
                run_capture_loop(
                    initial_capture,
                    sender_for_thread,
                    stop_for_thread,
                    key_for_thread,
                    display_info_for_thread,
                );
            })
            .expect("spawn shared capture thread");

        let inner = Arc::new(SharedInner {
            sender,
            stop_flag,
            join_handle: StdMutex::new(Some(join)),
            display_info,
            key: key.clone(),
            registry: Arc::downgrade(self),
        });

        let mut g = self.map.lock().expect("shared capture registry poisoned");
        // Re-check in case another thread raced us. If a different
        // entry got inserted, we lost the race — return the winner
        // and let our own freshly-built `inner` drop, which cleans
        // up its capture thread.
        if let Some(existing_weak) = g.get(&key)
            && let Some(existing_inner) = existing_weak.upgrade()
        {
            return Ok(SharedCaptureHandle {
                inner: existing_inner,
            });
        }
        g.insert(key, Arc::downgrade(&inner));
        Ok(SharedCaptureHandle { inner })
    }

    fn remove_stale_entry(&self, key: &CaptureKey) {
        let mut g = self.map.lock().expect("shared capture registry poisoned");
        // Only remove if the slot's Weak has actually expired —
        // otherwise we would knock out a brand-new entry inserted
        // mid-Drop by a racing subscribe path.
        if let Some(weak) = g.get(key)
            && weak.upgrade().is_none()
        {
            g.remove(key);
        }
    }

    /// Diagnostic / test introspection: count of live capture loops.
    pub fn live_count(&self) -> usize {
        let g = self.map.lock().expect("shared capture registry poisoned");
        g.values().filter(|w| w.upgrade().is_some()).count()
    }
}

fn key_for_settings(settings: &DeskSettings) -> Result<CaptureKey, CaptureError> {
    let backend: ImageCaptureType = settings.get_image_capture_type()?;
    Ok(CaptureKey {
        backend: <&'static str>::from(backend).to_string(),
        output_index: settings.video_device_index,
    })
}

fn run_capture_loop(
    mut capture: Box<dyn ImageCapture + Send>,
    sender: broadcast::Sender<Arc<SharedFrame>>,
    stop_flag: Arc<AtomicBool>,
    key: CaptureKey,
    initial_display_info: DisplayInfo,
) {
    info!(
        "[SharedCapture:{}/{}] capture loop starting",
        key.backend, key.output_index
    );
    // Cursor mode is fixed: the shared frame must not have the cursor
    // baked in because different subscribers may have different
    // `show_mouse` settings. SyncNative emits cursor metadata in
    // `CaptureResult::cursor_update`; per-connection consumers
    // forward it (or not) on their own `cursor_sync_event` DC.
    let request = CaptureRequest {
        cursor_mode: CursorCaptureMode::SyncNative,
    };
    // We grab `display_info` once at loop start and reuse it for every
    // frame's `SharedFrame::display_info`. Earlier code re-queried via
    // `capture.get_current_output()` on every tick "in case the user
    // resized the source display", but:
    //
    //   1. No downstream consumer reads `SharedFrame.display_info` —
    //      `media_producer` already snapshots `display_info` once at
    //      pipeline start via `SharedCaptureHandle::display_info()`.
    //   2. On Windows, `get_current_output` calls `EnumOutputs` +
    //      `EnumDisplayDevicesW` + `EnumDisplaySettingsW` per call,
    //      each emitting INFO logs from `desk-capture-engine`. At the
    //      OS refresh rate (60+ Hz) this floods the log file at
    //      ~20-25 enumerate-events/second per active capture, which
    //      is what the user observed.
    //
    // Per-frame `width`/`height`/`stride` continue to come from
    // `result.image`, so a runtime resolution change is still
    // reflected in the frame payload — only the unused
    // `display_info` re-query is removed.
    while !stop_flag.load(Ordering::Acquire) {
        let result = match capture.capture(request) {
            Ok(r) => r,
            Err(e) => {
                debug!(
                    "[SharedCapture:{}/{}] capture error: {e}; backing off 16ms",
                    key.backend, key.output_index
                );
                thread::sleep(Duration::from_millis(16));
                continue;
            }
        };

        // The OS-level capture buffer pointed to by `result.image`
        // becomes invalid on the next `capture()` call, so we copy
        // the BGRA payload into an `Arc`-able `Bytes`. This is the
        // single fan-out cost — every subscriber refcount-bumps
        // afterwards.
        let stride = result.image.get_stride();
        let width = result.image.get_width();
        let height = result.image.get_height();
        let image_type = result.image.get_type();
        let data = Bytes::copy_from_slice(result.image.get_data());

        let frame = Arc::new(SharedFrame {
            data,
            width,
            height,
            stride,
            image_type,
            dirty_rects: result.dirty_rects,
            content_changed: result.content_changed,
            cursor_update: result.cursor_update,
            display_info: initial_display_info.clone(),
        });

        // No subscribers right now is OK — the capture loop only
        // exits on stop_flag. (stop_flag is set when every handle
        // is dropped, so "no subscribers + flag still false" is a
        // brief transient between subscribe calls.)
        let _ = sender.send(frame);
    }
    info!(
        "[SharedCapture:{}/{}] capture loop exited",
        key.backend, key.output_index
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: registry construction is cheap and `live_count`
    /// starts at zero. Guards against an accidental `lazy_static`
    /// that would lazily spawn a capture thread on first read.
    #[test]
    fn empty_registry_has_no_live_captures() {
        let reg = SharedCaptureRegistry::new();
        assert_eq!(reg.live_count(), 0);
    }

    /// Stale-entry cleanup: a `Weak` whose `Arc` was dropped is
    /// removed by the next `subscribe()` for the same key, leaving
    /// `live_count()` at the truthful current count.
    #[test]
    fn stale_weak_is_pruned_on_resubscribe_attempt() {
        let reg = SharedCaptureRegistry::new();
        // Insert a synthetic stale Weak (no underlying Arc) directly,
        // bypassing `subscribe` so the test does not depend on the
        // OS having a real capture device available.
        let key = CaptureKey {
            backend: "DXGI".into(),
            output_index: 0u32,
        };
        {
            let mut g = reg.map.lock().unwrap();
            // Stand-in for a dropped inner — Weak::new() never
            // upgrades.
            g.insert(key.clone(), Weak::<SharedInner>::new());
        }
        assert_eq!(reg.live_count(), 0, "stale Weak must not count as live");
        reg.remove_stale_entry(&key);
        let g = reg.map.lock().unwrap();
        assert!(
            !g.contains_key(&key),
            "remove_stale_entry must drop expired slot"
        );
    }

    /// Key derivation must include both backend and output index so
    /// "DXGI display 0" and "GDI display 0" produce distinct keys
    /// (otherwise they would collide and the second subscriber would
    /// be handed the wrong backend's frames).
    #[test]
    fn key_derivation_separates_backend_and_output() {
        let mut s = DeskSettings::default();
        s.image_capture = Some("DXGI".into());
        s.video_device_index = 0;
        let k_dxgi_0 = key_for_settings(&s).unwrap();

        s.image_capture = Some("GDI".into());
        let k_gdi_0 = key_for_settings(&s).unwrap();
        assert_ne!(k_dxgi_0, k_gdi_0, "different backends must hash separately");

        s.image_capture = Some("DXGI".into());
        s.video_device_index = 1;
        let k_dxgi_1 = key_for_settings(&s).unwrap();
        assert_ne!(
            k_dxgi_0, k_dxgi_1,
            "different output indices must hash separately"
        );
    }

    /// Regression: `run_capture_loop` used to call
    /// `capture.get_current_output()` on every tick to refresh
    /// `SharedFrame.display_info` — but no downstream consumer reads
    /// that field, while on Windows DXGI each call enumerates display
    /// devices + display modes, each emitting INFO logs from
    /// `desk-capture-engine`. At the OS refresh rate this floods the
    /// log file (observed ~20-25 enumerate-events/s/capture in
    /// production).
    ///
    /// This test pins the contract via a counting mock: the loop must
    /// emit frames and reuse the initial `DisplayInfo` without ever
    /// re-querying the backend.
    #[tokio::test(flavor = "current_thread")]
    async fn capture_loop_does_not_query_display_info_per_frame() {
        use desk_capture_engine::model::image_capture::{
            CaptureRequest, CaptureResult, ImageCapture, ImageCaptureType, ImageInfo, ImageType,
        };
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        struct MockImage(Vec<u8>);
        impl ImageInfo for MockImage {
            fn get_type(&self) -> ImageType {
                ImageType::BGRA
            }
            fn get_data(&self) -> &[u8] {
                &self.0
            }
            fn get_width(&self) -> u32 {
                4
            }
            fn get_height(&self) -> u32 {
                4
            }
            fn get_stride(&self) -> u32 {
                16
            }
        }

        struct CountingCapture {
            cap_count: Arc<AtomicUsize>,
            gco_count: Arc<AtomicUsize>,
        }
        impl ImageCapture for CountingCapture {
            fn capture(
                &mut self,
                _r: CaptureRequest,
            ) -> Result<CaptureResult, CaptureError> {
                // Sleep so the broadcast ring (capacity 64 below) has no
                // chance of wrapping before the test finishes recv'ing
                // its 5 frames; this keeps the test deterministic on
                // any host without depending on scheduler latency.
                std::thread::sleep(Duration::from_millis(5));
                self.cap_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(CaptureResult {
                    image: Box::new(MockImage(vec![0u8; 64])),
                    cursor_update: None,
                    content_changed: true,
                    dirty_rects: None,
                })
            }
            fn get_capture_type(&self) -> ImageCaptureType {
                unreachable!("run_capture_loop must not call get_capture_type")
            }
            fn get_current_output(&self) -> Result<DisplayInfo, CaptureError> {
                self.gco_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(DisplayInfo::default())
            }
        }

        let cap_count = Arc::new(AtomicUsize::new(0));
        let gco_count = Arc::new(AtomicUsize::new(0));
        let mock = Box::new(CountingCapture {
            cap_count: Arc::clone(&cap_count),
            gco_count: Arc::clone(&gco_count),
        });

        let (sender, mut rx) = broadcast::channel::<Arc<SharedFrame>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let key = CaptureKey {
            backend: "TEST".into(),
            output_index: 0,
        };
        let initial = DisplayInfo {
            device_name: "test-display".into(),
            ..DisplayInfo::default()
        };

        let stop_for_thread = Arc::clone(&stop);
        let initial_for_thread = initial.clone();
        let key_for_thread = key.clone();
        let join = std::thread::spawn(move || {
            run_capture_loop(
                mock,
                sender,
                stop_for_thread,
                key_for_thread,
                initial_for_thread,
            );
        });

        for i in 0..5 {
            let frame = rx.recv().await.expect("recv frame");
            assert_eq!(
                frame.display_info.device_name, "test-display",
                "frame #{i} display_info must come from the loop's initial \
                 snapshot — re-querying per frame is what we removed"
            );
        }

        stop.store(true, Ordering::Release);
        tokio::task::spawn_blocking(move || join.join().expect("capture loop join"))
            .await
            .expect("spawn_blocking");

        let cap = cap_count.load(std::sync::atomic::Ordering::Relaxed);
        let gco = gco_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            cap >= 5,
            "capture must have been driven at least 5 times: got {cap}"
        );
        assert_eq!(
            gco, 0,
            "regression: run_capture_loop must NOT invoke get_current_output \
             per frame — each call enumerates display devices on Windows \
             DXGI and floods the log at the OS refresh rate (got {gco} \
             calls in 5 frames)"
        );
    }
}
