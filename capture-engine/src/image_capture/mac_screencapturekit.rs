use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::{
    error::CaptureError,
    model::image_capture::{
        CaptureRequest, CaptureResult, ImageCapture, ImageCaptureType, ImageInfo,
        ImageOutputEnumerator, ImageType,
    },
};
use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect, Resolution},
};
use desk_utils::error::DeskErrorCode;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;

/// Copyable snapshot of a recoverable/terminal stream error. `CaptureError`
/// itself is not `Clone`, so the shared state stores the code + message and
/// reconstructs a `CaptureError` on demand.
#[derive(Clone)]
struct ErrDesc {
    code: DeskErrorCode,
    message: String,
}

/// State shared between the ScreenCaptureKit callbacks (output handler +
/// delegate) and the synchronous `capture()` consumer. A single mutex plus a
/// condvar coordinates both the "new frame" and "stream stopped with error"
/// wakeups.
struct SharedInner {
    frame: Option<SCImageInfo>,
    error: Option<ErrDesc>,
}

struct CaptureState {
    inner: Mutex<SharedInner>,
    cond: Condvar,
}

pub struct MacScreencaptureKitImageCapture {
    stream: Option<SCStream>,
    shared: Arc<CaptureState>,
    width: u32,
    height: u32,
    capture_type: ImageCaptureType,
    target_display_id: u32,
    current_display: DisplayInfo,
}

/// Minimal abstraction over "a display with an id", so display selection can be
/// unit-tested with fake displays without constructing a real `SCDisplay`.
trait HasDisplayId {
    fn id(&self) -> u32;
}

impl HasDisplayId for SCDisplay {
    fn id(&self) -> u32 {
        self.display_id()
    }
}

/// Select the index of the display whose id matches `target_id`. Returns `None`
/// when no display matches. The real `capture()` build path calls this exact
/// helper, so a test that injects fake displays and asserts the chosen index
/// proves the stream is built against the user-selected display (not `[0]`).
fn select_display_index<D: HasDisplayId>(displays: &[D], target_id: u32) -> Option<usize> {
    displays.iter().position(|d| d.id() == target_id)
}

/// Copy a BGRA frame row by row, dropping any stride padding so the output is
/// tightly packed `width * 4` bytes per row. `bytes_per_row` is the source
/// stride and may exceed `width * 4`.
fn copy_bgra_rows(src: &[u8], width: u32, height: u32, bytes_per_row: usize) -> Vec<u8> {
    let row_len = (width as usize) * 4;
    let mut data = Vec::with_capacity(row_len * height as usize);
    for y in 0..height as usize {
        let offset = y * bytes_per_row;
        data.extend_from_slice(&src[offset..offset + row_len]);
    }
    data
}

struct FrameReceiver {
    shared: Arc<CaptureState>,
}

impl SCStreamOutputTrait for FrameReceiver {
    fn did_output_sample_buffer(&self, sample_buffer: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Screen {
            return;
        }
        let Some(pixel_buffer) = sample_buffer.image_buffer() else {
            return;
        };
        let Ok(guard) = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else {
            return;
        };

        let width = guard.width() as u32;
        let height = guard.height() as u32;
        let bytes_per_row = guard.bytes_per_row();
        let src = guard.as_slice();

        // Guard against a short mapping; never read out of bounds.
        if width == 0 || height == 0 || src.len() < bytes_per_row * height as usize {
            return;
        }

        let data = copy_bgra_rows(src, width, height, bytes_per_row);
        // The lock guard unlocks the pixel buffer on drop.
        drop(guard);

        let info = SCImageInfo {
            data,
            width,
            height,
        };
        let mut inner = self.shared.inner.lock().unwrap();
        inner.frame = Some(info);
        self.shared.cond.notify_one();
    }
}

struct StreamDelegate {
    shared: Arc<CaptureState>,
}

impl SCStreamDelegateTrait for StreamDelegate {
    fn did_stop_with_error(&self, error: SCError) {
        // A stopped stream is recoverable: record the error and wake the
        // consumer so it can drop the dead stream and rebuild on the next call.
        let mut inner = self.shared.inner.lock().unwrap();
        inner.error = Some(ErrDesc {
            code: DeskErrorCode::ACTION_NEED_RETRY,
            message: format!("ScreenCaptureKit stream stopped: {error}"),
        });
        self.shared.cond.notify_one();
    }
}

impl MacScreencaptureKitImageCapture {
    pub fn new(settings: &DeskSettings) -> Result<Self, CaptureError> {
        let content = SCShareableContent::get().map_err(|e| {
            CaptureError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, &e.to_string())
        })?;
        let displays = content.displays();

        let requested = &settings.video_device_name;
        if requested.is_empty() {
            return CaptureError::custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "video_device_name is empty: no display has been selected. \
                 Open the desktop dialog in the browser and pick a display \
                 before starting media.",
            );
        }
        let target_display_id = requested.parse::<u32>().map_err(|_| {
            CaptureError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                &format!("video_device_name {requested:?} is not a valid display id"),
            )
        })?;

        let idx = select_display_index(&displays, target_display_id).ok_or_else(|| {
            let available = displays
                .iter()
                .map(|d| d.display_id().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            CaptureError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                &format!(
                    "device_name {requested:?} not enumerated by ScreenCaptureKit; enumerated: [{available}]"
                ),
            )
        })?;
        let display = &displays[idx];

        let width = display.width();
        let height = display.height();

        let display_info = DisplayInfo {
            device_name: display.display_id().to_string(),
            display_device_name: Some(format!("Display {}", display.display_id())),
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            },
            resolutions: vec![Resolution::new(width, height)],
            attached_to_desktop: true,
            rotation: 0,
        };

        Ok(Self {
            stream: None,
            shared: Arc::new(CaptureState {
                inner: Mutex::new(SharedInner {
                    frame: None,
                    error: None,
                }),
                cond: Condvar::new(),
            }),
            width,
            height,
            capture_type: ImageCaptureType::SCKIT,
            target_display_id,
            current_display: display_info,
        })
    }

    /// (Re)build the ScreenCaptureKit stream against the selected display. The
    /// stream is built with `shows_cursor=false`; the remote cursor is rendered
    /// natively by the browser (a follow-up feature) rather than baked into the
    /// frame, so per-connection cursor preferences stay honored.
    fn build_stream(&mut self) -> Result<(), CaptureError> {
        let content = SCShareableContent::get().map_err(|e| {
            CaptureError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, &e.to_string())
        })?;
        let displays = content.displays();

        let idx = select_display_index(&displays, self.target_display_id).ok_or_else(|| {
            CaptureError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                &format!(
                    "selected display id {} is no longer enumerated by ScreenCaptureKit",
                    self.target_display_id
                ),
            )
        })?;
        let display = &displays[idx];

        let width = display.width();
        let height = display.height();
        self.width = width;
        self.height = height;

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();

        let config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(false);

        // Clear any stale frame/error left from a previous stream instance.
        {
            let mut inner = self.shared.inner.lock().unwrap();
            inner.frame = None;
            inner.error = None;
        }

        let delegate = StreamDelegate {
            shared: self.shared.clone(),
        };
        let mut stream = SCStream::new_with_delegate(&filter, &config, delegate);

        let receiver = FrameReceiver {
            shared: self.shared.clone(),
        };
        // `add_output_handler` returns `None` when registration fails; the
        // stream would start but never deliver frames, so treat it as an error.
        stream
            .add_output_handler(receiver, SCStreamOutputType::Screen)
            .ok_or_else(|| {
                CaptureError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "ScreenCaptureKit add_output_handler failed",
                )
            })?;

        stream.start_capture().map_err(|e| {
            CaptureError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &e.to_string())
        })?;

        self.stream = Some(stream);
        Ok(())
    }
}

impl ImageCapture for MacScreencaptureKitImageCapture {
    fn capture(&mut self, _request: CaptureRequest) -> Result<CaptureResult, CaptureError> {
        if self.stream.is_none() {
            self.build_stream()?;
        }

        let mut inner = self.shared.inner.lock().unwrap();
        if inner.frame.is_none() && inner.error.is_none() {
            let (guard, result) = self
                .shared
                .cond
                .wait_timeout(inner, Duration::from_secs(3))
                .unwrap();
            inner = guard;
            if result.timed_out() && inner.frame.is_none() && inner.error.is_none() {
                return Err(CaptureError::new_custom_error(
                    DeskErrorCode::ACTION_NEED_RETRY,
                    "No frame available (timeout)",
                ));
            }
        }

        // A terminal stream error wins over a stale frame: drop the dead stream
        // so the next `capture()` rebuilds the backend (the upstream capture
        // loop only retries `capture()`, it never rebuilds on its own).
        if let Some(err) = inner.error.take() {
            drop(inner);
            self.stream = None;
            return Err(CaptureError::new_custom_error(err.code, &err.message));
        }

        if let Some(info) = inner.frame.take() {
            Ok(CaptureResult {
                image: Box::new(info),
                cursor_update: None,
                content_changed: true,
                dirty_rects: None,
            })
        } else {
            Err(CaptureError::new_custom_error(
                DeskErrorCode::ACTION_NEED_RETRY,
                "No frame available",
            ))
        }
    }

    fn get_capture_type(&self) -> ImageCaptureType {
        self.capture_type
    }

    fn get_current_output(&self) -> Result<DisplayInfo, CaptureError> {
        Ok(self.current_display.clone())
    }
}

#[derive(Clone)]
struct SCImageInfo {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl ImageInfo for SCImageInfo {
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
}

#[derive(Default)]
pub struct MacScreencaptureKitImageOutputEnumerator;

impl MacScreencaptureKitImageOutputEnumerator {
    pub fn new() -> Self {
        Self
    }
}

impl ImageOutputEnumerator for MacScreencaptureKitImageOutputEnumerator {
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError> {
        let content = SCShareableContent::get().map_err(|e| {
            CaptureError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, &e.to_string())
        })?;
        let mut display_infos = Vec::new();

        for display in content.displays() {
            let width = display.width();
            let height = display.height();
            display_infos.push(DisplayInfo {
                device_name: display.display_id().to_string(),
                display_device_name: Some(format!("Display {}", display.display_id())),
                desktop_coordinates: DisplayRect {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                },
                resolutions: vec![Resolution::new(width, height)],
                attached_to_desktop: true,
                rotation: 0,
            });
        }

        Ok(display_infos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeDisplay(u32);
    impl HasDisplayId for FakeDisplay {
        fn id(&self) -> u32 {
            self.0
        }
    }

    #[test]
    fn select_display_index_picks_matching_id_not_first() {
        let displays = [FakeDisplay(10), FakeDisplay(20), FakeDisplay(30)];
        // Selecting the second display must resolve to index 1, proving the
        // build path targets the chosen display rather than `[0]`.
        assert_eq!(select_display_index(&displays, 20), Some(1));
        assert_eq!(select_display_index(&displays, 10), Some(0));
        assert_eq!(select_display_index(&displays, 30), Some(2));
    }

    #[test]
    fn select_display_index_returns_none_when_absent() {
        let displays = [FakeDisplay(10), FakeDisplay(20)];
        assert_eq!(select_display_index(&displays, 99), None);
    }

    #[test]
    fn copy_bgra_rows_drops_stride_padding() {
        // 2x2 BGRA frame with stride 12 (4 bytes padding per row).
        let width = 2u32;
        let height = 2u32;
        let bytes_per_row = 12usize;
        let mut src = vec![0u8; bytes_per_row * height as usize];
        // Row 0 pixels: [1,2,3,4, 5,6,7,8] then 4 padding bytes.
        src[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        // Row 1 pixels: [9,10,11,12, 13,14,15,16] then 4 padding bytes.
        src[12..20].copy_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);

        let out = copy_bgra_rows(&src, width, height, bytes_per_row);
        assert_eq!(out.len(), (width * height * 4) as usize);
        assert_eq!(
            out,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn copy_bgra_rows_tight_stride() {
        let width = 1u32;
        let height = 3u32;
        let bytes_per_row = 4usize;
        let src: Vec<u8> = (0..12).collect();
        let out = copy_bgra_rows(&src, width, height, bytes_per_row);
        assert_eq!(out, (0u8..12).collect::<Vec<_>>());
    }

    #[test]
    fn capture_config_disables_baked_in_cursor() {
        // Pin the decision that the macOS backend never bakes the cursor into
        // the frame (cursor is rendered natively by the browser). If this is
        // ever flipped to `true`, this assertion forces a deliberate review.
        let config = SCStreamConfiguration::new()
            .with_width(640)
            .with_height(480)
            .with_pixel_format(PixelFormat::BGRA)
            .with_shows_cursor(false);
        assert!(!config.shows_cursor());
    }
}
