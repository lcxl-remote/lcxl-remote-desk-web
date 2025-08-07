use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::desk_error::DeskError;

pub enum ImageType {
    BRGA,
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
}

pub enum ImageCaptureType {
    DIGX,
}

pub trait ImageCaptureTypeHelper {
    fn get_capture_type(&self) -> Result<ImageCaptureType, DeskError>;
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
