#[cfg(target_os = "windows")]
pub mod dxgi_capture;
#[cfg(target_os = "windows")]
pub mod dxgi_compose;
#[cfg(target_os = "windows")]
pub mod gdi_capture;
pub mod image_capture_factory;
#[cfg(target_os = "windows")]
pub mod wgc_capture;
#[cfg(target_os = "windows")]
pub mod wgc_compose;
#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod x11_capture;

#[cfg(target_os = "linux")]
pub mod pipewire_capture;
#[cfg(target_os = "linux")]
pub mod pipewire_utils;
#[cfg(target_os = "linux")]
pub mod portal_client;
#[cfg(target_os = "linux")]
pub mod wayland_portal_capture;

#[cfg(target_os = "macos")]
pub mod mac_screencapturekit;
