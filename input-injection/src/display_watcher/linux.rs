//! Linux X11 display-change watcher implementation.
//!
//! Spawns a dedicated thread that owns its own X11 connection, asks the
//! RandR extension for screen / CRTC / output change notifications on the
//! root window, and forwards each one as a [`DisplayChangeEvent`]. This
//! mirrors the Windows `WM_DISPLAYCHANGE` path so the worker can refresh
//! per-connection mouse geometry without tearing down the connection.
//!
//! ## Shutdown
//!
//! `poll_for_event` is used with a short sleep instead of the blocking
//! `wait_for_event` so the thread can observe the stop flag and exit
//! promptly when the watcher is dropped (a blocking read has no portable
//! wakeup).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::randr::{ConnectionExt as _, NotifyMask};
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;

use super::error::DisplayWatcherError;

/// How long to sleep between polls when no event is pending. Display
/// reconfiguration is rare, so a coarse interval keeps the thread idle.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Event emitted whenever RandR reports a display reconfiguration.
/// `seq` is a monotonic per-watcher counter — useful for test
/// observability and for distinguishing "we missed N events" from
/// "channel went silent".
#[derive(Debug, Clone, Copy)]
pub struct DisplayChangeEvent {
    pub seq: u64,
}

/// Handle to the live watcher. Drop signals the thread to stop and joins
/// it.
pub struct DisplayChangeWatcher {
    stop: Arc<AtomicBool>,
    /// `Option` so Drop can `take()` the join handle.
    join: Option<JoinHandle<()>>,
}

impl Drop for DisplayChangeWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

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

fn runner(
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

/// Spawn the watcher. Returns the live handle and the receiver end of the
/// event channel. On failure, returns a `DisplayWatcherError` — callers
/// should `warn!` and substitute a dummy receiver so the worker keeps
/// running with explicit triggers only.
pub fn spawn() -> Result<
    (
        DisplayChangeWatcher,
        mpsc::UnboundedReceiver<DisplayChangeEvent>,
    ),
    DisplayWatcherError,
> {
    spawn_with_runner(runner)
}

/// Internal orchestration: spawn a thread running `runner`, wait for its
/// init signal, and return the handle on success. Factored out so tests
/// can substitute a mock runner without touching X11.
fn spawn_with_runner<R>(
    runner: R,
) -> Result<
    (
        DisplayChangeWatcher,
        mpsc::UnboundedReceiver<DisplayChangeEvent>,
    ),
    DisplayWatcherError,
>
where
    R: FnOnce(
            mpsc::UnboundedSender<DisplayChangeEvent>,
            Arc<AtomicBool>,
            std::sync::mpsc::Sender<Result<(), DisplayWatcherError>>,
        ) + Send
        + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), DisplayWatcherError>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_thread = Arc::clone(&stop);

    let join = std::thread::Builder::new()
        .name("display-watcher".to_string())
        .spawn(move || runner(tx, stop_for_thread, init_tx))
        .map_err(DisplayWatcherError::SpawnThread)?;

    match init_rx.recv() {
        Ok(Ok(())) => Ok((
            DisplayChangeWatcher {
                stop,
                join: Some(join),
            },
            rx,
        )),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(DisplayWatcherError::ThreadDiedBeforeInit)
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

    /// Init failure from the runner propagates and `spawn_with_runner`
    /// joins cleanly (no hang). Exercises the orchestration without X11.
    #[test]
    fn spawn_with_runner_propagates_init_failure() {
        let result = spawn_with_runner(|_tx, _stop, init_tx| {
            let _ = init_tx.send(Err(DisplayWatcherError::ThreadDiedBeforeInit));
        });
        assert!(matches!(
            result,
            Err(DisplayWatcherError::ThreadDiedBeforeInit)
        ));
    }

    /// A runner that signals Ok and then watches the stop flag yields a
    /// live watcher; dropping it sets the flag and joins the thread.
    #[test]
    fn spawn_with_runner_returns_handle_and_drop_stops_thread() {
        let result = spawn_with_runner(|_tx, stop, init_tx| {
            let _ = init_tx.send(Ok(()));
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let (watcher, _rx) = result.expect("init ok");
        drop(watcher); // sets stop, joins — must not hang
    }

    /// A runner that drops `init_tx` without signalling is reported as
    /// `ThreadDiedBeforeInit` rather than hanging.
    #[test]
    fn spawn_with_runner_reports_thread_died_without_signal() {
        let result = spawn_with_runner(|_tx, _stop, _init_tx| {});
        assert!(matches!(
            result,
            Err(DisplayWatcherError::ThreadDiedBeforeInit)
        ));
    }
}
