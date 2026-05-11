use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Display Rectangle Struct
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    ToSchema,
    Default,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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
#[derive(
    Debug,
    Clone,
    Copy,
    Serialize,
    Deserialize,
    ToSchema,
    Default,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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
#[derive(
    Debug,
    Clone,
    Serialize,
    Deserialize,
    ToSchema,
    Default,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn display_info_round_trips_wincode() {
        // `DisplayInfo` chains three derives (DisplayRect + Resolution +
        // DisplayInfo). Construct a non-default instance with a non-empty
        // resolutions list so a missed derive on any of the three
        // surfaces immediately.
        let original = DisplayInfo {
            device_name: r"\\.\DISPLAY1".to_string(),
            display_device_name: Some("Generic PnP Monitor".to_string()),
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            resolutions: vec![Resolution::new(1920, 1080), Resolution::new(2560, 1440)],
            attached_to_desktop: true,
            rotation: 90,
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: DisplayInfo = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.device_name, original.device_name);
        assert_eq!(back.display_device_name, original.display_device_name);
        assert_eq!(back.desktop_coordinates.right, 1920);
        assert_eq!(back.desktop_coordinates.bottom, 1080);
        assert_eq!(back.resolutions.len(), 2);
        assert_eq!(back.resolutions[1].width, 2560);
        assert_eq!(back.resolutions[1].height, 1440);
        assert!(back.attached_to_desktop);
        assert_eq!(back.rotation, 90);
    }
}
