use desk_signal_facade::model::image_capture::DisplayInfo;
use strum_macros::{EnumIter, EnumString, IntoStaticStr};

use crate::error::DeskError;

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
    /// Capture image from DIGX device
    #[cfg(target_os = "windows")]
    DIGX,
    /// Capture image from DGI device
    #[cfg(target_os = "windows")]
    DGI,
    /// Capture image from X11 device
    #[cfg(target_os = "linux")]
    X11,
    /// Capture image from PipeWire device
    #[cfg(target_os = "linux")]
    PIPEWIRE,
}

impl Default for ImageCaptureType {
    fn default() -> Self {
        #[cfg(target_os = "windows")]
        return ImageCaptureType::DIGX;
        #[cfg(target_os = "linux")]
        return ImageCaptureType::X11;
    }
}

pub trait ImageCaptureTypeHelper {
    fn get_image_capture_type(&self) -> Result<ImageCaptureType, DeskError>;
}
