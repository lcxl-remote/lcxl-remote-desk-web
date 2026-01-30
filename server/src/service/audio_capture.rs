pub mod audio_capture_factory;
#[cfg(target_os = "linux")]
pub mod pipewire_capture;
#[cfg(target_os = "windows")]
pub mod wasapi_capture;

#[cfg(target_os = "macos")]
pub mod mac_screencapturekit;
