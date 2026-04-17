use desk_signal_facade::model::image_capture::DisplayInfo;
use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::error::DeskError;
use crate::model::data_channel::CursorSyncData;

#[derive(Debug, Clone, Copy)]
pub enum ImageType {
    BGRA,
    RGB,
}
pub trait ImageInfo {
    fn get_type(&self) -> ImageType;
    fn get_data(&self) -> &[u8];
    fn get_width(&self) -> u32;
    fn get_height(&self) -> u32;
}

pub trait ImageCapture {
    fn capture(&mut self, show_mouse: bool) -> Result<Box<dyn ImageInfo + Send + Sync>, DeskError>;
    fn capture_cursor(&mut self, last_shape_id: Option<u64>) -> Result<Option<CursorSyncData>, DeskError> {
        Ok(None)
    }
    fn get_capture_type(&self) -> ImageCaptureType;
    fn get_current_output(&self) -> Result<DisplayInfo, DeskError>;
}

/// Image Output Enumerator Trait
pub trait ImageOutputEnumerator {
    /// Enumerates image output devices.
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError>;
}

/// Image Capture Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString, Debug, Clone, Copy)]
pub enum ImageCaptureType {
    /// Capture image from DXGI device
    #[cfg(target_os = "windows")]
    DXGI,
    /// Capture image from GDI device
    #[cfg(target_os = "windows")]
    GDI,
    /// Capture image from X11 device
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
    fn get_image_capture_type(&self) -> Result<ImageCaptureType, DeskError>;
}
