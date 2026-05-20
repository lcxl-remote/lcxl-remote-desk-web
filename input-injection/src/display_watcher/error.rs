use std::io;

/// Errors that can occur while spawning the display-change watcher.
///
/// All variants are recoverable from the caller's standpoint: the
/// worker logs a warning and continues without the OS-driven refresh
/// path (the explicit IDD `SetMode` / Attach / Detach triggers still
/// work).
#[derive(Debug)]
pub enum DisplayWatcherError {
    /// Failed to `std::thread::Builder::spawn` the message-pump thread.
    SpawnThread(io::Error),
    /// `RegisterClassExW` returned 0 — the Win32 docs list a few
    /// pathological causes (out of memory / invalid hInstance) but in
    /// practice this only fires in extremely degraded process state.
    RegisterClass(io::Error),
    /// `CreateWindowExW` returned NULL. The most likely cause is
    /// running inside a process that has no interactive window station
    /// (e.g. accidentally launched in session 0).
    CreateWindow(io::Error),
    /// The thread terminated before it could report back the
    /// `CreateWindowExW` result — usually means an internal panic
    /// inside the thread body.
    ThreadDiedBeforeInit,
    /// Platform doesn't support a display-change watcher. Stub
    /// platforms (non-Windows) return this from `spawn()` so callers
    /// can detect and treat it as "no OS-driven refresh available".
    Unsupported,
}

impl std::fmt::Display for DisplayWatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayWatcherError::SpawnThread(e) => write!(f, "spawn watcher thread: {e}"),
            DisplayWatcherError::RegisterClass(e) => write!(f, "RegisterClassExW: {e}"),
            DisplayWatcherError::CreateWindow(e) => write!(f, "CreateWindowExW: {e}"),
            DisplayWatcherError::ThreadDiedBeforeInit => {
                write!(f, "watcher thread terminated before init")
            }
            DisplayWatcherError::Unsupported => {
                write!(f, "display watcher not supported on this platform")
            }
        }
    }
}

impl std::error::Error for DisplayWatcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DisplayWatcherError::SpawnThread(e)
            | DisplayWatcherError::RegisterClass(e)
            | DisplayWatcherError::CreateWindow(e) => Some(e),
            _ => None,
        }
    }
}
