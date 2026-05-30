//! X11 display-change watcher implementation.
//!
//! Owns its own X11 connection, asks the RandR extension for screen /
//! CRTC / output change notifications on the root window, and forwards
//! each one as a [`DisplayChangeEvent`]. This mirrors the Windows
//! `WM_DISPLAYCHANGE` path so the worker can refresh per-connection
//! mouse geometry without tearing down the connection.
//!
//! ## Shutdown
//!
//! `poll_for_event` is used with a short sleep instead of the blocking
//! `wait_for_event` so the thread can observe the stop flag and exit
//! promptly when the watcher is dropped (a blocking read has no portable
//! wakeup).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::randr::{ConnectionExt as _, NotifyMask};
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;

use super::super::error::DisplayWatcherError;
use super::{DisplayChangeEvent, POLL_INTERVAL};

/// Connect to X11 and arm RandR change notifications on the root window.
fn init_connection() -> Result<(RustConnection, Window), DisplayWatcherError> {
    let (conn, screen_num) = x11rb::connect(None).map_err(DisplayWatcherError::X11Connect)?;
    let root = conn.setup().roots[screen_num].root;

    // Some servers gate RandR event delivery on a version handshake.
    conn.randr_query_version(1, 5)
        .map_err(|e| DisplayWatcherError::X11Reply(e.into()))?
        .reply()
        .map_err(DisplayWatcherError::X11Reply)?;

    let mask = NotifyMask::SCREEN_CHANGE | NotifyMask::CRTC_CHANGE | NotifyMask::OUTPUT_CHANGE;
    conn.randr_select_input(root, mask)
        .map_err(|e| DisplayWatcherError::X11Reply(e.into()))?
        .check()
        .map_err(DisplayWatcherError::X11Reply)?;

    Ok((conn, root))
}

/// Whether an event signals a display reconfiguration we should forward.
fn is_display_change(event: &Event) -> bool {
    matches!(
        event,
        Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_)
    )
}

pub(super) fn runner(
    tx: mpsc::UnboundedSender<DisplayChangeEvent>,
    stop: Arc<AtomicBool>,
    init_tx: std::sync::mpsc::Sender<Result<(), DisplayWatcherError>>,
) {
    let conn = match init_connection() {
        Ok((conn, _root)) => {
            if init_tx.send(Ok(())).is_err() {
                return;
            }
            conn
        }
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    let mut seq: u64 = 0;
    while !stop.load(Ordering::Acquire) {
        match conn.poll_for_event() {
            Ok(Some(event)) => {
                if is_display_change(&event) {
                    seq += 1;
                    if tx.send(DisplayChangeEvent { seq }).is_err() {
                        // Receiver gone (worker shutting down).
                        break;
                    }
                }
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(e) => {
                log::warn!("display-watcher: X11 poll failed, stopping: {e}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::randr::ScreenChangeNotifyEvent;
    use x11rb::protocol::xproto::MapNotifyEvent;

    #[test]
    fn screen_change_is_a_display_change() {
        let screen = ScreenChangeNotifyEvent::default();
        assert!(is_display_change(&Event::RandrScreenChangeNotify(screen)));
    }

    #[test]
    fn unrelated_events_are_ignored() {
        assert!(!is_display_change(&Event::MapNotify(
            MapNotifyEvent::default()
        )));
    }
}
