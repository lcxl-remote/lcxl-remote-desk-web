//! Platform-specific capture and coordinate metadata for one selected window.
use super::*;

#[cfg(target_os = "macos")]
pub(crate) use desk_capture_engine::image_capture::mac_screencapturekit::MacWindowCaptureTarget as WindowCaptureTarget;
#[cfg(windows)]
pub(crate) use desk_capture_engine::image_capture::windows_window_capture::WindowsWindowCaptureTarget as WindowCaptureTarget;
#[cfg(not(any(windows, target_os = "macos")))]
#[derive(Clone, PartialEq)]
pub(crate) struct WindowCaptureTarget;

pub(super) struct CapturedWindow {
    pub frame: Box<dyn ImageInfo + Send + Sync>,
    pub geometry: Option<desk_agent_protocol::background_input::WindowInputGeometry>,
    pub dpi: (u32, u32),
}

#[cfg(windows)]
pub(super) fn capture(target: WindowCaptureTarget) -> Result<CapturedWindow, AgentError> {
    use desk_capture_engine::image_capture::windows_window_capture::capture_independent_window;
    use windows::Win32::{Foundation::HWND, UI::HiDpi::GetDpiForWindow};
    let hwnd = HWND(target.window_handle as *mut std::ffi::c_void);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        return Err(internal("cannot determine the selected window DPI"));
    }
    let frame = capture_independent_window(&target).map_err(capture_err)?;
    if unsafe { GetDpiForWindow(hwnd) } != dpi {
        return Err(internal("selected window DPI changed during capture"));
    }
    let geometry = desk_agent_protocol::background_input::WindowInputGeometry {
        width_pixels: frame.get_width(),
        height_pixels: frame.get_height(),
        width_millipoints: u64::from(frame.get_width()) * 96_000 / u64::from(dpi),
        height_millipoints: u64::from(frame.get_height()) * 96_000 / u64::from(dpi),
    };
    Ok(CapturedWindow {
        frame: Box::new(frame),
        geometry: Some(geometry),
        dpi: (dpi, dpi),
    })
}

#[cfg(target_os = "macos")]
pub(super) fn capture(target: WindowCaptureTarget) -> Result<CapturedWindow, AgentError> {
    let geometry = desk_agent_protocol::background_input::WindowInputGeometry {
        width_pixels: target.width.ceil() as u32,
        height_pixels: target.height.ceil() as u32,
        width_millipoints: (target.width * 1000.0).round() as u64,
        height_millipoints: (target.height * 1000.0).round() as u64,
    };
    let frame =
        desk_capture_engine::image_capture::mac_screencapturekit::capture_independent_window(
            &target,
        )
        .map_err(capture_err)?;
    Ok(CapturedWindow {
        frame,
        geometry: Some(geometry),
        dpi: capture_dpi(),
    })
}

#[cfg(not(any(windows, target_os = "macos")))]
pub(super) fn capture(_: WindowCaptureTarget) -> Result<CapturedWindow, AgentError> {
    Err(internal(
        "independent window capture is unavailable on this platform",
    ))
}
