//! Non-Windows stub: returns `Err(Unsupported)` from `spawn()`.
//!
//! The Linux RandR / Wayland and macOS `CGDisplayRegisterReconfigurationCallback`
//! equivalents are not implemented in this PR — the worker's explicit
//! triggers (IDD `SetMode`, virtual display Attach / Detach) still
//! work on those platforms because they go through worker IPC, not the
//! OS broadcast path.

use super::error::DisplayWatcherError;
use tokio::sync::mpsc;

/// Event emitted whenever the OS reports a display reconfiguration.
/// Carries a monotonic sequence number for test observability — the
/// worker doesn't currently use it beyond "an event arrived".
#[derive(Debug, Clone, Copy)]
pub struct DisplayChangeEvent {
    pub seq: u64,
}

/// Stub watcher handle. Holding it does nothing — present only so the
/// caller's drop order matches the Windows path.
pub struct DisplayChangeWatcher;

/// Unsupported platform: returns `Err(Unsupported)`. Callers should
/// log a warning and substitute a dummy receiver.
pub fn spawn() -> Result<
    (
        DisplayChangeWatcher,
        mpsc::UnboundedReceiver<DisplayChangeEvent>,
    ),
    DisplayWatcherError,
> {
    Err(DisplayWatcherError::Unsupported)
}
