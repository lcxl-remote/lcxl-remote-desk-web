#[cfg(target_os = "linux")]
pub mod linux_system_setting;
pub mod system_setting_factory;

#[cfg(target_os = "windows")]
pub mod windows_system_setting;

#[cfg(target_os = "windows")]
pub mod windows;
