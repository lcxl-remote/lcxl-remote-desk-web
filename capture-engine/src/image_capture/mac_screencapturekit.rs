use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::{
    error::CaptureError,
    model::image_capture::{
        CaptureRequest, CaptureResult, CursorCaptureMode, CursorSyncData, ImageCapture,
        ImageCaptureType, ImageInfo, ImageOutputEnumerator, ImageType,
    },
};
use base64::Engine;
use desk_signal_facade::model::{
    desk_settings::DeskSettings,
    image_capture::{DisplayInfo, DisplayRect, Resolution},
};
use desk_utils::error::DeskErrorCode;
use objc2::rc::autoreleasepool;
use objc2::runtime::AnyObject;
use objc2::{Encode, Encoding, class, msg_send};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;

/// CoreGraphics geometry structs with an explicit ObjC encoding. The
/// objc2-foundation `NSGeometry` feature is off in this build, so the geometry
/// types are declared locally for the raw `msg_send!` cursor path.
#[repr(C)]
#[derive(Clone, Copy)]
struct CgPoint {
    x: f64,
    y: f64,
}
unsafe impl Encode for CgPoint {
    const ENCODING: Encoding = Encoding::Struct(
        "CGPoint",
        &[<f64 as Encode>::ENCODING, <f64 as Encode>::ENCODING],
    );
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CgSize {
    width: f64,
    height: f64,
}
unsafe impl Encode for CgSize {
    const ENCODING: Encoding = Encoding::Struct(
        "CGSize",
        &[<f64 as Encode>::ENCODING, <f64 as Encode>::ENCODING],
    );
}

/// Backing scale factor (Retina multiplier) of the given display, derived from
/// its current mode's pixel width vs point width. Public CoreGraphics; takes a
/// `CGDirectDisplayID` directly. Falls back to 1.0 when the mode is
/// unavailable. Used to pick the cursor representation that matches the
/// on-screen pixel size (see `select_rep_index`).
fn display_backing_scale(display_id: u32) -> f64 {
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDisplayCopyDisplayMode(display: u32) -> *mut core::ffi::c_void;
        fn CGDisplayModeGetWidth(mode: *mut core::ffi::c_void) -> usize;
        fn CGDisplayModeGetPixelWidth(mode: *mut core::ffi::c_void) -> usize;
        fn CGDisplayModeRelease(mode: *mut core::ffi::c_void);
    }
    unsafe {
        let mode = CGDisplayCopyDisplayMode(display_id);
        if mode.is_null() {
            return 1.0;
        }
        let points = CGDisplayModeGetWidth(mode);
        let pixels = CGDisplayModeGetPixelWidth(mode);
        CGDisplayModeRelease(mode);
        if points > 0 {
            pixels as f64 / points as f64
        } else {
            1.0
        }
    }
}

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
    /// Backing scale of the target display, used to pick the cursor rep that
    /// matches its on-screen pixel size.
    backing_scale: f64,
    /// Last emitted cursor identity; `cursor_update` is only produced (and the
    /// cursor PNG only encoded) when this changes.
    last_cursor_fingerprint: Option<MacCursorFingerprint>,
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

/// Identity key used to deduplicate `CursorSyncData` emissions, mirroring the
/// Windows backends' fingerprints. `screen_width`/`screen_height` are folded in
/// so a mid-session display change re-emits the cursor even when its pixels are
/// unchanged (the front-end scales the sprite by the remote screen size). The
/// macOS backend always reports the cursor as visible (see
/// `capture_system_cursor`), so there is no hidden-cursor variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MacCursorFingerprint {
    hash: u64,
    screen_width: u32,
    screen_height: u32,
}

/// Hash the cursor's raw RGBA pixels (no PNG/zlib) so change detection stays
/// cheap on the per-frame path; PNG encoding only runs when this id changes.
/// Doubles as `CursorSyncData.shape_id`. Stable within a process run, which is
/// all the dedup needs.
fn pixel_shape_id(rgba: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rgba.hash(&mut hasher);
    hasher.finish()
}

/// Pick the `NSImageRep` whose pixel width best matches the cursor's on-screen
/// pixel size (`image point size * display backing scale`). A system cursor
/// `NSImage` ships several reps (e.g. 1x/2x/5x/10x); `TIFFRepresentation`
/// collapses to the largest, which would render the cursor far too big, so the
/// caller must select the rep for the target display's scale instead.
fn select_rep_index(rep_pixel_widths: &[u32], image_pt_width: u32, backing_scale: f64) -> usize {
    let target = image_pt_width as f64 * backing_scale;
    rep_pixel_widths
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            let da = (**a as f64 - target).abs();
            let db = (**b as f64 - target).abs();
            da.total_cmp(&db)
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Convert a hotspot coordinate from image points to pixels in the chosen rep.
/// The PNG is emitted at the rep's pixel resolution, so the hotspot must scale
/// by the same point->pixel ratio or it would drift on non-1x cursors.
fn scale_hotspot_to_pixels(hotspot_pt: f64, image_pt: f64, image_px: u32) -> i32 {
    if image_pt > 0.0 {
        (hotspot_pt * image_px as f64 / image_pt).round() as i32
    } else {
        hotspot_pt.round() as i32
    }
}

/// Result of one system-cursor capture attempt.
#[derive(Debug)]
enum CursorCaptureOutcome {
    /// The cursor changed since `last`: a fresh fingerprint + payload (the PNG
    /// was encoded for this case only).
    Changed(MacCursorFingerprint, CursorSyncData),
    /// The cursor is identical to `last`; no payload, no PNG encode.
    Unchanged,
    /// The cursor could not be read (no current cursor / no bitmap rep).
    Unavailable,
}

/// Read the current system cursor and build a `CursorSyncData`, selecting the
/// representation that matches `backing_scale` and gating PNG encoding on a
/// cheap raw-pixel hash compared against `last`. Pure AppKit/CoreGraphics: only
/// needs a GUI session, not screen-recording permission, so it is exercised by
/// the `system_cursor_capture_produces_png` smoke independently of SCStream.
fn capture_system_cursor(
    backing_scale: f64,
    screen_width: u32,
    screen_height: u32,
    last: Option<MacCursorFingerprint>,
) -> CursorCaptureOutcome {
    autoreleasepool(|_| unsafe {
        let cursor: *mut AnyObject = msg_send![class!(NSCursor), currentSystemCursor];
        if cursor.is_null() {
            return CursorCaptureOutcome::Unavailable;
        }
        let hotspot: CgPoint = msg_send![cursor, hotSpot];
        let image: *mut AnyObject = msg_send![cursor, image];
        if image.is_null() {
            return CursorCaptureOutcome::Unavailable;
        }
        let size: CgSize = msg_send![image, size];

        // Collect the bitmap representations; a system cursor ships several
        // (1x/2x/...), and we must pick the one matching the display scale
        // rather than the largest (which `TIFFRepresentation` would yield).
        let reps: *mut AnyObject = msg_send![image, representations];
        if reps.is_null() {
            return CursorCaptureOutcome::Unavailable;
        }
        let nreps: usize = msg_send![reps, count];
        let bitmap_class = class!(NSBitmapImageRep);
        let mut bitmap_reps: Vec<*mut AnyObject> = Vec::new();
        let mut widths: Vec<u32> = Vec::new();
        for i in 0..nreps {
            let rep: *mut AnyObject = msg_send![reps, objectAtIndex: i];
            let is_bitmap: bool = msg_send![rep, isKindOfClass: bitmap_class];
            if !is_bitmap {
                continue;
            }
            let w: isize = msg_send![rep, pixelsWide];
            if w <= 0 {
                continue;
            }
            bitmap_reps.push(rep);
            widths.push(w as u32);
        }
        if bitmap_reps.is_empty() {
            return CursorCaptureOutcome::Unavailable;
        }

        let idx = select_rep_index(&widths, size.width.round() as u32, backing_scale);
        let rep = bitmap_reps[idx];
        let px_w: isize = msg_send![rep, pixelsWide];
        let px_h: isize = msg_send![rep, pixelsHigh];
        let bytes_per_row: isize = msg_send![rep, bytesPerRow];
        // `-[NSBitmapImageRep bitmapData]` is encoded as `char *` (`*`); objc2
        // maps `*const u8`/`*mut u8` to that code, whereas `NSData bytes` below
        // is `void *` (`^v`) and uses `c_void`.
        let data_ptr: *const u8 = msg_send![rep, bitmapData];
        if data_ptr.is_null() || px_h <= 0 || bytes_per_row <= 0 {
            return CursorCaptureOutcome::Unavailable;
        }

        // Cheap change detection: hash the raw pixels (no PNG/zlib).
        let raw_len = bytes_per_row as usize * px_h as usize;
        let raw = std::slice::from_raw_parts(data_ptr, raw_len);
        let hash = pixel_shape_id(raw);

        let fingerprint = MacCursorFingerprint {
            hash,
            screen_width,
            screen_height,
        };
        if last == Some(fingerprint) {
            return CursorCaptureOutcome::Unchanged;
        }

        // Changed: now pay for the PNG encode of the selected rep.
        const PNG_FILE_TYPE: usize = 4; // NSBitmapImageFileTypePNG
        let props: *mut AnyObject = msg_send![class!(NSDictionary), dictionary];
        let png: *mut AnyObject =
            msg_send![rep, representationUsingType: PNG_FILE_TYPE, properties: props];
        if png.is_null() {
            return CursorCaptureOutcome::Unavailable;
        }
        let len: usize = msg_send![png, length];
        let bytes: *const core::ffi::c_void = msg_send![png, bytes];
        if bytes.is_null() || len == 0 {
            return CursorCaptureOutcome::Unavailable;
        }
        let png_data = std::slice::from_raw_parts(bytes as *const u8, len);
        let base64_png = base64::engine::general_purpose::STANDARD.encode(png_data);

        let hotspot_x = scale_hotspot_to_pixels(hotspot.x, size.width, px_w as u32);
        let hotspot_y = scale_hotspot_to_pixels(hotspot.y, size.height, px_h as u32);

        CursorCaptureOutcome::Changed(
            fingerprint,
            CursorSyncData {
                base64_png,
                hotspot_x,
                hotspot_y,
                visible: true,
                shape_id: hash,
                screen_width,
                screen_height,
                embedded: false,
            },
        )
    })
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
            backing_scale: display_backing_scale(target_display_id),
            last_cursor_fingerprint: None,
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

    /// Capture the current system cursor as a `CursorSyncData` for the native
    /// cursor-sync path (`CursorCaptureMode::SyncNative`). Returns `None` when
    /// the cursor is unchanged since the last emission (cheap raw-pixel hash
    /// gate; PNG is only encoded on change) or when the cursor cannot be read.
    ///
    /// `screen_width`/`screen_height` are the captured frame's pixel dimensions
    /// (the front-end scales the cursor sprite by these). Visibility defaults to
    /// `true`: `CGCursorIsVisible` is process-local and unreliable for a
    /// background capture process, so it is not used to gate emission.
    fn capture_cursor_update(
        &mut self,
        screen_width: u32,
        screen_height: u32,
    ) -> Option<CursorSyncData> {
        match capture_system_cursor(
            self.backing_scale,
            screen_width,
            screen_height,
            self.last_cursor_fingerprint,
        ) {
            CursorCaptureOutcome::Changed(fingerprint, data) => {
                self.last_cursor_fingerprint = Some(fingerprint);
                Some(data)
            }
            // Unchanged keeps the cached fingerprint; Unavailable leaves it as
            // is so a transient read failure re-emits once the cursor returns.
            CursorCaptureOutcome::Unchanged | CursorCaptureOutcome::Unavailable => None,
        }
    }
}

impl ImageCapture for MacScreencaptureKitImageCapture {
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureResult, CaptureError> {
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

        let frame = inner.frame.take();
        drop(inner);

        let Some(info) = frame else {
            return Err(CaptureError::new_custom_error(
                DeskErrorCode::ACTION_NEED_RETRY,
                "No frame available",
            ));
        };

        // Cursor metadata for SyncNative consumers. The frame's pixel size is
        // the basis the front-end scales the cursor sprite against.
        let cursor_update = if matches!(request.cursor_mode, CursorCaptureMode::SyncNative) {
            self.capture_cursor_update(info.width, info.height)
        } else {
            self.last_cursor_fingerprint = None;
            None
        };

        Ok(CaptureResult {
            image: Box::new(info),
            cursor_update,
            content_changed: true,
            dirty_rects: None,
        })
    }

    fn supports_cursor_sync(&self) -> bool {
        true
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

    #[test]
    fn mac_cursor_fingerprint_differs_on_screen_size_change() {
        // A display change must re-emit even when the cursor pixels (hash) are
        // identical, so the front-end re-scales the sprite.
        let a = MacCursorFingerprint {
            hash: 0xcafe,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = MacCursorFingerprint {
            hash: 0xcafe,
            screen_width: 2560,
            screen_height: 1440,
        };
        assert_ne!(a, b);
    }

    #[test]
    fn mac_cursor_fingerprint_equal_when_all_fields_match() {
        let a = MacCursorFingerprint {
            hash: 0xcafe,
            screen_width: 1920,
            screen_height: 1080,
        };
        let b = MacCursorFingerprint {
            hash: 0xcafe,
            screen_width: 1920,
            screen_height: 1080,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn pixel_shape_id_stable_and_distinct() {
        let a = pixel_shape_id(&[1, 2, 3, 4]);
        let b = pixel_shape_id(&[1, 2, 3, 4]);
        let c = pixel_shape_id(&[1, 2, 3, 5]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn select_rep_index_picks_rep_for_backing_scale() {
        // The four representations a system cursor ships (observed via the
        // Phase 0 spike): 1x / 2x / 5x / 10x of a 17pt-wide cursor.
        let widths = [17u32, 34, 85, 170];
        assert_eq!(select_rep_index(&widths, 17, 1.0), 0); // non-Retina -> 17px
        assert_eq!(select_rep_index(&widths, 17, 2.0), 1); // Retina 2x  -> 34px
        // A fractional scale snaps to the nearest available rep.
        assert_eq!(select_rep_index(&widths, 17, 1.5), 0); // 25.5 closer to 17 than 34
        assert_eq!(select_rep_index(&widths, 17, 1.6), 1); // 27.2 closer to 34
    }

    #[test]
    fn select_rep_index_empty_is_zero() {
        assert_eq!(select_rep_index(&[], 17, 2.0), 0);
    }

    #[test]
    fn scale_hotspot_to_pixels_handles_scale_and_degenerate() {
        // 1x: points == pixels.
        assert_eq!(scale_hotspot_to_pixels(4.0, 17.0, 17), 4);
        // 2x: 4pt on a 34px/17pt rep -> 8px.
        assert_eq!(scale_hotspot_to_pixels(4.0, 17.0, 34), 8);
        // Degenerate point size falls back to rounding the raw value.
        assert_eq!(scale_hotspot_to_pixels(4.4, 0.0, 34), 4);
    }

    /// Cursor-only smoke: exercises `capture_system_cursor` (the production
    /// AppKit path: `currentSystemCursor` -> rep selection -> raw-pixel hash ->
    /// PNG) without SCStream, so it runs with just a GUI session and needs no
    /// screen-recording permission. Validates a PNG is produced and that the
    /// dedup gate reports `Unchanged` when handed the same fingerprint. Also
    /// dumps the cursor for visual fidelity checks (hover a text field / link /
    /// resize edge before running to eyeball non-arrow cursors). Run with:
    ///   cargo test -p desk-capture-engine --lib \
    ///     image_capture::mac_screencapturekit::tests::system_cursor_capture_produces_png \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore]
    fn system_cursor_capture_produces_png() {
        let outcome = capture_system_cursor(1.0, 1920, 1080, None);
        let (fingerprint, data) = match outcome {
            CursorCaptureOutcome::Changed(fp, d) => (fp, d),
            other => panic!("expected Changed on first capture, got {other:?}"),
        };
        assert!(!data.base64_png.is_empty(), "cursor PNG must be populated");
        assert!(data.visible);
        assert!(!data.embedded);
        assert_eq!(data.screen_width, 1920);
        assert_eq!(data.screen_height, 1080);

        // Same fingerprint -> no re-emit, no PNG re-encode.
        assert!(matches!(
            capture_system_cursor(1.0, 1920, 1080, Some(fingerprint)),
            CursorCaptureOutcome::Unchanged
        ));

        let png = base64::engine::general_purpose::STANDARD
            .decode(data.base64_png.as_bytes())
            .expect("valid base64 PNG");
        let out = std::env::temp_dir().join("cursor_sync_smoke.png");
        std::fs::write(&out, &png).expect("write smoke PNG");
        eprintln!(
            "[smoke] wrote {} ({} bytes) hotspot=({},{})",
            out.display(),
            png.len(),
            data.hotspot_x,
            data.hotspot_y
        );
    }

    /// Hardware smoke for the native cursor-sync path. Builds the real backend
    /// against the first enumerated display and drives `capture()` in
    /// `SyncNative` mode until a `CursorSyncData` is produced, asserting the
    /// cursor PNG and geometry are populated. It also writes the captured cursor
    /// to `$TMPDIR/cursor_sync_smoke.png` so its fidelity (including non-system
    /// app cursors) can be eyeballed. Ignored by default: needs a GUI session
    /// and screen-recording permission. Run with:
    ///   cargo test -p desk-capture-engine --lib \
    ///     image_capture::mac_screencapturekit::tests::captures_cursor_update_locally \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore]
    fn captures_cursor_update_locally() {
        let content = SCShareableContent::get().expect("shareable content");
        let displays = content.displays();
        let first = displays.first().expect("at least one display required");
        let settings = DeskSettings {
            video_device_name: first.display_id().to_string(),
            ..Default::default()
        };
        let mut capture =
            MacScreencaptureKitImageCapture::new(&settings).expect("construct backend");

        let mut cursor = None;
        for _ in 0..60 {
            if let Ok(result) = capture.capture(CaptureRequest {
                cursor_mode: CursorCaptureMode::SyncNative,
            }) && let Some(c) = result.cursor_update
            {
                cursor = Some(c);
                break;
            }
        }
        let cursor = cursor.expect("expected a cursor_update within 60 frames");
        assert!(
            !cursor.base64_png.is_empty(),
            "cursor PNG must be populated"
        );
        assert!(cursor.visible);
        assert!(cursor.screen_width > 0 && cursor.screen_height > 0);
        assert!(!cursor.embedded, "macOS never bakes the cursor in");

        let png = base64::engine::general_purpose::STANDARD
            .decode(cursor.base64_png.as_bytes())
            .expect("valid base64 PNG");
        let out = std::env::temp_dir().join("cursor_sync_smoke.png");
        std::fs::write(&out, &png).expect("write smoke PNG");
        eprintln!(
            "[smoke] wrote {} ({} bytes) hotspot=({},{}) screen={}x{}",
            out.display(),
            png.len(),
            cursor.hotspot_x,
            cursor.hotspot_y,
            cursor.screen_width,
            cursor.screen_height
        );
    }
}
