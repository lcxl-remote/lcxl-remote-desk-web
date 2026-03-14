#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// System-level input blocking
pub fn block_input(block: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    return windows::block_input(block);
    #[cfg(target_os = "macos")]
    return macos::block_input(block);
    #[cfg(target_os = "linux")]
    return linux::block_input(block);
}

/// Check if the current platform supports the private screen feature
pub fn is_private_screen_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Check if running on Wayland
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            return false; // Wayland is not supported
        }
    }
    true
}
