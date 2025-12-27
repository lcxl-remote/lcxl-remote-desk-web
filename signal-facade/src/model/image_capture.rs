use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Display Rectangle Struct
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, Default)]
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

impl DisplayRect {
    /// Get width of the rectangle
    pub fn width(&self) -> i32 {
        self.right - self.left
    }

    /// Get height of the rectangle
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

/// Resolution Struct
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, Default)]
pub struct Resolution {
    /// Width of the resolution in pixels
    pub width: u32,
    /// Height of the resolution in pixels
    pub height: u32,
}

impl Resolution {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// Display Info
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct DisplayInfo {
    /// Display device name, e.g. "\\.\DISPLAY1"
    pub device_name: String,
    /// Display device friendly name, e.g. "Generic PnP Monitor"
    pub display_device_name: Option<String>,
    /// Display device rect coordinates on the desktop
    pub desktop_coordinates: DisplayRect,
    /// Supported display resolutions (width, height)
    pub resolutions: Vec<Resolution>,
    /// Is the display attached to the desktop
    pub attached_to_desktop: bool,
    /// Display rotation angle in degrees, e.g. 0, 90, 180, 270
    pub rotation: i32,
}
