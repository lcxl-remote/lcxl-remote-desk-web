//! Linux display-change watcher: per-session backend dispatch.
//!
//! Linux has two display stacks, so the watcher chooses its backend at
//! runtime from [`crate::linux_display::detect_backend`]:
//!
//! - **X11** (`x11.rs`): a thread holding an X11 connection that listens
//!   for RandR screen / CRTC / output change notifications on the root
//!   window.
//! - **Wayland** (`wayland.rs`): a thread holding a Wayland connection
//!   that listens for `wl_registry` / `wl_output` changes (output
//!   add/remove, mode, geometry, scale). Core `wl_output` is supported
//!   by every compositor (GNOME / KDE / wlroots).
//! - **Headless** (no `DISPLAY` / `WAYLAND_DISPLAY`): `spawn()` returns
//!   `Unsupported` so the worker keeps running with explicit triggers
//!   only.
//!
//! Both backends share the watcher handle, the [`DisplayChangeEvent`]
//! type, and the [`spawn_with_runner`] orchestration; they differ only
//! in their runner. A runner connects to the display server, reports its
//! init result back through `init_tx`, then loops emitting one
//! [`DisplayChangeEvent`] per reconfiguration until the stop flag is set.

mod wayland;
mod x11;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::mpsc;

use super::error::DisplayWatcherError;
use crate::linux_display::{Backend, detect_backend};

/// How long to sleep / block between polls when no event is pending.
/// Display reconfiguration is rare, so a coarse interval keeps the
/// thread idle while still observing the stop flag promptly.
pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Event emitted whenever the OS reports a display reconfiguration.
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

/// Spawn the watcher for the active display backend. Returns the live
/// handle and the receiver end of the event channel. On failure, returns
/// a `DisplayWatcherError` — callers should `warn!` and substitute a
/// dummy receiver so the worker keeps running with explicit triggers
/// only.
pub fn spawn() -> Result<
    (
        DisplayChangeWatcher,
        mpsc::UnboundedReceiver<DisplayChangeEvent>,
    ),
    DisplayWatcherError,
> {
    match detect_backend() {
        Backend::X11 => spawn_with_runner(x11::runner),
        Backend::Wayland => spawn_with_runner(wayland::runner),
        Backend::Headless => Err(DisplayWatcherError::Unsupported),
    }
}

/// Internal orchestration: spawn a thread running `runner`, wait for its
/// init signal, and return the handle on success. Factored out so tests
/// can substitute a mock runner without touching the display server, and
/// shared by both the X11 and Wayland backends.
pub(super) fn spawn_with_runner<R>(
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

    /// Init failure from the runner propagates and `spawn_with_runner`
    /// joins cleanly (no hang). Exercises the orchestration without a
    /// display server. Backend-agnostic: both the X11 and Wayland
    /// runners report init through the same channel.
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
