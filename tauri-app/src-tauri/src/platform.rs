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
