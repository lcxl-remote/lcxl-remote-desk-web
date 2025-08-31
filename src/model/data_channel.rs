use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const DATA_CHANNEL_LABEL_MOUSE_EVENT: &str = "mouse_event";
pub const DATA_CHANNEL_LABEL_KEYBOARD_EVENT: &str = "keyboard_event";
pub const DATA_CHANNEL_LABEL_CLIPBOARD_EVENT: &str = "clipboard_event";
pub const DATA_CHANNEL_LABEL_FILE_TRANSFER_EVENT: &str = "file_transfer_event";

/// Signal request control data
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, Default)]
#[serde(default)]
pub struct SignalRequestControlData {
    /// whether the control request is accepted
    pub accept: bool,
    /// whether to accept file transfer
    pub accept_file_transfer: bool,
    /// whether to accept clipboard sync
    pub accept_clipboard_sync: bool,
}

/// Mouse event data structure
/// https://developer.mozilla.org/zh-CN/docs/Web/API/MouseEvent
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
pub struct MouseEventData {
    /// mouse event type, e.g. "mousemove", "mousedown", "mouseup", "click", "dblclick", "contextmenu", "wheel"
    pub event: String,
    /// mouse x coordinate
    pub x: i32,
    /// mouse y coordinate
    pub y: i32,
    ///Returns true if the alt key was down when the mouse event was fired.
    pub alt_key: bool,
    /// The button number that was pressed or released (if applicable) when the mouse event was fired.
    pub button: i32,
    /// The buttons being pressed (if any) when the mouse event was fired.
    pub buttons: i32,
}

/// Keyboard event data structure
/// https://developer.mozilla.org/zh-CN/docs/Web/API/KeyboardEvent
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
pub struct KeyboardEventData {
    /// keyboard event type, e.g. "keydown", "keyup", "keypress"
    pub event: String,
    /// key value, e.g. "a", "
    pub key: String,
    /// key code, e.g. "KeyA", "Enter"
    pub code: String,
    /// whether the key is a system key
    pub alt_key: bool,
    /// whether the ctrl key is pressed
    pub ctrl_key: bool,
    /// whether the shift key is pressed
    pub shift_key: bool,
    /// whether the meta key is pressed
    pub meta_key: bool,
    /// location of the key on the keyboard
    pub location: i32,
    /// whether the key is repeated
    pub repeat: bool,
    /// whether the key is composing
    pub is_composing: bool,
}
