//! `screen.capture.current` collector — one frame of the current display.
//!
//! Captures on the worker side (where the authoritative desktop frame lives,
//! per the frozen design) via the capture-engine factory, then encodes the
//! frame to PNG. Production dispatch first binds `params.display` to the exact
//! owner-selected `DeskSettings.video_device_name`; this collector therefore
//! always captures that already-validated configured output instead of letting
//! the model retarget it. A capture failure surfaces as an error rather than
//! silently degrading, since a capture request implies a desktop is present.

use std::io::Cursor;

use desk_agent_protocol::{
    AgentError, AgentErrorKind, ImageFormat, ScreenCaptureOutput, ScreenCaptureParams,
};
use desk_capture_engine::error::CaptureError;
use desk_capture_engine::image_capture::image_capture_factory::create_image_capture;
use desk_capture_engine::model::image_capture::{
    CaptureRequest, CursorCaptureMode, ImageInfo, ImageType,
};
use desk_signal_facade::model::desk_settings::DeskSettings;

/// Maximum encoded image size returned to the daemon. The daemon ↔ worker
/// event pipe frames at 16 MiB (`desk_ipc_protocol` transport); a frame over
/// that limit fails to send and tears down the event writer, breaking all
/// subsequent worker replies. So an oversized capture is reported as
/// `OutputLimitExceeded` rather than shipped — the cap sits well under the
/// frame limit to leave room for the rest of the response envelope. A
/// downscaling / re-quality path that keeps a usable image can replace the
/// hard error later.
const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;

/// Capture one frame of the current display and return it as PNG.
pub fn collect(
    params: &ScreenCaptureParams,
    desk_settings: &DeskSettings,
) -> Result<ScreenCaptureOutput, AgentError> {
    // The broker already proved this optional value equals the configured
    // owner-selected display. Capture-engine consumes the configured target.
    let _ = &params.display;

    let mut capture = create_image_capture(desk_settings).map_err(capture_err)?;
    let result = capture
        .capture(CaptureRequest {
            cursor_mode: CursorCaptureMode::RenderInFrame,
        })
        .map_err(capture_err)?;

    let frame = result.image;
    let width = frame.get_width();
    let height = frame.get_height();
    let (dpi_x, dpi_y) = capture_dpi();
    let png = encode_png(frame.as_ref())?;
    enforce_size_limit(png.len(), MAX_IMAGE_BYTES)?;

    Ok(ScreenCaptureOutput {
        display: desk_settings.video_device_name.clone(),
        format: ImageFormat::Png,
        width,
        height,
        dpi_x,
        dpi_y,
        image: png,
        // The frame is returned whole; if it had exceeded the limit the call
        // would have errored above rather than shipping a partial image.
        truncated: false,
    })
}

#[cfg(windows)]
fn capture_dpi() -> (u32, u32) {
    use windows::Win32::UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::GetForegroundWindow};

    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return (96, 96);
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { (96, 96) } else { (dpi, dpi) }
}

#[cfg(not(windows))]
fn capture_dpi() -> (u32, u32) {
    // The Windows raw-input beta is the only consumer today. Other platform
    // capture adapters still return an explicit neutral value instead of
    // omitting the coordinate-space fact from the current schema.
    (96, 96)
}

/// Reject an encoded image that would overflow the IPC frame. A truncated image
/// is corrupt and useless, so the limit is a hard `OutputLimitExceeded` rather
/// than a `truncated` flag.
fn enforce_size_limit(len: usize, max: usize) -> Result<(), AgentError> {
    if len > max {
        return Err(AgentError {
            kind: AgentErrorKind::OutputLimitExceeded,
            message: format!("captured image is {len} bytes, over the {max} byte transport limit"),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
    }
    Ok(())
}

fn capture_err(err: CaptureError) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("screen capture failed: {err}"),
        retryable: true,
        safe_for_model: true,
        error_code: None,
    }
}

fn internal(message: &str) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: message.to_string(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// Encode a captured frame to PNG, normalizing the source pixels (BGRA or RGB,
/// with a possibly-padded row stride) to opaque RGBA. Desktop frames are
/// opaque, so the alpha channel is forced to 255.
fn encode_png(frame: &(dyn ImageInfo + Send + Sync)) -> Result<Vec<u8>, AgentError> {
    let width = frame.get_width();
    let height = frame.get_height();
    if width == 0 || height == 0 {
        return Err(internal("captured frame has zero dimensions"));
    }

    let stride = frame.get_stride() as usize;
    let data = frame.get_data();
    let (bytes_per_pixel, swap_rb) = match frame.get_type() {
        ImageType::BGRA => (4usize, true),
        ImageType::RGB => (3usize, false),
    };
    let row_bytes = width as usize * bytes_per_pixel;

    // A buffer shorter than its reported dimensions is a capture bug; fail
    // rather than slice out of bounds.
    let min_len = (height as usize - 1) * stride + row_bytes;
    if data.len() < min_len {
        return Err(internal(
            "captured frame buffer is smaller than its reported dimensions",
        ));
    }

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height as usize {
        let row = &data[y * stride..y * stride + row_bytes];
        for pixel in row.chunks_exact(bytes_per_pixel) {
            if swap_rb {
                rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
            } else {
                rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
            }
        }
    }

    let buffer = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| internal("failed to build RGBA image buffer"))?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(buffer)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|e| internal(&format!("PNG encode failed: {e}")))?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `ImageInfo` over an in-memory buffer for encoder tests.
    struct FakeFrame {
        data: Vec<u8>,
        width: u32,
        height: u32,
        image_type: ImageType,
        stride: u32,
    }

    impl ImageInfo for FakeFrame {
        fn get_type(&self) -> ImageType {
            self.image_type
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

    #[test]
    fn encodes_bgra_with_padded_stride_to_png() {
        // 2x1 BGRA with 4 bytes of row padding: pixels B,G,R = (10,20,30)
        // and (40,50,60); alpha bytes are ignored (forced opaque).
        let data = vec![
            10, 20, 30, 0, // px0 BGRA
            40, 50, 60, 0, // px1 BGRA
            99, 99, 99, 99, // row padding (stride = 12)
        ];
        let frame = FakeFrame {
            data,
            width: 2,
            height: 1,
            image_type: ImageType::BGRA,
            stride: 12,
        };
        let png = encode_png(&frame).expect("encode");
        // PNG magic.
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);

        // Round-trip decode and verify BGRA→RGBA swap + opaque alpha.
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [30, 20, 10, 255]);
        assert_eq!(decoded.get_pixel(1, 0).0, [60, 50, 40, 255]);
    }

    #[test]
    fn encodes_rgb_to_png() {
        let frame = FakeFrame {
            data: vec![1, 2, 3, 4, 5, 6], // 2x1 RGB, tight stride
            width: 2,
            height: 1,
            image_type: ImageType::RGB,
            stride: 6,
        };
        let png = encode_png(&frame).expect("encode");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert_eq!(decoded.get_pixel(0, 0).0, [1, 2, 3, 255]);
        assert_eq!(decoded.get_pixel(1, 0).0, [4, 5, 6, 255]);
    }

    #[test]
    fn rejects_undersized_buffer() {
        let frame = FakeFrame {
            data: vec![0; 4], // claims 4x4 but holds one pixel
            width: 4,
            height: 4,
            image_type: ImageType::BGRA,
            stride: 16,
        };
        let err = encode_png(&frame).expect_err("must reject");
        assert_eq!(err.kind, AgentErrorKind::Internal);
    }

    #[test]
    fn size_limit_rejects_oversized_image() {
        // Under / at the limit pass; over the limit is OutputLimitExceeded.
        assert!(enforce_size_limit(100, 128).is_ok());
        assert!(enforce_size_limit(128, 128).is_ok());
        let err = enforce_size_limit(129, 128).expect_err("must reject");
        assert_eq!(err.kind, AgentErrorKind::OutputLimitExceeded);
        assert!(!err.retryable);
    }

    #[test]
    fn rejects_zero_dimensions() {
        let frame = FakeFrame {
            data: Vec::new(),
            width: 0,
            height: 0,
            image_type: ImageType::BGRA,
            stride: 0,
        };
        assert!(encode_png(&frame).is_err());
    }

    /// Live capture on desktop targets: must not panic and, when it succeeds,
    /// returns a real PNG with non-zero dimensions. Tolerates failure on
    /// headless or permission-denied hosts.
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    #[cfg_attr(
        target_os = "macos",
        ignore = "requires an Aqua session and Screen Recording permission"
    )]
    fn live_capture_is_ok_or_errors_gracefully() {
        match collect(&ScreenCaptureParams::default(), &DeskSettings::default()) {
            Ok(out) => {
                assert_eq!(out.format, ImageFormat::Png);
                assert!(out.width > 0 && out.height > 0);
                assert_eq!(&out.image[..4], &[0x89, b'P', b'N', b'G']);
            }
            Err(e) => assert_eq!(e.kind, AgentErrorKind::Internal),
        }
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an active interactive Windows display"]
    fn live_windows_capture_requires_a_real_png() {
        let display = std::env::var("LRD_SCREEN_CAPTURE_TEST_DISPLAY")
            .expect("LRD_SCREEN_CAPTURE_TEST_DISPLAY must name the owner-selected display");
        let settings = DeskSettings {
            video_device_name: display,
            // The development VM exposes Microsoft Basic Render Driver; its
            // DXGI duplication device can be suspended even while the real
            // interactive desktop is available. GDI is the production
            // capture-engine fallback that exercises the same owner-selected
            // display contract without depending on that virtual GPU path.
            image_capture: Some("GDI".to_string()),
            ..Default::default()
        };
        let out =
            collect(&ScreenCaptureParams::default(), &settings).expect("Windows screen capture");
        assert_eq!(out.format, ImageFormat::Png);
        assert!(out.width > 0 && out.height > 0);
        assert_eq!(&out.image[..4], &[0x89, b'P', b'N', b'G']);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires an Aqua session with Screen Recording permission"]
    fn live_macos_capture_requires_a_real_png() {
        let display_id = core_graphics::display::CGDisplay::main().id;
        assert_ne!(display_id, 0, "macOS must expose a main display");
        let settings = DeskSettings {
            video_device_name: display_id.to_string(),
            ..Default::default()
        };
        let out =
            collect(&ScreenCaptureParams::default(), &settings).expect("macOS screen capture");
        assert_eq!(out.format, ImageFormat::Png);
        assert!(out.width > 0 && out.height > 0);
        assert_eq!(&out.image[..4], &[0x89, b'P', b'N', b'G']);
    }
}
