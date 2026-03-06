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

/// A representation of an image on the clipboard
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub width: usize,
    pub height: usize,
    pub bytes: std::borrow::Cow<'static, [u8]>,
}

pub trait SystemSettingHelper {
    /// Change display settings
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError>;

    /// Block or unblock user input (keyboard and mouse)
    fn block_input(&self, block: bool) -> Result<(), DeskError>;

    /// Enable or disable private screen mode
    fn enable_private_screen(&self, from_session_id: &str, enable: bool) -> Result<(), DeskError>;

    /// Control monitor power (turn on/off)
    fn control_monitor_power(&self, turn_off: bool) -> Result<(), DeskError>;

    /// Set text to clipboard
    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), DeskError>;

    /// Read text from clipboard
    fn get_text_from_clipboard(&mut self) -> Result<Option<String>, DeskError>;

    /// Read image from clipboard
    fn get_image_from_clipboard(&mut self) -> Result<Option<ClipboardImage>, DeskError>;

    /// Set image to clipboard
    fn set_image_to_clipboard(&mut self, image: &ClipboardImage) -> Result<(), DeskError>;
}

#[derive(Debug, Clone)]
pub struct PrivateScreenState {
    pub hotkey_clicked: bool,
    pub visible: bool,
}

#[derive(Debug, Clone)]
pub enum SystemSettingEventType {
    PrivateScreenInited(PrivateScreenState),
    PrivateScreenVisibleChanged(String /*from session id*/, bool),

    PrivateScreenHotkeyRegisterError,
    PrivateScreenUnknownError(
        Option<String>, /*from session id*/
        String,         /*error message*/
    ),
    PrivateScreenClosed,
}

#[derive(Debug, Clone)]
pub enum PrivateScreenCommand {
    Show(String /*from session id*/),
    Hide(String /*from session id*/),
    Quit,
}

pub type SystemSettingSubscriber = tokio::sync::mpsc::UnboundedSender<SystemSettingEventType>;

/// Command sent from server to tauri for whiteboard overlay management
#[derive(Debug, Clone)]
pub enum WhiteboardCommand {
    /// Show whiteboard overlay and start rendering
    Show(String /*from session id*/),
    /// Forward a whiteboard drawing message to the overlay
    DrawMessage(String /*serialized WhiteboardMessage JSON*/),
    /// Hide whiteboard overlay
    Hide(String /*from session id*/),
    /// Quit whiteboard (cleanup)
    Quit,
}
