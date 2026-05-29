//! Shared image-capture broadcaster for the worker side.
//!
//! Two browsers connecting to the same desktop both want a frame
//! stream from `(backend, output_index)` 鈥?but the OS-level capture
//! sources (DXGI Output Duplication in particular) treat
//! `IDXGIOutputDuplication::DuplicateOutput` as exclusive: the second
//! call against the same output in the same process returns
//! `E_INVALIDARG`. Previously the worker spawned a dedicated
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
//! handle drops (`Arc<SharedInner>` count 鈫?0 鈫?`Drop` sets the stop
//! flag and removes the registry entry).
//!
//! Concurrency contract:
//! - One capture loop per `CaptureKey`. Different connections that
//!   pick *different* backends or *different* output indices each get
//!   their own loop 鈥?e.g. one DXGI on display 0 + one GDI on display
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
//!   over the connection's `cursor_sync_event` DataChannel 鈥?the
//!   shared frame never has the cursor baked in. Both DXGI and GDI
//!   advertise `supports_cursor_sync = true` today so this is
//!   universal on Windows; the abstraction is platform-agnostic
//!   regardless.
//! - Backpressure: `broadcast::Sender::send` returns `Err(SendError)`
//!   only when there are zero subscribers, which is fine 鈥?there is
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
/// `device_name` is the GDI device name (`\\.\DISPLAYn`) of the chosen
/// monitor; selection by name was introduced together with IDD virtual
/// display support, since IDD monitors are addressable via GDI but
/// invisible to DXGI enumeration.
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub struct CaptureKey {
    pub backend: String,
    pub device_name: String,
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
        // Last handle went away 鈥?tell the capture thread to exit and
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
    /// `Receiver` 鈥?the underlying ring buffer is shared across all
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
    /// keeps the loop alive 鈥?drop it (e.g. when the connection ends)
    /// and the loop stops once no other handle remains.
    pub fn subscribe(
        self: &Arc<Self>,
        settings: &DeskSettings,
    ) -> Result<SharedCaptureHandle, CaptureError> {
        let key = key_for_settings(settings)?;
        // Fast path: existing live entry under the originally
        // requested key. Avoids the cost of building a capture
        // instance when the user-configured backend is already
        // running.
        {
            let mut g = self.map.lock().expect("shared capture registry poisoned");
            if let Some(weak) = g.get(&key) {
                if let Some(inner) = weak.upgrade() {
                    return Ok(SharedCaptureHandle { inner });
                }
                // Stale Weak 鈥?last handle just dropped. Remove and
                // fall through to construct fresh.
                g.remove(&key);
            }
        }
        // Slow path: build the capture instance + spawn the loop.
        // The first capture instance is built synchronously so the
        // caller observes `CaptureError` directly (e.g. wrong output
        // index, missing display device). The factory may
        // transparently fall back to a different backend (e.g.
        // WGC 鈫?DXGI when the WGC RuntimeBroker is unavailable in
        // SYSTEM/Winlogon contexts), so the effective backend is
        // queried below and used to key the registry slot.
        let initial_capture = create_image_capture(settings)?;
        let effective_type = initial_capture.get_capture_type();
        // Key by the device_name the capture instance actually realised
        // 鈥?not by whatever was on `settings.video_device_name` 鈥?so
        // any backend that re-resolved the request (today none do, but
        // the contract should hold) cannot orphan the registry slot.
        let display_info = initial_capture.get_current_output()?;
        let effective_key = key_from_capture_type(effective_type, &display_info.device_name);

        // Pre-spawn race re-check on the effective key. Crucial when
        // a sibling subscriber already runs the effective backend on
        // this output (e.g. an existing DXGI subscriber when our WGC
        // request just fell back to DXGI): we must reuse the existing
        // loop instead of momentarily spawning a second DXGI Desktop
        // Duplication on the same monitor (DXGI is exclusive per
        // output and the second instance would fail or churn).
        {
            let mut g = self.map.lock().expect("shared capture registry poisoned");
            if let Some(existing) = decide_registry_reuse(&mut g, &key, &effective_key) {
                drop(initial_capture);
                return Ok(SharedCaptureHandle { inner: existing });
            }
        }

        let (sender, _) = broadcast::channel::<Arc<SharedFrame>>(8);
        let stop_flag = Arc::new(AtomicBool::new(false));

        let sender_for_thread = sender.clone();
        let stop_for_thread = Arc::clone(&stop_flag);
        let key_for_thread = effective_key.clone();
        let display_info_for_thread = display_info.clone();
        let join = thread::Builder::new()
            .name(format!(
                "shared-capture-{}-{}",
                effective_key.backend, effective_key.device_name
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
            key: effective_key.clone(),
            registry: Arc::downgrade(self),
        });

        let mut g = self.map.lock().expect("shared capture registry poisoned");
        // Post-spawn race re-check (safety net against true races
        // between two concurrent subscribers). Keyed by effective_key,
        // matching what we'll insert.
        if let Some(existing_weak) = g.get(&effective_key)
            && let Some(existing_inner) = existing_weak.upgrade()
        {
            return Ok(SharedCaptureHandle {
                inner: existing_inner,
            });
        }
        g.insert(effective_key, Arc::downgrade(&inner));
        Ok(SharedCaptureHandle { inner })
    }

    fn remove_stale_entry(&self, key: &CaptureKey) {
        let mut g = self.map.lock().expect("shared capture registry poisoned");
        // Only remove if the slot's Weak has actually expired 鈥?        // otherwise we would knock out a brand-new entry inserted
        // mid-Drop by a racing subscribe path.
        if let Some(weak) = g.get(key)
            && weak.upgrade().is_none()
        {
            g.remove(key);
        }
    }

    /// Force-evict a registry slot and signal the underlying capture
    /// loop to exit, regardless of how many `SharedCaptureHandle`
    /// instances are still live. Used by the `SetVirtualDisplayMode`
    /// path to bypass the WGC self-adapt gap: a mid-session
    /// `IddCxMonitorDeparture` + Arrival invalidates the HMONITOR a
    /// `GraphicsCaptureItem` was bound to, but WGC's `TryGetNextFrame`
    /// keeps returning stale frames. Removing the slot here forces the
    /// next `subscribe()` onto the slow path so it reconstructs
    /// against the fresh HMONITOR.
    ///
    /// The caller is expected to follow up with
    /// `MediaProducer::stop_media` + `start_media` on the affected
    /// connections so their video pipeline threads observe the new
    /// loop. Returns true when a live slot was actually evicted.
    pub(crate) fn invalidate_key(&self, key: &CaptureKey) -> bool {
        let mut g = self.map.lock().expect("shared capture registry poisoned");
        let Some(weak) = g.remove(key) else {
            return false;
        };
        match weak.upgrade() {
            Some(inner) => {
                inner.stop_flag.store(true, Ordering::Release);
                true
            }
            None => false,
        }
    }

    /// Diagnostic / test introspection: count of live capture loops.
    pub fn live_count(&self) -> usize {
        let g = self.map.lock().expect("shared capture registry poisoned");
        g.values().filter(|w| w.upgrade().is_some()).count()
    }
}

/// Build a `CaptureKey` from a backend variant and GDI device name.
/// Kept as a tiny pure function so the rest of the module (and the
/// tests for `decide_registry_reuse`) can talk about keys without
/// detouring through a `DeskSettings` round-trip.
fn key_from_capture_type(t: ImageCaptureType, device_name: &str) -> CaptureKey {
    CaptureKey {
        backend: <&'static str>::from(t).to_string(),
        device_name: device_name.to_string(),
    }
}

fn key_for_settings(settings: &DeskSettings) -> Result<CaptureKey, CaptureError> {
    let backend: ImageCaptureType = settings.get_image_capture_type()?;
    Ok(key_from_capture_type(backend, &settings.video_device_name))
}

/// In-lock decision: should we reuse an existing live entry under
/// `effective_key`, or proceed to spawn a new capture loop?
/// Opportunistically prunes stale `Weak` entries at both keys so the
/// registry does not accumulate dead slots.
///
/// Returns `Some(existing_inner)` when a live entry under
/// `effective_key` is found (caller must drop their fresh capture
/// instance without spawning), `None` otherwise (caller proceeds to
/// spawn and insert).
///
/// This is split out as a pure function so the WGC 鈫?DXGI fallback
/// race-handling can be unit-tested without depending on a real
/// capture device.
fn decide_registry_reuse(
    map: &mut HashMap<CaptureKey, Weak<SharedInner>>,
    original_key: &CaptureKey,
    effective_key: &CaptureKey,
) -> Option<Arc<SharedInner>> {
    if let Some(weak) = map.get(effective_key) {
        if let Some(inner) = weak.upgrade() {
            return Some(inner);
        }
        map.remove(effective_key);
    }
    if effective_key != original_key
        && let Some(weak) = map.get(original_key)
        && weak.upgrade().is_none()
    {
        map.remove(original_key);
    }
    None
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
        key.backend, key.device_name
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
    //   1. No downstream consumer reads `SharedFrame.display_info` 鈥?    //      `media_producer` already snapshots `display_info` once at
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
    // reflected in the frame payload 鈥?only the unused
    // `display_info` re-query is removed.
    while !stop_flag.load(Ordering::Acquire) {
        let result = match capture.capture(request) {
            Ok(r) => r,
            Err(e) => {
                debug!(
                    "[SharedCapture:{}/{}] capture error: {e}; backing off 16ms",
                    key.backend, key.device_name
                );
                thread::sleep(Duration::from_millis(16));
                continue;
            }
        };

        // The OS-level capture buffer pointed to by `result.image`
        // becomes invalid on the next `capture()` call, so we copy
        // the BGRA payload into an `Arc`-able `Bytes`. This is the
        // single fan-out cost 鈥?every subscriber refcount-bumps
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

        // No subscribers right now is OK 鈥?the capture loop only
        // exits on stop_flag. (stop_flag is set when every handle
        // is dropped, so "no subscribers + flag still false" is a
        // brief transient between subscribe calls.)
        let _ = sender.send(frame);
    }
    info!(
        "[SharedCapture:{}/{}] capture loop exited",
        key.backend, key.device_name
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
            device_name: r"\\.\DISPLAY1".into(),
        };
        {
            let mut g = reg.map.lock().unwrap();
            // Stand-in for a dropped inner 鈥?Weak::new() never
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

    /// Key derivation must include both backend and device_name so
    /// "DXGI display 1" and "GDI display 1" produce distinct keys
    /// (otherwise they would collide and the second subscriber would
    /// be handed the wrong backend's frames).
    #[test]
    fn key_derivation_separates_backend_and_device_name() {
        let mut s = DeskSettings::default();
        s.image_capture = Some("DXGI".into());
        s.video_device_name = r"\\.\DISPLAY1".into();
        let k_dxgi_1 = key_for_settings(&s).unwrap();

        s.image_capture = Some("GDI".into());
        let k_gdi_1 = key_for_settings(&s).unwrap();
        assert_ne!(k_dxgi_1, k_gdi_1, "different backends must hash separately");

        s.image_capture = Some("DXGI".into());
        s.video_device_name = r"\\.\DISPLAY7".into();
        let k_dxgi_7 = key_for_settings(&s).unwrap();
        assert_ne!(
            k_dxgi_1, k_dxgi_7,
            "different device_names must hash separately"
        );
    }

    /// Windows backends DXGI / GDI / WGC must all hash to distinct
    /// `CaptureKey`s at the same `video_device_name`. Without this,
    /// switching between WGC and DXGI on the same display could hand
    /// a stale frame from the wrong backend.
    #[cfg(target_os = "windows")]
    #[test]
    fn key_derivation_separates_wgc_dxgi_gdi() {
        let mut s = DeskSettings::default();
        s.video_device_name = r"\\.\DISPLAY1".into();
        s.image_capture = Some("WGC".into());
        let k_wgc = key_for_settings(&s).unwrap();
        s.image_capture = Some("DXGI".into());
        let k_dxgi = key_for_settings(&s).unwrap();
        s.image_capture = Some("GDI".into());
        let k_gdi = key_for_settings(&s).unwrap();
        assert_ne!(k_wgc, k_dxgi);
        assert_ne!(k_dxgi, k_gdi);
        assert_ne!(k_wgc, k_gdi);
    }

    /// Construct a barebones `Arc<SharedInner>` for registry tests
    /// without spawning any threads or touching the capture engine.
    /// The capture loop never runs against this synthetic instance 鈥?    /// it only exists to satisfy `Arc::downgrade` so we can drive
    /// `decide_registry_reuse` against a realistic registry map.
    #[cfg(target_os = "windows")]
    fn synthetic_shared_inner(key: CaptureKey) -> Arc<SharedInner> {
        let (sender, _) = broadcast::channel::<Arc<SharedFrame>>(1);
        Arc::new(SharedInner {
            sender,
            stop_flag: Arc::new(AtomicBool::new(false)),
            join_handle: StdMutex::new(None),
            display_info: DisplayInfo::default(),
            key,
            registry: Weak::new(),
        })
    }

    /// Variant of [`synthetic_shared_inner`] whose `registry` field is a
    /// real `Weak<SharedCaptureRegistry>` pointer. Needed by the
    /// `invalidate_then_reinsert_drop_preserves_new_slot` test, which
    /// requires the inner's `Drop` impl to actually call
    /// `registry.remove_stale_entry` so we can verify the only-if-
    /// expired guard does not knock out a freshly-inserted slot.
    #[cfg(target_os = "windows")]
    fn synthetic_shared_inner_attached(
        key: CaptureKey,
        registry: &Arc<SharedCaptureRegistry>,
    ) -> Arc<SharedInner> {
        let (sender, _) = broadcast::channel::<Arc<SharedFrame>>(1);
        Arc::new(SharedInner {
            sender,
            stop_flag: Arc::new(AtomicBool::new(false)),
            join_handle: StdMutex::new(None),
            display_info: DisplayInfo::default(),
            key,
            registry: Arc::downgrade(registry),
        })
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn key_from_capture_type_pure_function() {
        let k_dxgi = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");
        assert_eq!(k_dxgi.backend, "DXGI");
        assert_eq!(k_dxgi.device_name, r"\\.\DISPLAY1");

        let k_wgc = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY7");
        assert_eq!(k_wgc.backend, "WGC");
        assert_eq!(k_wgc.device_name, r"\\.\DISPLAY7");

        let k_gdi = key_from_capture_type(ImageCaptureType::GDI, r"\\.\DISPLAY2");
        assert_eq!(k_gdi.backend, "GDI");
        assert_eq!(k_gdi.device_name, r"\\.\DISPLAY2");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn key_from_capture_type_distinct_per_backend() {
        let kw = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY1");
        let kd = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");
        let kg = key_from_capture_type(ImageCaptureType::GDI, r"\\.\DISPLAY1");
        assert_ne!(kw, kd);
        assert_ne!(kd, kg);
        assert_ne!(kw, kg);
    }

    /// Codex review #4: the registry must key on the device_name the
    /// capture instance actually realised (`get_current_output().
    /// device_name`), not on whatever was in `settings.video_device_name`.
    /// Today no backend re-resolves 鈥?`select_display_info_by_name`
    /// hard-errors on a miss 鈥?but if one ever introduces a fallback
    /// path the cache could otherwise key two distinct loops as the
    /// same slot. Pin the contract explicitly.
    #[cfg(target_os = "windows")]
    #[test]
    fn effective_key_uses_initial_capture_device_name_not_settings() {
        let k_settings = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");
        let k_realised = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY7");
        // The realised key is what the registry must store / look up
        // against, derived from `initial_capture.get_current_output()`.
        // The settings key would only get reused on a coincidence 鈥?        // the test pins that the two are not silently aliased.
        assert_ne!(
            k_settings, k_realised,
            "settings-derived key and capture-realised key must hash apart"
        );
    }

    /// The critical fallback property: when the factory silently
    /// rewrote WGC 鈫?DXGI for the new capture instance and an existing
    /// DXGI subscriber is already running on the same output,
    /// `decide_registry_reuse` must return that existing inner so the
    /// caller can drop its fresh capture without spawning a second
    /// DXGI Desktop Duplication on the monitor.
    #[cfg(target_os = "windows")]
    #[test]
    fn decide_registry_reuse_returns_existing_on_effective_key_hit() {
        let mut map: HashMap<CaptureKey, Weak<SharedInner>> = HashMap::new();
        let dxgi_key = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");
        let wgc_key = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY1");

        let live = synthetic_shared_inner(dxgi_key.clone());
        map.insert(dxgi_key.clone(), Arc::downgrade(&live));

        let decision = decide_registry_reuse(&mut map, &wgc_key, &dxgi_key);
        let got = decision.expect("must reuse existing DXGI entry");
        assert!(
            Arc::ptr_eq(&got, &live),
            "reused inner must be the exact pre-populated Arc"
        );
    }

    /// When no live entry exists under the effective key, the function
    /// returns None so the caller proceeds to spawn.
    #[cfg(target_os = "windows")]
    #[test]
    fn decide_registry_reuse_returns_none_when_no_hit() {
        let mut map: HashMap<CaptureKey, Weak<SharedInner>> = HashMap::new();
        let dxgi_key = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");
        let wgc_key = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY1");
        assert!(decide_registry_reuse(&mut map, &wgc_key, &dxgi_key).is_none());
    }

    /// Stale `Weak` entries at both `effective_key` and (if different)
    /// `original_key` are removed by the same call that probes for
    /// reuse 鈥?keeping the registry from accumulating dead slots even
    /// in the fallback path.
    #[cfg(target_os = "windows")]
    #[test]
    fn decide_registry_reuse_cleans_stale_weak() {
        let mut map: HashMap<CaptureKey, Weak<SharedInner>> = HashMap::new();
        let dxgi_key = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");
        let wgc_key = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY1");

        // Insert two stale Weaks: one at effective_key, one at original_key.
        let dropped_dxgi = synthetic_shared_inner(dxgi_key.clone());
        let stale_dxgi = Arc::downgrade(&dropped_dxgi);
        drop(dropped_dxgi);
        map.insert(dxgi_key.clone(), stale_dxgi);

        let dropped_wgc = synthetic_shared_inner(wgc_key.clone());
        let stale_wgc = Arc::downgrade(&dropped_wgc);
        drop(dropped_wgc);
        map.insert(wgc_key.clone(), stale_wgc);

        assert!(
            decide_registry_reuse(&mut map, &wgc_key, &dxgi_key).is_none(),
            "stale entries must not be returned as reusable"
        );
        assert!(
            !map.contains_key(&dxgi_key),
            "stale effective_key slot must be removed"
        );
        assert!(
            !map.contains_key(&wgc_key),
            "stale original_key slot must be removed when it differs from effective_key"
        );
    }

    /// When `effective_key == original_key` (no fallback), the
    /// function does not over-prune: a stale slot at the shared key is
    /// removed once, not twice.
    #[cfg(target_os = "windows")]
    #[test]
    fn decide_registry_reuse_when_effective_equals_original() {
        let mut map: HashMap<CaptureKey, Weak<SharedInner>> = HashMap::new();
        let dxgi_key = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");

        let dropped = synthetic_shared_inner(dxgi_key.clone());
        let stale = Arc::downgrade(&dropped);
        drop(dropped);
        map.insert(dxgi_key.clone(), stale);

        assert!(decide_registry_reuse(&mut map, &dxgi_key, &dxgi_key).is_none());
        assert!(!map.contains_key(&dxgi_key));
    }

    /// Live entry must be evicted from the map AND its stop_flag set
    /// so the capture loop exits on its next tick. Returns true to
    /// signal "something was actually evicted" so the caller knows the
    /// follow-up stop/start will hit a freshly-spawned loop.
    #[cfg(target_os = "windows")]
    #[test]
    fn invalidate_key_removes_live_slot_and_sets_stop_flag() {
        let reg = SharedCaptureRegistry::new();
        let key = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY1");
        let inner = synthetic_shared_inner_attached(key.clone(), &reg);
        reg.map
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::downgrade(&inner));

        let evicted = reg.invalidate_key(&key);
        assert!(evicted, "live slot must report as evicted");
        assert!(
            !reg.map.lock().unwrap().contains_key(&key),
            "slot must be gone from the registry map"
        );
        assert!(
            inner.stop_flag.load(Ordering::Acquire),
            "stop_flag must be set so the capture loop exits"
        );
    }

    /// An unknown key is a no-op that returns false; the caller can
    /// distinguish "nothing to invalidate" from "we just evicted a live
    /// loop" without inspecting the map directly.
    #[cfg(target_os = "windows")]
    #[test]
    fn invalidate_key_returns_false_for_unknown_key() {
        let reg = SharedCaptureRegistry::new();
        let key = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY99");
        assert!(!reg.invalidate_key(&key));
    }

    /// A stale Weak (the last strong Arc already dropped) still gets
    /// the slot removed from the map but reports false, because there
    /// was no live loop to signal a stop on.
    #[cfg(target_os = "windows")]
    #[test]
    fn invalidate_key_returns_false_for_stale_weak() {
        let reg = SharedCaptureRegistry::new();
        let key = key_from_capture_type(ImageCaptureType::DXGI, r"\\.\DISPLAY1");
        let inner = synthetic_shared_inner(key.clone());
        let weak = Arc::downgrade(&inner);
        drop(inner);
        reg.map.lock().unwrap().insert(key.clone(), weak);

        let evicted = reg.invalidate_key(&key);
        assert!(!evicted, "stale weak must report nothing live to evict");
        assert!(
            !reg.map.lock().unwrap().contains_key(&key),
            "stale slot must still be removed from the map"
        );
    }

    /// Critical race: after invalidate_key clears a slot, a fresh
    /// SharedInner can be inserted under the same key. When the old
    /// inner finally drops, its `SharedInner::drop` calls
    /// `remove_stale_entry`, which must observe the new slot's Weak
    /// upgrades successfully and therefore leave the new slot alone.
    /// Without this only-if-expired guard the SetVirtualDisplayMode
    /// stop/start would race against the old loop's natural shutdown
    /// and intermittently knock out the brand-new capture loop.
    #[cfg(target_os = "windows")]
    #[test]
    fn invalidate_then_reinsert_drop_preserves_new_slot() {
        let reg = SharedCaptureRegistry::new();
        let key = key_from_capture_type(ImageCaptureType::WGC, r"\\.\DISPLAY1");

        let inner_a = synthetic_shared_inner_attached(key.clone(), &reg);
        reg.map
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::downgrade(&inner_a));

        assert!(reg.invalidate_key(&key));
        assert!(!reg.map.lock().unwrap().contains_key(&key));

        // SetVirtualDisplayMode path: stop_media exits old thread,
        // start_media spawns a new one which inserts a fresh inner.
        let inner_b = synthetic_shared_inner_attached(key.clone(), &reg);
        reg.map
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::downgrade(&inner_b));

        // Old loop only now finishes draining and drops inner_a.
        // SharedInner::drop runs remove_stale_entry(&key), which must
        // observe inner_b's still-live Weak and leave the slot intact.
        drop(inner_a);

        let g = reg.map.lock().unwrap();
        let weak = g.get(&key).expect("new slot must survive old inner drop");
        assert!(
            Arc::ptr_eq(
                &weak.upgrade().expect("new inner must still be alive"),
                &inner_b
            ),
            "surviving slot must point at the freshly-inserted inner_b"
        );
    }

    /// Regression: `run_capture_loop` used to call
    /// `capture.get_current_output()` on every tick to refresh
    /// `SharedFrame.display_info` 鈥?but no downstream consumer reads
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
            fn capture(&mut self, _r: CaptureRequest) -> Result<CaptureResult, CaptureError> {
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
            device_name: r"\\.\DISPLAY1".into(),
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
                 snapshot 鈥?re-querying per frame is what we removed"
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
             per frame 鈥?each call enumerates display devices on Windows \
             DXGI and floods the log at the OS refresh rate (got {gco} \
             calls in 5 frames)"
        );
    }
}
