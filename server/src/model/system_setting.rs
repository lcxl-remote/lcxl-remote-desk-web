use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::DeskError;

/// Display settings structure
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
pub struct DisplaySettings {
    /// Device name
    pub device_name: String,
    /// Width in pixels
    pub width: Option<u32>,
    /// Height in pixels
    pub height: Option<u32>,
    /// Refresh frequency in Hz
    pub frequency: Option<u32>,
    /// Scaling factor for high DPI displays (e.g., 1.0, 1.25, 1.5, 2.0)
    /// see https://github.com/imniko/SetDPI
    pub scaling_factor: Option<f64>, // Add
}

pub trait SystemSettingHelper {
    /// Change display settings
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError>;

    /// Block or unblock user input (keyboard and mouse)
    fn block_input(&self, block: bool) -> Result<(), DeskError>;

    /// Enable or disable private screen mode
    fn enable_private_screen(&self, enable: bool) -> Result<(), DeskError>;

    /// Control monitor power (turn on/off)
    fn control_monitor_power(&self, turn_off: bool) -> Result<(), DeskError>;

    /// Set text to clipboard
    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), DeskError>;
}

#[derive(Debug, Clone)]
pub struct PrivateScreenState {
    pub hotkey_clicked: bool,
    pub visible: bool,
}

#[derive(Debug, Clone)]
pub enum SystemSettingEventType {
    PrivateScreenInited(PrivateScreenState),
    PrivateScreenVisibleChanged(bool),

    PrivateScreenHotkeyRegisterError,
    PrivateScreenUnknownError(String),
    PrivateScreenClosed,
}

pub type SystemSettingSubscriber = fn(event_type: SystemSettingEventType) -> Result<(), DeskError>;
