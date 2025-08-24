use serde::{Deserialize, Serialize};
use strum_macros::{EnumIter, EnumString, IntoStaticStr};
use utoipa::ToSchema;

use crate::desk_error::DeskError;

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
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError>;
    fn get_capture_type(&self) -> ImageCaptureType;
}

/// Image Output Enumerator Trait
pub trait ImageOutputEnumerator {
    /// Enumerates image output devices.
    fn get_output_list(&self) -> Result<Vec<DisplayInfo>, DeskError>;
}

/// Image Capture Type Enum
#[derive(EnumIter, IntoStaticStr, EnumString, Debug)]
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

/// Display Rectangle Struct
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub struct DisplayRect {
    /// Left coordinate of the rectangle
    pub left: i32,
    /// Top coordinate of the rectangle
    pub top: i32,
    /// Right coordinate of the rectangle
    pub right: i32,
    /// Bottom coordinate of the rectangle
    pub bottom: i32,
}

/// Display Info
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DisplayInfo {
    /// Display device name, e.g. "\\.\DISPLAY1"
    pub device_name: String,
    /// Display device friendly name, e.g. "Generic PnP Monitor"
    pub display_device_name: Option<String>,
    /// Display device rect coordinates on the desktop
    pub desktop_coordinates: DisplayRect,
    /// Is the display attached to the desktop
    pub attached_to_desktop: bool,
    /// Display rotation angle in degrees, e.g. 0, 90, 180, 270
    pub rotation: i32,
}
