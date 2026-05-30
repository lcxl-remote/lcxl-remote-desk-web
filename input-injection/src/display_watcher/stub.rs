//! Fallback stub for platforms without a display-change watcher
//! (currently macOS): returns `Err(Unsupported)` from `spawn()`.
//!
//! A macOS implementation would use
//! `CGDisplayRegisterReconfigurationCallback`. Until then, the worker's
//! explicit triggers (virtual display SetMode / Attach / Detach) still
//! work there because they go through worker IPC, not the OS broadcast
//! path.

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
