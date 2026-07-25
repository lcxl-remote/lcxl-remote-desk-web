use std::sync::Arc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// Invoked by a platform input interceptor once the user has fully entered the
/// local escape chord that dismisses the privacy screen.
///
/// It runs on the interception thread, so implementations must return
/// immediately: macOS disables an event tap whose callback is slow, and the
/// privacy screen state machine tears that same thread down while handling the
/// dismissal, so waiting on it would deadlock.
pub type LocalEscapeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

/// System-level input blocking.
///
/// Only macOS routes `on_local_escape`; the other platforms rely on the Tauri
/// global shortcut alone and ignore it.
pub fn block_input(
    block: bool,
    on_local_escape: Option<LocalEscapeCallback>,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let _ = on_local_escape;
        return windows::block_input(block);
    }
    #[cfg(target_os = "macos")]
    return macos::block_input(block, on_local_escape);
    #[cfg(target_os = "linux")]
    {
        let _ = on_local_escape;
        return linux::block_input(block);
    }
}

/// Resolve the attached LCXL virtual display to the OS monitor name.
pub fn virtual_display_name() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        return windows::virtual_display_name();
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}
