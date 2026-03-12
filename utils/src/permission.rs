#[cfg(windows)]
use windows::Win32::UI::Shell::IsUserAnAdmin;

/// Check if the current process has administrative/root privileges.
pub fn is_admin() -> bool {
    #[cfg(windows)]
    {
        // On Windows, use IsUserAnAdmin from shell32
        unsafe { IsUserAnAdmin().as_bool() }
    }

    #[cfg(unix)]
    {
        // On Unix-like systems (Linux, macOS), check if the effective user ID is 0 (root)
        unsafe { libc::getuid() == 0 }
    }

    #[cfg(not(any(windows, unix)))]
    {
        // Fallback for other platforms
        false
    }
}
