//! One-shot WGC window capture. Never substitutes a monitor for a window.
use crate::{
    error::CaptureError,
    model::image_capture::{ImageInfo, ImageType},
};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::{
    Capture::*,
    DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
};
use windows::Win32::{
    Foundation::{CloseHandle, FILETIME, HMODULE, HWND},
    Graphics::{Direct3D::D3D_DRIVER_TYPE_HARDWARE, Direct3D11::*, Dxgi::IDXGIDevice},
    System::{
        Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
    UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsIconic, IsWindowVisible},
};
use windows_core::Interface;

/// Values resolved by the authorized caller, not accepted from model input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsWindowCaptureTarget {
    pub window_handle: isize,
    pub host_process_id: u32,
    pub host_process_started_at: u64,
}

pub struct WindowFrame {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}
impl ImageInfo for WindowFrame {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }
    fn get_data(&self) -> &[u8] {
        &self.pixels
    }
    fn get_width(&self) -> u32 {
        self.width
    }
    fn get_height(&self) -> u32 {
        self.height
    }
}

fn invalid(message: &str) -> CaptureError {
    CaptureError::new_custom_error(
        desk_utils::error::DeskErrorCode::PRECONDITION_FAILED,
        message,
    )
}

fn validate(target: &WindowsWindowCaptureTarget) -> Result<HWND, CaptureError> {
    require_default_desktop()?;
    let hwnd = HWND(target.window_handle as *mut std::ffi::c_void);
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    if pid == 0
        || pid != target.host_process_id
        || !unsafe { IsWindowVisible(hwnd) }.as_bool()
        || unsafe { IsIconic(hwnd) }.as_bool()
    {
        return Err(invalid("selected window is missing, hidden, or minimized"));
    }
    let mut own_session = 0;
    let mut target_session = 0;
    unsafe {
        ProcessIdToSessionId(std::process::id(), &mut own_session)?;
        ProcessIdToSessionId(pid, &mut target_session)?;
    }
    if own_session == 0 || own_session != target_session {
        return Err(invalid(
            "selected window is outside the interactive session",
        ));
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let result =
        unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    unsafe {
        let _ = CloseHandle(process);
    }
    result?;
    let start = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    if start != target.host_process_started_at {
        return Err(invalid("selected window host restarted"));
    }
    Ok(hwnd)
}

fn require_default_desktop() -> Result<(), CaptureError> {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
    };
    let desktop = unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS)? };
    let mut name = [0u16; 256];
    let result = unsafe {
        GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            std::mem::size_of_val(&name) as u32,
            None,
        )
    };
    unsafe {
        let _ = CloseDesktop(desktop);
    }
    result?;
    let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    if !String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case("Default") {
        return Err(invalid(
            "window capture is unavailable on a secure or non-default desktop",
        ));
    }
    Ok(())
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}
struct Pipeline {
    pool: Direct3D11CaptureFramePool,
    session: Option<GraphicsCaptureSession>,
    token: Option<i64>,
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            let _ = self.pool.RemoveFrameArrived(token);
        }
        if let Some(session) = self.session.take() {
            let _ = session.Close();
        }
        let _ = self.pool.Close();
    }
}

/// Returns owned pixels; all COM and GPU resources remain on a dedicated thread.
pub fn capture_independent_window(
    target: &WindowsWindowCaptureTarget,
) -> Result<WindowFrame, CaptureError> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static BUSY: AtomicBool = AtomicBool::new(false);
    struct Admission;
    impl Drop for Admission {
        fn drop(&mut self) {
            BUSY.store(false, Ordering::Release);
        }
    }
    BUSY.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| invalid("a previous window capture is still running"))?;
    let admission = Admission;
    let target = target.clone();
    let (reply, result) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("window-screenshot".into())
        .spawn(move || {
            let output = capture(&target);
            drop(admission);
            let _ = reply.send(output);
        })?;
    // Timeout does not imply native cancellation. Admission stays closed until
    // the original thread releases all COM/GPU resources.
    result.recv_timeout(Duration::from_secs(5))?
}

fn capture(target: &WindowsWindowCaptureTarget) -> Result<WindowFrame, CaptureError> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let _apartment = Apartment;
    let hwnd = validate(target)?;
    if !GraphicsCaptureSession::IsSupported()? {
        return Err(invalid("Windows Graphics Capture is unavailable"));
    }
    let factory: IGraphicsCaptureItemInterop =
        windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
    let item = unsafe { factory.CreateForWindow::<GraphicsCaptureItem>(hwnd)? };
    let size = item.Size()?;
    dimensions(size.Width, size.Height)?;
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or_else(|| invalid("missing capture device"))?;
    let context = context.ok_or_else(|| invalid("missing capture context"))?;
    let dxgi: IDXGIDevice = device.cast()?;
    let direct: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &direct,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        1,
        size,
    )?;
    let mut pipeline = Pipeline {
        pool,
        session: None,
        token: None,
    };
    pipeline.session = Some(pipeline.pool.CreateCaptureSession(&item)?);
    let (arrived, frames) = mpsc::sync_channel(1);
    pipeline.token = Some(
        pipeline
            .pool
            .FrameArrived(&TypedEventHandler::new(move |_, _| {
                let _ = arrived.try_send(());
                Ok(())
            }))?,
    );
    let session = pipeline.session.as_ref().unwrap();
    session.SetIsCursorCaptureEnabled(false)?;
    validate(target)?;
    session.StartCapture()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(invalid(
                "window capture timed out waiting for a fresh frame",
            ));
        }
        frames.recv_timeout(remaining)?;
        let Ok(frame) = pipeline.pool.TryGetNextFrame() else {
            continue;
        };
        let result = read_frame(&device, &context, &frame, size.Width, size.Height);
        let _ = frame.Close();
        let result = result?;
        validate(target)?;
        return Ok(result);
    }
}

fn dimensions(width: i32, height: i32) -> Result<(u32, u32), CaptureError> {
    if width <= 0 || height <= 0 || i64::from(width) * i64::from(height) > 16_777_216 {
        return Err(invalid(
            "window capture dimensions exceed the bounded pixel budget",
        ));
    }
    Ok((width as u32, height as u32))
}

fn read_frame(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &Direct3D11CaptureFrame,
    expected_width: i32,
    expected_height: i32,
) -> Result<WindowFrame, CaptureError> {
    let size = frame.ContentSize()?;
    let (width, height) = dimensions(size.Width, size.Height)?;
    if size.Width != expected_width || size.Height != expected_height {
        return Err(invalid(
            "window resized during capture; request a fresh screenshot",
        ));
    }
    let surface: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
    let texture: ID3D11Texture2D = unsafe { surface.GetInterface()? };
    let mut description = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut description);
    }
    if description.Width != width || description.Height != height {
        return Err(invalid(
            "window frame texture and content dimensions disagree",
        ));
    }
    let mut staging = None;
    unsafe {
        device.CreateTexture2D(
            &super::wgc_compose::staging_texture_desc(width, height),
            None,
            Some(&mut staging),
        )?;
    }
    let staging = staging.ok_or_else(|| invalid("missing staging texture"))?;
    unsafe {
        context.CopyResource(&staging, &texture);
    }
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let result = if mapped.pData.is_null() || mapped.RowPitch < width * 4 {
        Err(invalid("invalid window frame row pitch"))
    } else {
        let mut pixels = vec![0; width as usize * height as usize * 4];
        for row in 0..height as usize {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                    width as usize * 4,
                )
            };
            pixels[row * width as usize * 4..(row + 1) * width as usize * 4].copy_from_slice(bytes);
        }
        Ok(WindowFrame {
            width,
            height,
            pixels,
        })
    };
    unsafe {
        context.Unmap(&staging, 0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dimensions_reject_empty_negative_and_oversized_frames() {
        assert!(dimensions(0, 1).is_err());
        assert!(dimensions(-1, 100).is_err());
        assert!(dimensions(i32::MAX, i32::MAX).is_err());
        assert_eq!(dimensions(1920, 1080).unwrap(), (1920, 1080));
    }
}
