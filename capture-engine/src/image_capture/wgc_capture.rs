//! Windows.Graphics.Capture (WGC) capture backend.
//!
//! WGC asks the DWM to deliver the composed desktop surface, including
//! hardware-overlay regions that DXGI Desktop Duplication renders as
//! black placeholders (the failure mode that motivated this backend —
//! DXGI dirty-rect and per-rect-compose hypotheses were exhaustively
//! ruled out before the switch).
//!
//! Trade-off: WGC does not expose dirty rect metadata in the baseline
//! ABI; this backend always reports `dirty_rects = None`, forcing
//! downstream encoders through a full BGRA→YUV pass each frame. CPU
//! cost goes up vs. DXGI partial-update, but content correctness is
//! preserved for the video / HTML5-overlay workloads that motivated
//! the switch. `Direct3D11CaptureFrame::DirtyRegions()` (Win11 22H2+)
//! is a future optimization.

use std::backtrace::Backtrace;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use desk_signal_facade::model::{desk_settings::DeskSettings, image_capture::DisplayInfo};
use desk_utils::error::DeskErrorCode;
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{
    CloseHandle, E_ILLEGAL_STATE_CHANGE, HANDLE, RPC_E_CHANGED_MODE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, ID3D11Device, ID3D11DeviceContext, ID3D11Resource,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{DXGI_ERROR_DEVICE_REMOVED, IDXGIDevice};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, RGBQUAD, ReleaseDC,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetIconInfo, ICONINFO,
};
use windows_core::{Interface, PCWSTR};

use crate::error::CaptureError;
use crate::image_capture::dxgi_capture::ScreenRecordManager;
use crate::image_capture::monitors::{
    enum_display_infos, find_monitor_by_device_name, select_display_info_by_name,
};
use crate::image_capture::wgc_compose;
use crate::model::image_capture::CursorSyncData;
use crate::model::image_capture::{
    CaptureRequest, CaptureResult, CursorCaptureMode, ImageCapture, ImageCaptureType, ImageInfo,
    ImageOutputEnumerator, ImageType,
};

/// Placeholder image returned when content_changed == false (Map was not called).
struct EmptyImageInfo;

impl ImageInfo for EmptyImageInfo {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }
    fn get_data(&self) -> &[u8] {
        &[]
    }
    fn get_width(&self) -> u32 {
        0
    }
    fn get_height(&self) -> u32 {
        0
    }
}

/// Fingerprint used to skip emitting `cursor_update` when the cursor
/// shape has not changed. `Shape` carries a hash of the cursor pixel
/// buffer, not the cursor's screen position. `screen_width` /
/// `screen_height` are included so a mid-session resolution change
/// (via `frame_pool.Recreate` below) forces a fresh emission even
/// when the cursor shape is unchanged — the front-end would
/// otherwise reuse a stale `screen_width` and mis-scale the cursor
/// sprite.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WgcCursorFingerprint {
    Hidden,
    Shape {
        id: u64,
        screen_width: u32,
        screen_height: u32,
    },
}

/// Holds all of the WGC pipeline state. Lazy-built inside `capture()`
/// (the shared capture loop's thread) so the actual `CreateForMonitor`
/// / `CreateFreeThreaded` / `FrameArrived` subscription happens on the
/// same thread that calls `TryGetNextFrame`. Mirrors the DXGI backend's
/// `ScreenOutput: Option<...>` rebuild-on-failure pattern.
struct WgcPipeline {
    /// Held to keep the capture item alive for the lifetime of the
    /// frame pool / session subscription. Dropped after `shutdown()`.
    _item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// Auto-reset Win32 event signalled by the `FrameArrived` handler.
    /// We block on it via `WaitForSingleObject` from `capture()` so the
    /// pull-style `ImageCapture::capture()` contract maps cleanly to
    /// WGC's push-style callback API.
    frame_event: HANDLE,
    /// Same handle value, shared with the `FrameArrived` closure.
    /// Drop sets the inner `Option<isize>` to `None` *before*
    /// `CloseHandle`, so any in-flight callback observes the cleared
    /// slot and skips `SetEvent` rather than poking a closed handle.
    handle_slot: Arc<Mutex<Option<isize>>>,
    frame_arrived_token: i64,
    staging: ID3D11Texture2D,
    staging_size: (u32, u32),
}

impl WgcPipeline {
    fn shutdown(&mut self) {
        // Step 1: clear the slot under the lock so any in-flight callback
        // can no longer post to `frame_event`.
        if let Ok(mut slot) = self.handle_slot.lock() {
            *slot = None;
        }
        // Step 2: drop the FrameArrived subscription. After this returns
        // the closure is no longer invoked.
        if let Err(e) = self.frame_pool.RemoveFrameArrived(self.frame_arrived_token) {
            log::warn!("[WGC] RemoveFrameArrived failed: {:?}", e);
        }
        // Step 3: stop capturing.
        if let Err(e) = self.session.Close() {
            log::warn!("[WGC] session.Close failed: {:?}", e);
        }
        // Step 4: release pool resources.
        if let Err(e) = self.frame_pool.Close() {
            log::warn!("[WGC] frame_pool.Close failed: {:?}", e);
        }
        // Step 5: close the Win32 event handle now that no callback can
        // see it.
        if !self.frame_event.is_invalid() {
            let _ = unsafe { CloseHandle(self.frame_event) };
            self.frame_event = HANDLE::default();
        }
    }
}

impl Drop for WgcPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// SAFETY: `WgcImageCapture` is constructed on one thread (the actix
// request thread that handles a subscribe call) and then handed off
// to a dedicated capture-loop thread spawned by
// `shared_capture.rs::run_capture_loop`. The non-`Send` interior is
// limited to Win32 `HANDLE` / `HMONITOR` opaque handles, which the
// Windows API documents as safe to use across threads. The struct is
// never accessed from more than one thread at a time.
unsafe impl Send for WgcImageCapture {}

pub struct WgcImageOutputEnumerator;

impl WgcImageOutputEnumerator {
    /// Fails if `Windows.Graphics.Capture` is not supported on this OS
    /// (Win10 1809 / Server 2016 and older). When `new` returns `Err`,
    /// `list_image_capture()` skips the WGC entry, so the frontend
    /// dropdown automatically hides this backend.
    pub fn new() -> Result<Self, CaptureError> {
        match GraphicsCaptureSession::IsSupported() {
            Ok(true) => Ok(Self),
            // Structural unavailability: tagged FEATURE_UNAVAILABLE so the
            // factory can transparently fall back to DXGI for capture
            // instances, and `list_image_capture` can demote the
            // enumeration log to WARN (this is the expected path on
            // Winlogon / SYSTEM-token workers).
            Ok(false) => CaptureError::custom_error(
                DeskErrorCode::FEATURE_UNAVAILABLE,
                "Windows.Graphics.Capture is not supported on this system",
            ),
            Err(e) => CaptureError::custom_error(
                DeskErrorCode::FEATURE_UNAVAILABLE,
                &format!("Failed to query GraphicsCaptureSession::IsSupported: {}", e),
            ),
        }
    }
}

impl ImageOutputEnumerator for WgcImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        // GDI EnumDisplayMonitors, not DXGI EnumAdapters/EnumOutputs.
        // WGC binds capture via
        // `IGraphicsCaptureItemInterop::CreateForMonitor(HMONITOR)`,
        // and `HMONITOR` is a GDI-layer handle — `EnumDisplayMonitors`
        // is the natural source. DXGI also enumerates IDD virtual
        // displays (the IDD driver registers a virtual IDXGIAdapter),
        // but it hands back IDXGIOutput, not HMONITOR, so we still
        // need GDI here regardless of IDD support; this path has been
        // validated end-to-end.
        let infos = enum_display_infos()?;
        log::info!(
            "WgcImageOutputEnumerator: enumerated {} monitor(s) via EnumDisplayMonitors",
            infos.len()
        );
        Ok(infos)
    }
}

pub struct WgcImageCapture {
    manager: Arc<ScreenRecordManager>,
    display_info: DisplayInfo,
    /// HMONITOR is `*mut c_void` under the hood, which Rust marks as
    /// `!Send`. Store the opaque value as `isize` so the value itself
    /// is trivially `Send`. Reconstructed on use.
    hmonitor_raw: isize,
    monitor_size: SizeInt32,
    pipeline: Option<WgcPipeline>,
    last_cursor_fingerprint: Option<WgcCursorFingerprint>,
    /// Once-per-instance thread bookkeeping. The shared capture loop
    /// spawns a fresh `std::thread`, which has never been attached to
    /// the input desktop and has no COM initialization. We have to do
    /// both on the first `capture()` call (which lands on that loop
    /// thread, not the request thread that owns `new`).
    com_initialized: bool,
    desktop_attached: bool,
}

impl WgcImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, CaptureError> {
        // Same `IsSupported()` check the enumerator runs — we hit this
        // path if a user has WGC saved in settings but lands on a
        // machine that no longer supports it.
        // The IsSupported failure modes here are structural — either
        // the OS is too old or the WGC broker service is not present
        // in our session (Winlogon / SYSTEM). Both are tagged
        // FEATURE_UNAVAILABLE so the factory can fall back to DXGI for
        // this capture instance without surfacing the error to the
        // pipeline. Failures later in this function (EnumOutputs,
        // CreateDirect3D11DeviceFromDXGIDevice, …) keep their original
        // error code — those indicate real trouble and must not be
        // silently downgraded.
        match GraphicsCaptureSession::IsSupported() {
            Ok(true) => {}
            Ok(false) => {
                return CaptureError::custom_error(
                    DeskErrorCode::FEATURE_UNAVAILABLE,
                    "WGC requires Windows 10 1903+ (GraphicsCaptureSession::IsSupported = false)",
                );
            }
            Err(e) => {
                return CaptureError::custom_error(
                    DeskErrorCode::FEATURE_UNAVAILABLE,
                    &format!("Failed to query GraphicsCaptureSession::IsSupported: {}", e),
                );
            }
        }

        // Resolve the target monitor through the GDI EnumDisplayMonitors
        // path, which is the only enumeration that returns HMONITOR
        // (what `CreateForMonitor` needs). Selection happens before
        // D3D11 device creation so an empty / unknown device_name
        // surfaces INVALID_PARAMS without the cost (and the
        // headless-CI failure mode) of building a D3D11 device.
        let infos = enum_display_infos()?;
        let display_info = select_display_info_by_name(&infos, &settings.video_device_name)?;
        let monitor_entry =
            find_monitor_by_device_name(&display_info.device_name)?.ok_or_else(|| {
                CaptureError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!(
                        "device_name {:?} disappeared between enum_display_infos \
                         and find_monitor_by_device_name (race with display \
                         hot-plug?)",
                        display_info.device_name
                    ),
                )
            })?;
        let manager = ScreenRecordManager::new(settings)?;
        let hmonitor_raw = monitor_entry.hmonitor_raw;
        let monitor_size = SizeInt32 {
            Width: display_info.desktop_coordinates.width(),
            Height: display_info.desktop_coordinates.height(),
        };

        log::info!(
            "[WGC] capture instance created: device_name={:?} monitor_size={}x{}",
            display_info.device_name,
            monitor_size.Width,
            monitor_size.Height
        );

        Ok(WgcImageCapture {
            manager,
            display_info,
            hmonitor_raw,
            monitor_size,
            pipeline: None,
            last_cursor_fingerprint: None,
            com_initialized: false,
            desktop_attached: false,
        })
    }

    /// Builds (or rebuilds) the WGC pipeline on the current thread.
    /// First call additionally attaches the thread to the input
    /// desktop and initialises COM as MTA — both prerequisites for
    /// `CreateForMonitor` / `CreateFreeThreaded` to work in the
    /// SessionWorker scenario.
    fn ensure_pipeline(&mut self, draw_mouse: bool) -> Result<(), CaptureError> {
        if !self.desktop_attached {
            // Reuse DXGI's helper: SessionWorker spawns capture threads
            // not yet attached to the user's input desktop.
            ScreenRecordManager::set_thread_input_desktop()?;
            self.desktop_attached = true;
        }

        if !self.com_initialized {
            // CoInitializeEx returns HRESULT, not Result, in
            // windows-rs 0.61 — branch on the raw status.
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.is_ok() {
                self.com_initialized = true;
            } else if hr == RPC_E_CHANGED_MODE {
                log::info!(
                    "[WGC] CoInitializeEx returned RPC_E_CHANGED_MODE; thread already \
                     initialised under a different apartment, reusing existing init"
                );
                self.com_initialized = true;
            } else {
                hr.ok()?;
            }
        }

        if self.pipeline.is_some() {
            return Ok(());
        }

        // Sanity recheck in case the OS state changed between `new`
        // and the first `capture()`.
        if !GraphicsCaptureSession::IsSupported()? {
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "GraphicsCaptureSession::IsSupported flipped to false post-init",
            );
        }

        // Resolve the WinRT GraphicsCaptureItem for the target monitor
        // via the Win32 interop factory.
        let interop: IGraphicsCaptureItemInterop =
            windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // Reconstruct the HMONITOR from the stored opaque value. The
        // handle is process-wide and safe to invoke on any thread.
        let hmonitor = windows::Win32::Graphics::Gdi::HMONITOR(self.hmonitor_raw as *mut c_void);
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForMonitor::<GraphicsCaptureItem>(hmonitor) }?;
        let item_size = item.Size()?;

        // Wrap the existing D3D11 device (already created by the
        // shared ScreenRecordManager) as a WinRT IDirect3DDevice so
        // WGC can share buffers with our staging/copy path.
        let dxgi_device: IDXGIDevice = self.manager.device.cast()?;
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
        let d3d_device: IDirect3DDevice = inspectable.cast()?;

        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item_size,
        )?;
        let session = frame_pool.CreateCaptureSession(&item)?;

        // RenderInFrame mode → let WGC bake the OS cursor into the
        // captured frame; SyncNative/Disable → cursor is captured
        // separately via GetCursorInfo and emitted as `cursor_update`.
        session.SetIsCursorCaptureEnabled(draw_mouse)?;
        // Win11 22H2+ only; older OSes throw E_NOTIMPL — swallow it
        // and accept the yellow capture border.
        if let Err(e) = session.SetIsBorderRequired(false) {
            log::info!(
                "[WGC] SetIsBorderRequired(false) unsupported (E_NOTIMPL is expected on \
                 pre-22H2 Windows builds): {}",
                e
            );
        }

        // Auto-reset event: signalled by every FrameArrived, reset by
        // every successful WaitForSingleObject. Bridges WGC's push
        // model to the `ImageCapture::capture()` pull contract.
        let frame_event = unsafe { CreateEventW(None, false, false, PCWSTR::null())? };

        // Share the event handle's *value* (as isize, which is Send)
        // with the FrameArrived closure. Direct HANDLE capture is
        // rejected: windows-rs 0.61 declares `HANDLE(*mut c_void)`
        // and the inner pointer is not Send, so TypedEventHandler's
        // `Send + 'static` bound fails. The Drop path clears this
        // slot under the lock before closing the handle, so any
        // in-flight callback observes None and skips SetEvent.
        let handle_slot: Arc<Mutex<Option<isize>>> =
            Arc::new(Mutex::new(Some(frame_event.0 as isize)));
        let handle_for_cb = Arc::clone(&handle_slot);
        let handler =
            TypedEventHandler::<Direct3D11CaptureFramePool, windows_core::IInspectable>::new(
                move |_sender, _args| {
                    if let Ok(slot) = handle_for_cb.lock()
                        && let Some(value) = *slot
                    {
                        // SAFETY: `value` was captured from a still-open event
                        // handle inside the Mutex; `slot.is_some()` only while
                        // WgcPipeline::shutdown has not yet run. SetEvent is
                        // documented as thread-safe on Win32 event handles.
                        let h = HANDLE(value as *mut c_void);
                        let _ = unsafe { SetEvent(h) };
                    }
                    Ok(())
                },
            );

        let frame_arrived_token = frame_pool.FrameArrived(&handler)?;

        // Match the staging desc to whatever item.Size() reported;
        // ContentSize on the first frame may differ if the monitor
        // resized between `new` and now, but that's handled inside
        // `capture()` by the resize branch.
        let staging_w = item_size.Width as u32;
        let staging_h = item_size.Height as u32;
        let staging = Self::create_staging_texture(&self.manager.device, staging_w, staging_h)?;

        session.StartCapture()?;

        log::info!(
            "[WGC] pipeline initialised: item_size={}x{}, draw_mouse={}",
            item_size.Width,
            item_size.Height,
            draw_mouse
        );

        self.pipeline = Some(WgcPipeline {
            _item: item,
            frame_pool,
            session,
            frame_event,
            handle_slot,
            frame_arrived_token,
            staging,
            staging_size: (staging_w, staging_h),
        });
        Ok(())
    }

    fn create_staging_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        let desc = wgc_compose::staging_texture_desc(width, height);
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }?;
        Ok(tex.unwrap())
    }

    /// SyncNative cursor capture — equivalent to the GDI backend's
    /// `capture_cursor_update`, except the resulting `CursorSyncData`
    /// is annotated with this monitor's size (the WGC frame surface
    /// dimensions). Returns `Ok(None)` if the cursor shape is empty
    /// or the icon could not be inspected.
    fn capture_cursor_update(
        &self,
    ) -> Result<Option<(WgcCursorFingerprint, CursorSyncData)>, CaptureError> {
        let mut cursor_info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        let visible = unsafe {
            GetCursorInfo(&mut cursor_info)?;
            !cursor_info.hCursor.is_invalid() && cursor_info.flags == CURSOR_SHOWING
        };
        if !visible {
            return Ok(Some((
                WgcCursorFingerprint::Hidden,
                CursorSyncData {
                    visible: false,
                    ..Default::default()
                },
            )));
        }

        let shape_id = cursor_info.hCursor.0 as u64;
        let mut icon_info = ICONINFO::default();
        unsafe {
            GetIconInfo(cursor_info.hCursor.into(), &mut icon_info)?;
        }

        let screen_dc = unsafe { GetDC(None) };
        let mut bmp = BITMAP::default();
        let is_color = !icon_info.hbmColor.is_invalid();
        let target_hbm = if is_color {
            icon_info.hbmColor
        } else {
            icon_info.hbmMask
        };
        unsafe {
            GetObjectW(
                target_hbm.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bmp as *mut _ as _),
            )
        };

        let width = bmp.bmWidth as u32;
        let height = if is_color {
            bmp.bmHeight as u32
        } else {
            (bmp.bmHeight / 2) as u32
        };

        if width == 0 || height == 0 {
            if !icon_info.hbmMask.is_invalid() {
                let _ = unsafe { DeleteObject(icon_info.hbmMask.into()) };
            }
            if !icon_info.hbmColor.is_invalid() {
                let _ = unsafe { DeleteObject(icon_info.hbmColor.into()) };
            }
            unsafe { ReleaseDC(None, screen_dc) };
            return Ok(None);
        }

        let mut rgba_buffer: Vec<u8> = Vec::new();
        if is_color {
            let mut color_buffer: Vec<u8> = vec![0u8; (width * height * 4) as usize];
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD::default()],
            };
            unsafe {
                GetDIBits(
                    screen_dc,
                    icon_info.hbmColor,
                    0,
                    height,
                    Some(color_buffer.as_mut_ptr() as *mut _),
                    &mut bi,
                    DIB_RGB_COLORS,
                )
            };
            if let Some(packed) =
                wgc_compose::pack_bgra_cursor(&color_buffer, width, height, width * 4)
            {
                rgba_buffer = packed;
            }
        } else {
            let mask_height = bmp.bmHeight as u32;
            let pitch = width.div_ceil(32) * 4;
            let mask_buffer_size = pitch * mask_height;
            let mut mask_buffer = vec![0u8; mask_buffer_size as usize];
            let mut mask_bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(mask_height as i32),
                    biPlanes: 1,
                    biBitCount: 1,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD::default()],
            };
            unsafe {
                GetDIBits(
                    screen_dc,
                    icon_info.hbmMask,
                    0,
                    mask_height,
                    Some(mask_buffer.as_mut_ptr() as *mut _),
                    &mut mask_bi,
                    DIB_RGB_COLORS,
                )
            };
            if let Some(packed) = wgc_compose::pack_mono_cursor(&mask_buffer, width, height, pitch)
            {
                rgba_buffer = packed;
            }
        }

        if !icon_info.hbmMask.is_invalid() {
            let result = unsafe { DeleteObject(icon_info.hbmMask.into()) };
            if !result.as_bool() {
                log::warn!("[WGC] failed to delete cursor mask bitmap");
            }
        }
        if !icon_info.hbmColor.is_invalid() {
            let result = unsafe { DeleteObject(icon_info.hbmColor.into()) };
            if !result.as_bool() {
                log::warn!("[WGC] failed to delete cursor color bitmap");
            }
        }
        unsafe { ReleaseDC(None, screen_dc) };

        if rgba_buffer.is_empty() {
            return Ok(None);
        }

        use image::{ImageBuffer, Rgba};
        use std::io::Cursor;
        let img = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba_buffer)
            .unwrap_or_else(|| ImageBuffer::new(width, height));
        let mut png_data = Cursor::new(Vec::new());
        img.write_to(&mut png_data, image::ImageFormat::Png)
            .map_err(|e| {
                CaptureError::custom_error::<()>(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
                    .unwrap_err()
            })?;
        use base64::Engine;
        let base64_png = base64::engine::general_purpose::STANDARD.encode(png_data.into_inner());

        let screen_width = self.monitor_size.Width as u32;
        let screen_height = self.monitor_size.Height as u32;

        Ok(Some((
            WgcCursorFingerprint::Shape {
                id: shape_id,
                screen_width,
                screen_height,
            },
            CursorSyncData {
                base64_png,
                hotspot_x: icon_info.xHotspot as i32,
                hotspot_y: icon_info.yHotspot as i32,
                visible: true,
                shape_id,
                screen_width,
                screen_height,
                embedded: false,
            },
        )))
    }

    /// Reset the cursor fingerprint cache so the next capture pass
    /// re-emits a full `CursorSyncData`. Defensive backstop on the
    /// frame-pool-resize path; the size-aware fingerprint already
    /// covers the common case, but resetting here keeps every
    /// backend's rebuild branch symmetric.
    pub fn reset_cursor_cache(&mut self) {
        self.last_cursor_fingerprint = None;
    }
}

/// Mapped WGC frame. Holds the staging `ID3D11Resource` and unmaps it
/// on drop so a subsequent `capture()` call can re-map the same
/// resource without leaking.
struct WgcFrame {
    resource: ID3D11Resource,
    context: ID3D11DeviceContext,
    data: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
}

impl Drop for WgcFrame {
    fn drop(&mut self) {
        unsafe { self.context.Unmap(&self.resource, 0) };
    }
}

impl ImageInfo for WgcFrame {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }
    fn get_data(&self) -> &[u8] {
        &self.data
    }
    fn get_width(&self) -> u32 {
        self.width
    }
    fn get_height(&self) -> u32 {
        self.height
    }
    fn get_stride(&self) -> u32 {
        self.stride
    }
}

impl ImageCapture for WgcImageCapture {
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureResult, CaptureError> {
        let draw_mouse = matches!(request.cursor_mode, CursorCaptureMode::RenderInFrame);

        // Dynamic cursor-mode switch: cheap, only fires on change.
        if let Some(pipeline) = self.pipeline.as_ref() {
            let current = pipeline.session.IsCursorCaptureEnabled().unwrap_or(false);
            if current != draw_mouse {
                pipeline.session.SetIsCursorCaptureEnabled(draw_mouse)?;
            }
        }

        self.ensure_pipeline(draw_mouse)?;
        let pipeline = self.pipeline.as_mut().expect("pipeline ensured");

        // Block up to 500 ms for a fresh frame. Same wait budget as
        // DXGI's `AcquireNextFrame(500)` so back-pressure in the
        // shared capture loop stays consistent across backends.
        let wait_result = unsafe { WaitForSingleObject(pipeline.frame_event, 500) };
        if wait_result == WAIT_TIMEOUT {
            return Ok(CaptureResult {
                image: Box::new(EmptyImageInfo),
                cursor_update: None,
                content_changed: false,
                dirty_rects: Some(vec![]),
            });
        }
        if wait_result != WAIT_OBJECT_0 {
            return CaptureError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "[WGC] WaitForSingleObject returned unexpected status {:?}",
                    wait_result
                ),
            );
        }

        // TryGetNextFrame returns Result<Direct3D11CaptureFrame> in
        // windows-rs 0.61; there is no Option variant for "no frame
        // available", so any failure here is a real error.
        let frame: Direct3D11CaptureFrame = match pipeline.frame_pool.TryGetNextFrame() {
            Ok(f) => f,
            Err(e) => {
                if e.code() == DXGI_ERROR_DEVICE_REMOVED
                    || e.code() == E_ILLEGAL_STATE_CHANGE
                    || e.code().is_err()
                {
                    log::warn!(
                        "[WGC] TryGetNextFrame failed (will rebuild pipeline): code={:?} message={}",
                        e.code(),
                        e.message()
                    );
                    self.pipeline = None;
                    return CaptureError::custom_error(
                        DeskErrorCode::ACTION_NEED_RETRY,
                        &format!("[WGC] frame source lost, will retry: {}", e),
                    );
                }
                return Err(CaptureError::WindowsResultError(Backtrace::disabled(), e));
            }
        };

        // Handle resolution changes: drop the frame *before* recreating
        // the pool so we are not holding a buffer that belongs to the
        // old pool.
        let content_size = frame.ContentSize()?;
        if wgc_compose::frame_needs_resize(content_size, pipeline.staging_size) {
            let new_w = content_size.Width as u32;
            let new_h = content_size.Height as u32;
            log::info!(
                "[WGC] frame pool resize: {}x{} -> {}x{}",
                pipeline.staging_size.0,
                pipeline.staging_size.1,
                new_w,
                new_h
            );
            drop(frame);

            let dxgi_device: IDXGIDevice = self.manager.device.cast()?;
            let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)? };
            let d3d_device: IDirect3DDevice = inspectable.cast()?;
            pipeline.frame_pool.Recreate(
                &d3d_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                content_size,
            )?;
            pipeline.staging = Self::create_staging_texture(&self.manager.device, new_w, new_h)?;
            pipeline.staging_size = (new_w, new_h);
            // Keep `self.monitor_size` in lock-step with the
            // frame_pool dimensions: `capture_cursor_update` reads
            // it for `CursorSyncData.screen_width/height`, so a
            // stale value here makes the front-end mis-scale the
            // cursor sprite after a mid-session resize even though
            // the size-aware fingerprint forces re-emission.
            self.monitor_size = SizeInt32 {
                Width: new_w as i32,
                Height: new_h as i32,
            };
            // Defensive cache reset on the resize path; the
            // size-aware fingerprint already catches the dimension
            // change but resetting here keeps all backends'
            // rebuild branches symmetric.
            self.reset_cursor_cache();
            return Ok(CaptureResult {
                image: Box::new(EmptyImageInfo),
                cursor_update: None,
                content_changed: false,
                dirty_rects: Some(vec![]),
            });
        }

        // Pull the underlying D3D11 texture out of the WGC surface
        // and copy into our CPU-readable staging texture.
        let surface = frame.Surface()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
        let wgc_texture: ID3D11Texture2D = unsafe { access.GetInterface::<ID3D11Texture2D>()? };
        unsafe {
            self.manager
                .device_context
                .CopyResource(&pipeline.staging, &wgc_texture);
        }
        // Releasing the frame promptly lets WGC recycle the underlying
        // buffer back into the pool; deferred drop works too but burns
        // a pool slot longer than necessary.
        drop(frame);

        let resource: ID3D11Resource = pipeline.staging.cast()?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.manager
                .device_context
                .Map(&resource, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }
        let stride = mapped.RowPitch;
        let width = pipeline.staging_size.0;
        let height = pipeline.staging_size.1;
        let total = stride as usize * height as usize;
        let data = unsafe {
            // SAFETY: `mapped.pData` points to `total` valid bytes for
            // the lifetime of the Map; we copy them out immediately so
            // the slice does not outlive the Unmap (which happens in
            // WgcFrame's Drop, but we already copied by then).
            std::slice::from_raw_parts(mapped.pData as *const u8, total).to_vec()
        };
        // Build the frame holder so Drop runs Unmap on the same
        // resource handle.
        let wgc_frame = WgcFrame {
            resource,
            context: self.manager.device_context.clone(),
            data,
            width,
            height,
            stride,
        };

        // Cursor metadata for SyncNative consumers. RenderInFrame had
        // WGC bake the OS cursor; Disable / SyncNative still want the
        // cursor_update channel populated when the shape changes.
        let mut cursor_update = None;
        if matches!(request.cursor_mode, CursorCaptureMode::SyncNative) {
            match self.capture_cursor_update() {
                Ok(Some((fingerprint, data))) => {
                    if self.last_cursor_fingerprint != Some(fingerprint) {
                        self.last_cursor_fingerprint = Some(fingerprint);
                        cursor_update = Some(data);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    log::warn!("[WGC] cursor capture failed: {}", err);
                }
            }
        } else {
            self.last_cursor_fingerprint = None;
        }

        Ok(CaptureResult {
            image: Box::new(wgc_frame),
            cursor_update,
            content_changed: true,
            // WGC does not currently surface DirtyRegions in this
            // baseline; downstream encoder will do a full-frame YUV
            // conversion.
            dirty_rects: None,
        })
    }

    fn supports_cursor_sync(&self) -> bool {
        true
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        ImageCaptureType::WGC
    }

    fn get_current_output(&self) -> Result<DisplayInfo, CaptureError> {
        // Cached at construction; matches the shared capture loop's
        // "snapshot once" semantics (see `shared_capture::run_capture_loop`).
        Ok(self.display_info.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Size-aware fingerprint: a mid-session frame-pool resize
    /// (the `frame_pool.Recreate` path in `capture()`) updates
    /// `self.monitor_size` and thus the next fingerprint differs
    /// from the cached value even if the cursor pixel hash is
    /// unchanged. Guards against the regression where a stale
    /// `monitor_size` would cause `CursorSyncData.screen_width` to
    /// stay at the pre-resize value forever, breaking front-end
    /// cursor scaling.
    #[test]
    fn wgc_fingerprint_differs_on_screen_width_change() {
        let a = WgcCursorFingerprint::Shape {
            id: 0xcafe,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = WgcCursorFingerprint::Shape {
            id: 0xcafe,
            screen_width: 2560,
            screen_height: 1080,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn wgc_fingerprint_differs_on_screen_height_change() {
        let a = WgcCursorFingerprint::Shape {
            id: 0xcafe,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = WgcCursorFingerprint::Shape {
            id: 0xcafe,
            screen_width: 1920,
            screen_height: 1440,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn wgc_fingerprint_equal_when_all_fields_match() {
        let a = WgcCursorFingerprint::Shape {
            id: 0xcafe,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = WgcCursorFingerprint::Shape {
            id: 0xcafe,
            screen_width: 1920,
            screen_height: 1080,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn wgc_image_output_enumerator_constructor_reflects_is_supported() {
        // Most dev machines support WGC (Win10 1903+); skip the
        // negative branch — it is exercised on legacy CI runners.
        if let Ok(true) = GraphicsCaptureSession::IsSupported() {
            assert!(WgcImageOutputEnumerator::new().is_ok());
        }
    }

    /// Every GDI device name returned by the new EnumDisplayMonitors-
    /// backed enumerator must follow the documented `\\.\DISPLAYn`
    /// shape. The WGC capture-instance constructor will look up
    /// HMONITORs by exact-string match on this field, so a stray
    /// transformation (e.g. trimming the leading `\\?\`) would silently
    /// orphan every selection. Headless CI may legitimately return an
    /// empty list; the assertion only kicks in when at least one
    /// monitor is present.
    #[test]
    fn wgc_image_output_enumerator_returns_device_names_with_backslash_prefix() {
        // Skip on hosts where WGC itself is unsupported — the
        // enumerator runs through the GDI path regardless, but we
        // gate on the same precondition the production factory uses
        // to avoid spurious failures on non-WGC CI runners.
        if !matches!(GraphicsCaptureSession::IsSupported(), Ok(true)) {
            return;
        }
        let enumerator = WgcImageOutputEnumerator::new().expect("enumerator");
        let infos = enumerator.get_output_list().expect("get_output_list");
        for info in &infos {
            assert!(
                info.device_name.starts_with(r"\\.\"),
                "WGC enumerator yielded device_name without GDI prefix: {:?}",
                info.device_name
            );
        }
    }

    /// Hardware smoke: when an `lcxl` IDD virtual monitor is attached,
    /// the GDI-backed WGC enumerator must list it. WGC needs HMONITOR
    /// (which only `EnumDisplayMonitors` returns), so this test guards
    /// the GDI side of the cross-backend select-by-name contract from
    /// regressing. Ignored by default because it requires the
    /// production virtual-display driver to be loaded and bound — we
    /// run it manually via `cargo test -p desk-capture-engine -- \
    /// --ignored wgc_image_output_enumerator_includes_idd_when_attached`
    /// against a daemon-driven workstation.
    #[test]
    #[ignore]
    fn wgc_image_output_enumerator_includes_idd_when_attached() {
        let enumerator = WgcImageOutputEnumerator::new().expect("enumerator");
        let infos = enumerator.get_output_list().expect("get_output_list");
        let has_idd = infos.iter().any(|info| {
            info.display_device_name
                .as_deref()
                .map(|n| n.to_ascii_lowercase().contains("lcxl"))
                .unwrap_or(false)
        });
        assert!(
            has_idd,
            "expected at least one DisplayInfo whose display_device_name contains \
             'Lcxl' (the IDD virtual display); enumerated: {:?}",
            infos
                .iter()
                .map(|i| (i.device_name.clone(), i.display_device_name.clone()))
                .collect::<Vec<_>>()
        );
    }

    /// Hardware-touching integration smoke test. Ignored in CI (no GPU)
    /// but runnable locally with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn captures_a_frame_locally() {
        // Smoke needs a real device_name to bind the capture session.
        // The first entry from EnumDisplayMonitors is always present
        // on a desktop session.
        let infos = enum_display_infos().expect("enum_display_infos");
        let primary = infos
            .first()
            .expect("at least one display required for the smoke test");
        let settings = DeskSettings {
            image_capture: Some("WGC".into()),
            video_device_name: primary.device_name.clone(),
            ..Default::default()
        };
        let mut capture = WgcImageCapture::new(&settings).expect("WgcImageCapture::new");
        for _ in 0..30 {
            let r = capture
                .capture(CaptureRequest {
                    cursor_mode: CursorCaptureMode::SyncNative,
                })
                .expect("capture ok");
            if r.content_changed {
                assert!(r.image.get_width() > 0);
                assert!(r.image.get_height() > 0);
                assert!(!r.image.get_data().is_empty());
                return;
            }
        }
        panic!("did not receive any frame in 30 attempts");
    }
}
