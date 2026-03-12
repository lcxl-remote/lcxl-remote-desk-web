#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

/// 系统级输入拦截
pub fn block_input(block: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    return windows::block_input(block);
    #[cfg(target_os = "macos")]
    return macos::block_input(block);
    #[cfg(target_os = "linux")]
    return linux::block_input(block);
}

/// 检查当前平台是否支持隐私屏功能
pub fn is_private_screen_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        // 检查是否 Wayland
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            return false; // Wayland 不支持
        }
    }
    true
}
