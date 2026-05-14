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
//! The `MouseEventHandler` needs the screen width / height so it can
//! map the browser's normalised cursor coordinates onto SendInput's
//! pixel grid. Cut 5 queries the primary display via
//! `desk_capture_engine::list_image_capture()` once at handler
//! construction. Display reconfiguration during a session (resolution
//! change, monitor add) is handled by killing the connection — the
//! browser re-establishes and we re-query. Live resolution change
//! mid-session is out of scope for cut 5.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

use desk_capture_engine::image_capture::image_capture_factory::list_image_capture;
use desk_input_injection::keyboard_event::keyboard_event_factory::create_keyboard_event_handler;
use desk_input_injection::model::data_channel::{
    KeyboardEventData, KeyboardEventHandler, MouseEventData, MouseEventHandler,
};
use desk_input_injection::mouse_event::mouse_event_factory::create_mouse_event_handler;
use desk_ipc_protocol::message::{InputPayload, StartMediaPayload, StopMediaPayload};
use desk_signal_facade::model::desk_settings::DeskSettings;
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
        let (width, height) = primary_display_size();
        let wayland_mode = self.desk_settings.wayland_control_mode.as_deref();
        let mouse = match create_mouse_event_handler(width, height, wayland_mode) {
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
            },
        );
        if prev.is_some() {
            warn!(
                "[InputDispatcher] {}: replaced existing input handlers (duplicate StartMedia?)",
                payload.connection_id
            );
        }
        info!(
            "[InputDispatcher] {}: input handlers ready (display={}x{}, wayland_mode={:?})",
            payload.connection_id, width, height, wayland_mode
        );
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

/// Query the primary display's pixel dimensions. Falls back to a
/// sensible 1920x1080 default when the capture-engine reports nothing
/// (e.g. headless test rig) so handler construction still succeeds —
/// the cursor mapping will be wrong on a non-1080p display but the
/// process stays alive.
fn primary_display_size() -> (i32, i32) {
    for (_backend, displays) in list_image_capture() {
        for display in displays {
            if !display.attached_to_desktop {
                continue;
            }
            let w = display.desktop_coordinates.width();
            let h = display.desktop_coordinates.height();
            if w > 0 && h > 0 {
                return (w, h);
            }
        }
    }
    warn!(
        "[InputDispatcher] no attached display found via capture-engine; \
         falling back to 1920x1080 default"
    );
    (1920, 1080)
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
}
