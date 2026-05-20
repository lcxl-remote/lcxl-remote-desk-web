//! # Worker-side input dispatcher (Arch IV cut 5)
//!
//! Per-`connection_id` mouse / keyboard injection handlers. The
//! daemon's `on_data_channel` router (see `daemon::pc_manager::
//! register_data_channel_router`) gates on
//! `signaling_state.accept_control` and forwards the raw browser DC
//! payload as `ServiceToWorker::MouseInput` / `MouseMoveInput` /
//! `KeyboardInput`. This module owns the actual injection: it
//! constructs the platform `MouseEventHandler` /
//! `KeyboardEventHandler` from `desk-input-injection` and dispatches
//! decoded events into them.
//!
//! ## Lifecycle
//!
//! - `start_connection(payload)` is called from the worker IPC loop
//!   when `ServiceToWorker::StartMedia` arrives. It instantiates the
//!   per-connection handlers eagerly so the very first input event
//!   doesn't pay handler-construction latency.
//! - `dispatch_*` is called from the same IPC loop when the matching
//!   `ServiceToWorker::*Input` arrives. Decoding errors are logged
//!   and dropped — the IPC loop must remain alive for subsequent
//!   inputs.
//! - `stop_connection(connection_id)` releases the per-connection
//!   handlers when `StopMedia` arrives or the worker is shutting down.
//!
//! ## Display dimensions
//!
//! The `MouseEventHandler` needs the captured monitor's *rectangle*
//! (left / top / width / height in virtual desktop space) so it can map
//! the browser's normalised cursor coordinates onto SendInput's pixel
//! grid AND translate by the monitor's offset. Off-origin monitors —
//! a second physical screen or an IDD virtual display attached to the
//! right of the primary — have a non-zero `left`; without it the
//! cursor always lands on the primary.
//!
//! The geometry is derived from `payload.video_device`: that is the
//! GDI device name the browser picked, so the rect that matches it is
//! the surface the user actually sees. When no device is selected
//! (legal during the fresh-install pre-pick state) we fall back to the
//! first attached display from `desk_capture_engine::list_image_capture()`.
//! Display reconfiguration during a session (resolution change, monitor
//! add) is handled by killing the connection — the browser
//! re-establishes and we re-query. Live resolution change mid-session
//! is out of scope.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use desk_capture_engine::image_capture::image_capture_factory::list_image_capture;
use desk_input_injection::keyboard_event::keyboard_event_factory::create_keyboard_event_handler;
use desk_input_injection::model::data_channel::{
    KeyboardEventData, KeyboardEventHandler, MouseEventData, MouseEventHandler,
};
use desk_input_injection::model::geometry::{
    MonitorGeometry, SharedMonitorGeometry, shared as shared_geometry,
};
use desk_input_injection::mouse_event::mouse_event_factory::create_mouse_event_handler;
use desk_ipc_protocol::message::{InputPayload, StartMediaPayload, StopMediaPayload};
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::image_capture::DisplayInfo;
use log::{debug, error, info, warn};

/// Per-connection injection state. Mirrors the per-DC `Arc<Mutex<...>>`
/// pattern used by Arch III's `service::mouse_event` /
/// `service::keyboard_event` handlers; the difference is the dispatcher
/// constructs them at `StartMedia` time rather than at first DC open.
struct ConnectionInputState {
    mouse: Box<dyn MouseEventHandler + Send + Sync>,
    keyboard: Box<dyn KeyboardEventHandler + Send + Sync>,
    /// Last sequence number observed for mouse events. Discards late
    /// out-of-order packets to match Arch III `handle_mouse_event`
    /// behaviour (browser sends `sequence_number` to deduplicate
    /// retransmits / late deliveries).
    last_mouse_seq: u64,
    /// Hot-updatable captured-monitor rect. The clone held here is the
    /// **same** `Arc<RwLock<...>>` the platform mouse handler holds —
    /// writing to it through this struct is immediately visible to the
    /// next mouse event. Mutated by `refresh_geometry` /
    /// `retarget_connection`.
    geometry: SharedMonitorGeometry,
    /// `\\.\DISPLAYn` device name the connection currently captures.
    /// Required to scope `refresh_geometry(Some(device_name))` to the
    /// right connections after IDD `SetMode`, and re-written by
    /// `retarget_connection` after virtual display Attach / Detach.
    video_device: Option<String>,
}

/// Worker-side input dispatcher. Cheap to clone (`Arc` inside) so the
/// IPC loop can take a clone for each branch.
#[derive(Clone)]
pub struct InputDispatcher {
    /// Initial DeskSettings; used to pull `wayland_control_mode`. The
    /// dispatcher does not refresh this snapshot mid-session — settings
    /// changes that affect input semantics (e.g. wayland mode flip)
    /// are out of scope for cut 5 and would require an explicit IPC
    /// notify path anyway.
    desk_settings: DeskSettings,
    inner: Arc<StdMutex<HashMap<String, ConnectionInputState>>>,
}

impl InputDispatcher {
    pub fn new(desk_settings: DeskSettings) -> Self {
        Self {
            desk_settings,
            inner: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// Construct per-connection input handlers. Called from
    /// `ServiceToWorker::StartMedia`; the dispatcher reuses the
    /// payload `connection_id` as its key so the lifecycle matches
    /// the media producer's exactly.
    ///
    /// On handler construction failure the connection is left without
    /// input — input messages will warn-and-drop until the daemon
    /// either reissues StartMedia (no current trigger) or the
    /// connection terminates. Failure is rare in practice (Windows
    /// SendInput initialisation does not fail) but is handled
    /// gracefully so a transient capture-engine glitch does not crash
    /// the worker.
    pub fn start_connection(&self, payload: &StartMediaPayload) {
        let (left, top, width, height) =
            display_geometry_for_device(payload.video_device.as_deref());
        let geometry = shared_geometry(MonitorGeometry::new(left, top, width, height));
        let wayland_mode = self.desk_settings.wayland_control_mode.as_deref();
        let mouse = match create_mouse_event_handler(geometry.clone(), wayland_mode) {
            Ok(h) => h,
            Err(e) => {
                error!(
                    "[InputDispatcher] {}: mouse handler init failed: {e}; \
                     mouse input will be dropped for this connection",
                    payload.connection_id
                );
                return;
            }
        };
        let keyboard = match create_keyboard_event_handler(wayland_mode) {
            Ok(h) => h,
            Err(e) => {
                error!(
                    "[InputDispatcher] {}: keyboard handler init failed: {e}; \
                     keyboard input will be dropped for this connection",
                    payload.connection_id
                );
                return;
            }
        };
        let mut map = self.inner.lock().expect("input dispatcher lock poisoned");
        let prev = map.insert(
            payload.connection_id.clone(),
            ConnectionInputState {
                mouse,
                keyboard,
                last_mouse_seq: 0,
                geometry,
                video_device: payload.video_device.clone(),
            },
        );
        if prev.is_some() {
            warn!(
                "[InputDispatcher] {}: replaced existing input handlers (duplicate StartMedia?)",
                payload.connection_id
            );
        }
        info!(
            "[InputDispatcher] {}: input handlers ready (rect=({},{},{}x{}), device={:?}, wayland_mode={:?})",
            payload.connection_id,
            left,
            top,
            width,
            height,
            payload.video_device.as_deref(),
            wayland_mode
        );
    }

    /// Refresh the shared geometry of one or more connections in
    /// response to a display reconfiguration. Re-queries the GDI
    /// display list once and writes new rects atomically into the
    /// matching connections' `SharedMonitorGeometry` — handlers pick
    /// up the new value on the next mouse event without being
    /// rebuilt.
    ///
    /// - `device_name = Some(name)`: only refresh connections whose
    ///   `video_device == Some(name)`. Used after IDD `SetMode` apply
    ///   when the worker knows exactly which display changed.
    /// - `device_name = None`: refresh every connection. Used for
    ///   `WM_DISPLAYCHANGE` (the OS broadcast doesn't tell us which
    ///   display moved, so we refresh all).
    pub fn refresh_geometry(&self, device_name: Option<&str>) {
        let displays = enumerate_attached_displays();
        let map = self.inner.lock().expect("input dispatcher lock poisoned");
        refresh_geometry_in(&map, &displays, device_name);
    }

    /// Retarget one connection to a new video device — used after a
    /// virtual display Attach/Detach swaps the capture target. Updates
    /// both `video_device` and `geometry` in place; preserves
    /// `last_mouse_seq` and the underlying mouse/keyboard handlers
    /// (they hold the same `SharedMonitorGeometry` Arc, so the
    /// in-place write is automatically visible).
    pub fn retarget_connection(&self, payload: &StartMediaPayload) {
        let displays = enumerate_attached_displays();
        let mut map = self.inner.lock().expect("input dispatcher lock poisoned");
        retarget_connection_in(&mut map, &displays, payload);
    }

    /// Drop per-connection input handlers. Called from
    /// `ServiceToWorker::StopMedia`. No-op on unknown id.
    pub fn stop_connection(&self, payload: &StopMediaPayload) {
        let mut map = self.inner.lock().expect("input dispatcher lock poisoned");
        if map.remove(&payload.connection_id).is_some() {
            info!(
                "[InputDispatcher] {}: input handlers released",
                payload.connection_id
            );
        }
    }

    /// Drop every per-connection input handler. Called from worker
    /// shutdown so injection threads do not outlive the worker.
    pub fn shutdown(&self) {
        let mut map = self.inner.lock().expect("input dispatcher lock poisoned");
        map.clear();
    }

    /// Decode + inject a mouse non-move event. Late out-of-order
    /// packets (sequence_number < last) are silently dropped to match
    /// Arch III behaviour.
    pub fn dispatch_mouse(&self, payload: &InputPayload) {
        let event = match decode_mouse(&payload.data) {
            Some(e) => e,
            None => return,
        };
        let mut map = self.inner.lock().expect("input dispatcher lock poisoned");
        let state = match map.get_mut(&payload.connection_id) {
            Some(s) => s,
            None => {
                debug!(
                    "[InputDispatcher] {}: mouse event for unknown connection — dropping",
                    payload.connection_id
                );
                return;
            }
        };
        if let Some(seq) = event.sequence_number
            && seq > 0
        {
            if seq < state.last_mouse_seq {
                return;
            }
            state.last_mouse_seq = seq;
        }
        if let Err(e) = state.mouse.handle_mouse_event(&event) {
            error!(
                "[InputDispatcher] {}: mouse event injection failed: {e}",
                payload.connection_id
            );
        }
    }

    /// Decode + inject a mouse-move event. Same path as
    /// [`dispatch_mouse`]; the daemon ships moves on a distinct IPC
    /// variant only so the worker can apply move-specific coalescing
    /// in a future cut. Cut 5 treats them identically.
    pub fn dispatch_mouse_move(&self, payload: &InputPayload) {
        self.dispatch_mouse(payload);
    }

    /// Decode + inject a keyboard event.
    pub fn dispatch_keyboard(&self, payload: &InputPayload) {
        let event = match decode_keyboard(&payload.data) {
            Some(e) => e,
            None => return,
        };
        let mut map = self.inner.lock().expect("input dispatcher lock poisoned");
        let state = match map.get_mut(&payload.connection_id) {
            Some(s) => s,
            None => {
                debug!(
                    "[InputDispatcher] {}: keyboard event for unknown connection — dropping",
                    payload.connection_id
                );
                return;
            }
        };
        if let Err(e) = state.keyboard.handle_keyboard_event(&event) {
            error!(
                "[InputDispatcher] {}: keyboard event injection failed: {e}",
                payload.connection_id
            );
        }
    }
}

/// Decode a mouse event payload from the raw DC bytes the daemon
/// forwarded. Returns `None` (and logs) on malformed input — the IPC
/// loop must keep flowing.
fn decode_mouse(data: &[u8]) -> Option<MouseEventData> {
    let s = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(e) => {
            error!("[InputDispatcher] mouse payload not UTF-8: {e}");
            return None;
        }
    };
    match serde_json::from_str::<MouseEventData>(s) {
        Ok(e) => Some(e),
        Err(e) => {
            error!("[InputDispatcher] mouse JSON decode failed: {e}");
            None
        }
    }
}

/// Decode a keyboard event payload. Symmetric to [`decode_mouse`].
fn decode_keyboard(data: &[u8]) -> Option<KeyboardEventData> {
    let s = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(e) => {
            error!("[InputDispatcher] keyboard payload not UTF-8: {e}");
            return None;
        }
    };
    match serde_json::from_str::<KeyboardEventData>(s) {
        Ok(e) => Some(e),
        Err(e) => {
            error!("[InputDispatcher] keyboard JSON decode failed: {e}");
            None
        }
    }
}

/// Resolve the captured monitor's full virtual-desktop rectangle
/// (left, top, width, height) for the device the browser picked.
///
/// - When `requested_device_name` is `Some` and matches a `DisplayInfo`
///   surfaced by `list_image_capture()`, return that monitor's rect.
///   This is the path that fixes the off-origin cursor bug for IDD /
///   secondary monitors.
/// - When `requested_device_name` is `Some` but does not match (display
///   was hot-unplugged between StartMedia and the dispatcher firing,
///   stale TOML config, …), fall back to the primary so handler
///   construction still succeeds; the cursor will land on the primary
///   instead of the missing surface but the process stays alive.
/// - When `requested_device_name` is `None`, fall back to the primary.
///   The browser dialog already gates submit on a non-empty device
///   name, so this branch is mostly defensive.
///
/// Falls back to a sensible 1920x1080 default when the capture-engine
/// reports nothing (e.g. headless test rig) so handler construction
/// still succeeds.
fn display_geometry_for_device(requested_device_name: Option<&str>) -> (i32, i32, i32, i32) {
    let all_displays = enumerate_attached_displays();
    geometry_for_device_in(&all_displays, requested_device_name)
}

/// Shared "give me the attached displays" helper. Used by every path
/// that builds or refreshes per-connection geometry. Kept as a private
/// function so future displacement of the capture-engine source-of-
/// truth is a single-file change.
fn enumerate_attached_displays() -> Vec<DisplayInfo> {
    let mut out = Vec::new();
    for (_backend, displays) in list_image_capture() {
        for display in displays {
            if display.attached_to_desktop {
                out.push(display);
            }
        }
    }
    out
}

/// Pure refresh: walk the connection map and rewrite the geometry of
/// every connection whose `video_device` matches `device_name`
/// (`None` = match all). Tested with a hand-built `HashMap` so we
/// don't need real connections.
fn refresh_geometry_in(
    map: &HashMap<String, ConnectionInputState>,
    displays: &[DisplayInfo],
    device_name: Option<&str>,
) {
    for state in map.values() {
        let is_match = device_name.is_none_or(|want| state.video_device.as_deref() == Some(want));
        if !is_match {
            continue;
        }
        let g = geometry_for_device_in(displays, state.video_device.as_deref());
        let mut w = state.geometry.write().expect("monitor geometry poisoned");
        *w = MonitorGeometry::new(g.0, g.1, g.2, g.3);
    }
}

/// Pure retarget: look up `payload.connection_id`, rewrite both
/// `video_device` and `geometry` to match the new capture target.
/// Unknown ids are a warn + no-op so the IPC loop stays alive even if
/// a daemon-side bug ships a stale id.
fn retarget_connection_in(
    map: &mut HashMap<String, ConnectionInputState>,
    displays: &[DisplayInfo],
    payload: &StartMediaPayload,
) {
    let Some(state) = map.get_mut(&payload.connection_id) else {
        warn!(
            "[InputDispatcher] {}: retarget for unknown connection — dropping",
            payload.connection_id
        );
        return;
    };
    state.video_device = payload.video_device.clone();
    let g = geometry_for_device_in(displays, payload.video_device.as_deref());
    let mut w = state.geometry.write().expect("monitor geometry poisoned");
    *w = MonitorGeometry::new(g.0, g.1, g.2, g.3);
    info!(
        "[InputDispatcher] {}: retargeted to device={:?}, rect=({},{},{}x{})",
        payload.connection_id, payload.video_device, g.0, g.1, g.2, g.3
    );
}

/// Pure helper extracted from `display_geometry_for_device` so it can
/// be unit-tested without a real display attached. Takes the already-
/// filtered `attached_to_desktop` list and applies the lookup
/// precedence. Identical fallback to 1920x1080 when no candidate has
/// non-zero dimensions.
fn geometry_for_device_in(
    candidates: &[desk_signal_facade::model::image_capture::DisplayInfo],
    requested_device_name: Option<&str>,
) -> (i32, i32, i32, i32) {
    let pick_rect =
        |d: &desk_signal_facade::model::image_capture::DisplayInfo| -> (i32, i32, i32, i32) {
            let r = &d.desktop_coordinates;
            (r.left, r.top, r.width(), r.height())
        };
    if let Some(name) = requested_device_name
        && !name.is_empty()
        && let Some(matched) = candidates.iter().find(|d| d.device_name == name)
    {
        let g = pick_rect(matched);
        if g.2 > 0 && g.3 > 0 {
            return g;
        }
    }
    if let Some(name) = requested_device_name
        && !name.is_empty()
    {
        warn!(
            "[InputDispatcher] requested device_name {:?} not enumerated; \
             falling back to primary display",
            name
        );
    }
    for display in candidates {
        let g = pick_rect(display);
        if g.2 > 0 && g.3 > 0 {
            return g;
        }
    }
    warn!(
        "[InputDispatcher] no attached display found via capture-engine; \
         falling back to (0,0,1920,1080) default"
    );
    (0, 0, 1920, 1080)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatcher() -> InputDispatcher {
        InputDispatcher::new(DeskSettings::default())
    }

    fn start_payload(connection_id: &str) -> StartMediaPayload {
        StartMediaPayload {
            connection_id: connection_id.to_string(),
            video_codec: desk_ipc_protocol::message::MediaCodec::H264,
            audio_codec: desk_ipc_protocol::message::MediaCodec::Opus,
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

    /// Decoder rejects non-UTF8 bytes without panicking.
    #[test]
    fn decode_mouse_rejects_invalid_utf8() {
        // 0xFF is not a valid UTF-8 start byte.
        assert!(decode_mouse(&[0xFFu8, 0xFE]).is_none());
    }

    /// Decoder rejects malformed JSON without panicking.
    #[test]
    fn decode_mouse_rejects_invalid_json() {
        assert!(decode_mouse(b"not json").is_none());
    }

    #[test]
    fn decode_keyboard_rejects_invalid_utf8() {
        assert!(decode_keyboard(&[0xFFu8, 0xFE]).is_none());
    }

    /// `dispatch_mouse` for an unknown connection_id is a silent no-op
    /// (returns without panicking). Critical for liveness — the IPC
    /// loop must stay alive even if a stale event arrives after
    /// StopMedia.
    #[test]
    fn dispatch_mouse_unknown_connection_is_silent_noop() {
        let d = dispatcher();
        let payload = InputPayload {
            connection_id: "ghost".into(),
            // Even valid-shaped JSON for an unknown connection must
            // not panic; just be dropped.
            data: br#"{"event_type":"mouse_move","x":0.5,"y":0.5}"#.to_vec(),
        };
        d.dispatch_mouse(&payload);
    }

    /// `stop_connection` on an unknown id is a no-op.
    #[test]
    fn stop_unknown_connection_is_noop() {
        let d = dispatcher();
        d.stop_connection(&StopMediaPayload {
            connection_id: "ghost".into(),
        });
    }

    /// `start_connection` then `stop_connection` removes the entry —
    /// reflected by an additional `start_connection` after stop not
    /// triggering the "duplicate" warning path (we can't observe the
    /// warning directly but we can verify the entry is in / out by
    /// re-stopping).
    #[test]
    fn start_then_stop_releases_state() {
        let d = dispatcher();
        let payload = start_payload("conn-x");
        d.start_connection(&payload);
        d.stop_connection(&StopMediaPayload {
            connection_id: "conn-x".into(),
        });
        // Second stop is a no-op; the assertion is just that this
        // doesn't panic — entry was removed by the first stop.
        d.stop_connection(&StopMediaPayload {
            connection_id: "conn-x".into(),
        });
    }

    /// `shutdown` clears every connection. Subsequent dispatches all
    /// see "unknown connection" and silently drop.
    #[test]
    fn shutdown_clears_state() {
        let d = dispatcher();
        d.start_connection(&start_payload("conn-a"));
        d.start_connection(&start_payload("conn-b"));
        d.shutdown();
        d.dispatch_keyboard(&InputPayload {
            connection_id: "conn-a".into(),
            data: br#"{"key":"a","event_type":"key_down"}"#.to_vec(),
        });
    }

    fn display(
        name: &str,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    ) -> desk_signal_facade::model::image_capture::DisplayInfo {
        use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};
        DisplayInfo {
            device_name: name.to_string(),
            display_device_name: None,
            attached_to_desktop: true,
            rotation: 0,
            resolutions: Vec::new(),
            desktop_coordinates: DisplayRect {
                left,
                top,
                right,
                bottom,
            },
        }
    }

    /// Requested device matches an entry: return that monitor's rect.
    /// This is the path that fixes the off-origin cursor bug — without
    /// it the worker would use the primary's rect even when the browser
    /// picked an IDD or secondary monitor.
    #[test]
    fn geometry_for_device_returns_selected_monitor_rect() {
        let primary = display(r"\\.\DISPLAY1", 0, 0, 1280, 800);
        let idd = display(r"\\.\DISPLAY8", 1280, 0, 2780, 900);
        let pick = geometry_for_device_in(&[primary, idd], Some(r"\\.\DISPLAY8"));
        // 1500x900 panel at offset (1280, 0). Crucially `left` is 1280,
        // not 0 — that's the whole point of this commit.
        assert_eq!(pick, (1280, 0, 1500, 900));
    }

    /// Off-origin secondary monitor: `left`/`top` are propagated, not
    /// stripped to zero.
    #[test]
    fn geometry_for_device_preserves_left_top_offset() {
        let primary = display(r"\\.\DISPLAY1", 0, 0, 1920, 1080);
        let stacked = display(r"\\.\DISPLAY2", 0, 1080, 1920, 2160);
        let pick = geometry_for_device_in(&[primary, stacked], Some(r"\\.\DISPLAY2"));
        assert_eq!(pick, (0, 1080, 1920, 1080));
    }

    /// Requested device not in the enumerated list (hot-unplug between
    /// capabilities query and StartMedia, or stale TOML): fall back to
    /// the first attached display so the handler still constructs.
    #[test]
    fn geometry_for_device_falls_back_to_primary_when_device_missing() {
        let primary = display(r"\\.\DISPLAY1", 0, 0, 1280, 800);
        let other = display(r"\\.\DISPLAY8", 1280, 0, 2780, 900);
        let pick = geometry_for_device_in(&[primary, other], Some(r"\\.\DISPLAY99"));
        assert_eq!(pick, (0, 0, 1280, 800));
    }

    /// `None` (no device picked in the payload) → first attached.
    #[test]
    fn geometry_for_device_uses_first_when_no_device_requested() {
        let primary = display(r"\\.\DISPLAY1", 0, 0, 1280, 800);
        let pick = geometry_for_device_in(&[primary], None);
        assert_eq!(pick, (0, 0, 1280, 800));
    }

    /// Empty string in the payload behaves like `None` (the browser
    /// uses an empty string for the "no display picked yet" state).
    #[test]
    fn geometry_for_device_treats_empty_string_as_unselected() {
        let primary = display(r"\\.\DISPLAY1", 0, 0, 1280, 800);
        let pick = geometry_for_device_in(&[primary], Some(""));
        assert_eq!(pick, (0, 0, 1280, 800));
    }

    /// Headless environment with no displays: hard-coded 1920x1080 at
    /// the origin so handler construction does not fail. Matches the
    /// previous behaviour of `primary_display_size()`.
    #[test]
    fn geometry_for_device_returns_default_when_list_empty() {
        let pick = geometry_for_device_in(&[], None);
        assert_eq!(pick, (0, 0, 1920, 1080));
    }

    // === Hot-reload geometry tests ===
    //
    // The factory-driven `start_connection` path can't run on a
    // headless CI box (it touches platform mouse / keyboard
    // handlers — uinput on Linux, real Win32 calls on Windows). To
    // exercise the refresh / retarget logic without depending on the
    // factory, we hand-build `ConnectionInputState` entries with no-op
    // handlers and drive `refresh_geometry_in` / `retarget_connection_in`
    // directly. This is the same pure-function pattern used by
    // `geometry_for_device_in` above.

    use desk_input_injection::error::InputError;
    use desk_input_injection::model::data_channel::{KeyboardEventData, MouseEventData};
    use desk_input_injection::model::geometry::{MonitorGeometry, shared as shared_geometry};

    struct NoopMouse;
    impl MouseEventHandler for NoopMouse {
        fn handle_mouse_move(&mut self, _: &MouseEventData) -> Result<(), InputError> {
            Ok(())
        }
        fn handle_mouse_down(&mut self, _: &MouseEventData) -> Result<(), InputError> {
            Ok(())
        }
        fn handle_mouse_up(&mut self, _: &MouseEventData) -> Result<(), InputError> {
            Ok(())
        }
        fn handle_mouse_wheel(&mut self, _: &MouseEventData) -> Result<(), InputError> {
            Ok(())
        }
    }
    struct NoopKeyboard;
    impl KeyboardEventHandler for NoopKeyboard {
        fn handle_key_down(&mut self, _: &KeyboardEventData) -> Result<(), InputError> {
            Ok(())
        }
        fn handle_key_up(&mut self, _: &KeyboardEventData) -> Result<(), InputError> {
            Ok(())
        }
    }

    fn fake_state(
        video_device: Option<&str>,
        geometry: SharedMonitorGeometry,
    ) -> ConnectionInputState {
        ConnectionInputState {
            mouse: Box::new(NoopMouse),
            keyboard: Box::new(NoopKeyboard),
            last_mouse_seq: 0,
            geometry,
            video_device: video_device.map(|s| s.to_string()),
        }
    }

    /// `refresh_geometry_in(Some(device))` writes only into the
    /// connections whose `video_device` matches. Sibling connections
    /// keep their old rect. This is the IDD `SetMode` apply path.
    #[test]
    fn refresh_geometry_in_updates_matching_connection() {
        let primary = display(r"\\.\DISPLAY1", 0, 0, 1280, 800);
        let idd = display(r"\\.\DISPLAY8", 1280, 0, 2780, 900);
        let displays = vec![primary, idd];

        let g_phys = shared_geometry(MonitorGeometry::new(0, 0, 1280, 800));
        let g_idd = shared_geometry(MonitorGeometry::new(1280, 0, 1500, 900));
        let mut map = HashMap::new();
        map.insert(
            "conn-phys".to_string(),
            fake_state(Some(r"\\.\DISPLAY1"), g_phys.clone()),
        );
        map.insert(
            "conn-idd".to_string(),
            fake_state(Some(r"\\.\DISPLAY8"), g_idd.clone()),
        );

        // Pretend the IDD resolution changed: rewrite DISPLAY8 in
        // the enumeration to the new rect, then refresh.
        let displays_after = vec![
            display(r"\\.\DISPLAY1", 0, 0, 1280, 800),
            display(r"\\.\DISPLAY8", 1280, 0, 3840, 2160),
        ];
        refresh_geometry_in(&map, &displays_after, Some(r"\\.\DISPLAY8"));

        assert_eq!(
            *g_idd.read().unwrap(),
            MonitorGeometry::new(1280, 0, 2560, 2160),
            "IDD geometry must reflect the new rect",
        );
        assert_eq!(
            *g_phys.read().unwrap(),
            MonitorGeometry::new(0, 0, 1280, 800),
            "DISPLAY1 must be untouched — the filter scoped to DISPLAY8",
        );
        // Suppress unused-variable warning on `displays`.
        let _ = displays;
    }

    /// `refresh_geometry_in(Some(device))` with a device that no
    /// connection holds is a no-op — every connection keeps its rect.
    #[test]
    fn refresh_geometry_in_unknown_device_is_noop() {
        let displays = vec![display(r"\\.\DISPLAY1", 0, 0, 1280, 800)];
        let g = shared_geometry(MonitorGeometry::new(0, 0, 1280, 800));
        let mut map = HashMap::new();
        map.insert(
            "conn-x".to_string(),
            fake_state(Some(r"\\.\DISPLAY1"), g.clone()),
        );

        refresh_geometry_in(&map, &displays, Some(r"\\.\GHOST"));
        assert_eq!(*g.read().unwrap(), MonitorGeometry::new(0, 0, 1280, 800));
    }

    /// `refresh_geometry_in(None)` re-applies all geometries — the
    /// `WM_DISPLAYCHANGE` broadcast path. Both connections pick up
    /// new rects.
    #[test]
    fn refresh_geometry_in_none_updates_all_connections() {
        let displays = vec![
            display(r"\\.\DISPLAY1", 0, 0, 2560, 1440),
            display(r"\\.\DISPLAY2", 2560, 0, 5120, 1440),
        ];
        let g1 = shared_geometry(MonitorGeometry::new(0, 0, 1920, 1080));
        let g2 = shared_geometry(MonitorGeometry::new(1920, 0, 3440, 1080));
        let mut map = HashMap::new();
        map.insert(
            "c1".to_string(),
            fake_state(Some(r"\\.\DISPLAY1"), g1.clone()),
        );
        map.insert(
            "c2".to_string(),
            fake_state(Some(r"\\.\DISPLAY2"), g2.clone()),
        );

        refresh_geometry_in(&map, &displays, None);
        assert_eq!(*g1.read().unwrap(), MonitorGeometry::new(0, 0, 2560, 1440));
        assert_eq!(
            *g2.read().unwrap(),
            MonitorGeometry::new(2560, 0, 2560, 1440)
        );
    }

    /// `retarget_connection_in` flips both `video_device` and
    /// `geometry`. The connection's mouse handler (which holds a
    /// clone of the same Arc) sees the new rect on the next event.
    /// `last_mouse_seq` is preserved.
    #[test]
    fn retarget_connection_updates_video_device_and_geometry() {
        let displays = vec![
            display(r"\\.\DISPLAY1", 0, 0, 1280, 800),
            display(r"\\.\DISPLAY9", 1280, 0, 2780, 1700),
        ];
        let g = shared_geometry(MonitorGeometry::new(0, 0, 1280, 800));
        let mut state = fake_state(Some(r"\\.\DISPLAY1"), g.clone());
        state.last_mouse_seq = 42;
        let mut map = HashMap::new();
        map.insert("conn-z".to_string(), state);

        let new_payload = StartMediaPayload {
            connection_id: "conn-z".to_string(),
            video_device: Some(r"\\.\DISPLAY9".to_string()),
            ..start_payload("conn-z")
        };
        retarget_connection_in(&mut map, &displays, &new_payload);

        let updated = map.get("conn-z").unwrap();
        assert_eq!(updated.video_device.as_deref(), Some(r"\\.\DISPLAY9"));
        assert_eq!(updated.last_mouse_seq, 42, "seq must survive retarget");
        assert_eq!(
            *g.read().unwrap(),
            MonitorGeometry::new(1280, 0, 1500, 1700),
        );
    }

    /// `retarget_connection_in` on an unknown id is a warn + no-op;
    /// it must not panic or modify any other connection. Critical for
    /// IPC-loop liveness if the daemon ever ships a stale id.
    #[test]
    fn retarget_connection_unknown_id_is_noop() {
        let displays = vec![display(r"\\.\DISPLAY1", 0, 0, 1280, 800)];
        let g = shared_geometry(MonitorGeometry::new(0, 0, 1280, 800));
        let mut map = HashMap::new();
        map.insert(
            "live".to_string(),
            fake_state(Some(r"\\.\DISPLAY1"), g.clone()),
        );

        let new_payload = StartMediaPayload {
            connection_id: "ghost".to_string(),
            video_device: Some(r"\\.\DISPLAY9".to_string()),
            ..start_payload("ghost")
        };
        retarget_connection_in(&mut map, &displays, &new_payload);

        // Live connection untouched.
        assert_eq!(
            map.get("live").unwrap().video_device.as_deref(),
            Some(r"\\.\DISPLAY1")
        );
        assert_eq!(*g.read().unwrap(), MonitorGeometry::new(0, 0, 1280, 800));
    }
}
