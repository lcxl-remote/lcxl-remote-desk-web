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
    /// Platform doesn't support a display-change watcher. Stub platforms
    /// return this from `spawn()` so callers can detect and treat it as
    /// "no OS-driven refresh available".
    Unsupported,
    /// macOS: CoreGraphics rejected callback registration.
    #[cfg(target_os = "macos")]
    MacRegistration(i32),
    /// Linux: failed to connect to the X11 display server when starting
    /// the RandR watcher.
    #[cfg(target_os = "linux")]
    X11Connect(x11rb::errors::ConnectError),
    /// Linux: an X11 request (RandR version query / select-input) failed
    /// while setting up the watcher.
    #[cfg(target_os = "linux")]
    X11Reply(x11rb::errors::ReplyError),
    /// Linux: failed to connect to the Wayland compositor when starting
    /// the `wl_output` watcher.
    #[cfg(target_os = "linux")]
    WaylandConnect(wayland_client::ConnectError),
    /// Linux: a Wayland dispatch (registry roundtrip, socket read, or
    /// pending-event dispatch) failed while running the watcher.
    #[cfg(target_os = "linux")]
    WaylandReply(wayland_client::DispatchError),
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
            #[cfg(target_os = "macos")]
            DisplayWatcherError::MacRegistration(code) => {
                write!(
                    f,
                    "CGDisplayRegisterReconfigurationCallback: CGError {code}"
                )
            }
            #[cfg(target_os = "linux")]
            DisplayWatcherError::X11Connect(e) => write!(f, "X11 connect: {e}"),
            #[cfg(target_os = "linux")]
            DisplayWatcherError::X11Reply(e) => write!(f, "X11 RandR setup: {e}"),
            #[cfg(target_os = "linux")]
            DisplayWatcherError::WaylandConnect(e) => write!(f, "Wayland connect: {e}"),
            #[cfg(target_os = "linux")]
            DisplayWatcherError::WaylandReply(e) => write!(f, "Wayland dispatch: {e}"),
        }
    }
}

#[cfg(target_os = "linux")]
impl From<wayland_client::ConnectError> for DisplayWatcherError {
    fn from(e: wayland_client::ConnectError) -> Self {
        DisplayWatcherError::WaylandConnect(e)
    }
}

#[cfg(target_os = "linux")]
impl From<wayland_client::DispatchError> for DisplayWatcherError {
    fn from(e: wayland_client::DispatchError) -> Self {
        DisplayWatcherError::WaylandReply(e)
    }
}

#[cfg(target_os = "linux")]
impl From<wayland_client::backend::WaylandError> for DisplayWatcherError {
    fn from(e: wayland_client::backend::WaylandError) -> Self {
        // `conn.flush()` / `guard.read()` surface a backend I/O error;
        // fold it into the dispatch variant via `DispatchError::Backend`.
        DisplayWatcherError::WaylandReply(wayland_client::DispatchError::Backend(e))
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
