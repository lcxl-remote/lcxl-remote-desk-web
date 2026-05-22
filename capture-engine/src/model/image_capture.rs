use desk_signal_facade::model::image_capture::DisplayInfo;
use serde::{Deserialize, Serialize};
use strum_macros::{EnumIter, EnumString, IntoStaticStr};
use utoipa::ToSchema;

use crate::error::CaptureError;

#[derive(Debug, Clone, Copy)]
pub enum ImageType {
    BGRA,
    RGB,
}
/// A rectangular region of the screen that changed in the current frame.
/// Coordinates are in pixels, top-left origin.  Width and height are always
/// aligned to even numbers (required by YUV420 chroma subsampling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

pub trait ImageInfo {
    fn get_type(&self) -> ImageType;
    fn get_data(&self) -> &[u8];
    fn get_width(&self) -> u32;
    fn get_height(&self) -> u32;
    /// Row stride in bytes.  May be larger than width * 4 (e.g. DXGI staging
    /// texture pitch).  Defaults to tightly-packed layout.
    fn get_stride(&self) -> u32 {
        self.get_width() * 4
    }
    /// Which regions of the frame changed since the last captured frame.
    /// - `None`           → no dirty-rect information; caller must do a full YUV conversion.
    /// - `Some([])`       → nothing changed; caller may skip encoding entirely.
    /// - `Some(rects)`    → only these regions changed; partial YUV conversion is possible.
    fn get_dirty_rects(&self) -> Option<&[DirtyRect]> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorCaptureMode {
    RenderInFrame,
    SyncNative,
    Disable,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest {
    pub cursor_mode: CursorCaptureMode,
}

/// Cursor sync data structure
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
#[serde(default)]
pub struct CursorSyncData {
    /// PNG image data (Base64)
    pub base64_png: String,
    /// Mouse hotspot X coordinate (offset within the image)
    pub hotspot_x: i32,
    /// Mouse hotspot Y coordinate (offset within the image)
    pub hotspot_y: i32,
    /// Whether the mouse is visible
    pub visible: bool,
    /// Graphic hash or ID, used to detect shape changes
    pub shape_id: u64,
    /// Remote screen width
    pub screen_width: u32,
    /// Remote screen height
    pub screen_height: u32,
    /// True when the cursor pixel is already composited into the
    /// captured desktop frame (DXGI software-cursor path). The
    /// front-end keeps showing the local CSS cursor sprite for a
    /// responsive feel — meaning the user sees two cursors (the
    /// low-latency local sprite plus the lagging OS cursor in the
    /// video frame) — but the embedded flag drives a one-off toast
    /// so the user understands the source of the second cursor.
    /// Defaults to false on older payloads.
    pub embedded: bool,
}

pub struct CaptureResult {
    pub image: Box<dyn ImageInfo + Send + Sync>,
    pub cursor_update: Option<CursorSyncData>,
    /// Whether the desktop content changed since the last captured frame.
    /// When `false` the caller should skip YUV conversion and encoding.
    pub content_changed: bool,
    /// Dirty regions for this frame (see `ImageInfo::get_dirty_rects`).
    /// `None` here means the image field carries full-frame data without
    /// region annotations; encoders must do a full conversion.
    pub dirty_rects: Option<Vec<DirtyRect>>,
}

pub trait ImageCapture {
    fn capture(&mut self, request: CaptureRequest) -> Result<CaptureResult, CaptureError>;
    fn supports_cursor_sync(&self) -> bool {
        false
    }
    fn get_capture_type(&self) -> ImageCaptureType;
    fn get_current_output(&self) -> Result<DisplayInfo, CaptureError>;
}

/// Image Output Enumerator Trait
pub trait ImageOutputEnumerator {
    /// Enumerates image output devices.
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, CaptureError>;
}

/// Image Capture Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString, Debug, Clone, Copy)]
pub enum ImageCaptureType {
    /// Capture image via Windows.Graphics.Capture (WGC).
    /// Declared first so EnumIter visits it before DXGI/GDI; the frontend
    /// dropdown additionally pins WGC to the top for preferred ordering.
    #[cfg(target_os = "windows")]
    WGC,
    /// Capture image from DXGI device
    #[cfg(target_os = "windows")]
    DXGI,
    /// Capture image from GDI device
    #[cfg(target_os = "windows")]
    GDI,
    /// Capture image from X11 device
    #[cfg(target_os = "linux")]
    X11,
    /// Capture image via Wayland portal + PipeWire
    #[cfg(target_os = "linux")]
    WAYLANDPORTAL,
    /// Capture image using ScreenCaptureKit
    #[cfg(target_os = "macos")]
    SCKIT,
}

impl Default for ImageCaptureType {
    fn default() -> Self {
        #[cfg(target_os = "windows")]
        return ImageCaptureType::DXGI;
        #[cfg(target_os = "linux")]
        return ImageCaptureType::X11;
        #[cfg(target_os = "macos")]
        return ImageCaptureType::SCKIT;
    }
}

pub trait ImageCaptureTypeHelper {
    fn get_image_capture_type(&self) -> Result<ImageCaptureType, CaptureError>;
}
