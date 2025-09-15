use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::desk_error::DeskError;

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
    /// mouse x coordinate(relative to the viewport)
    pub x: f64,
    /// mouse y coordinate(relative to the viewport)
    pub y: f64,
    ///Returns true if the alt key was down when the mouse event was fired.
    pub alt_key: bool,
    /// The button number that was pressed or released (if applicable) when the mouse event was fired.
    pub button: i32,
    /// The buttons being pressed (if any) when the mouse event was fired.
    pub buttons: i32,

    /// The distance that the mouse wheel has been rotated around the X axis (if applicable) when the mouse event was fired.
    pub delta_x: f64,
    /// The distance that the mouse wheel has been rotated around the Y axis (if applicable) when the mouse event was fired.
    pub delta_y: f64,
}

/// Keyboard event data structure
/// https://developer.mozilla.org/zh-CN/docs/Web/API/KeyboardEvent
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
pub struct KeyboardEventData {
    /// keyboard event type, e.g. "keydown", "keyup", "keypress"
    pub event: String,
    /// key value, e.g. "a", see https://developer.mozilla.org/zh-CN/docs/Web/API/UI_Events/Keyboard_event_key_values
    pub key: String,
    /// key code, e.g. "KeyA", "Enter", see https://developer.mozilla.org/zh-CN/docs/Web/API/KeyboardEvent/code#code_values
    pub code: String,
    /// physical key code, e.g. 65 for "a", see https://developer.mozilla.org/zh-CN/docs/Web/API/KeyboardEvent/keyCode
    pub key_code: u32,
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

/// Mouse Event Handler Trait
pub trait MouseEventHandler {
    /// Handle mouse event
    fn handle_mouse_event(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
        match event.event.as_str() {
            "mousemove" => self.handle_mouse_move(event),
            "mousedown" => self.handle_mouse_down(event),
            "mouseup" => self.handle_mouse_up(event),
            "wheel" => self.handle_mouse_wheel(event),
            _ => {
                log::warn!("Unsupported mouse event type: {}", event.event);
                Ok(())
            }
        }
    }

    /// Handle mouse move event
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), DeskError>;

    /// Handle mouse down event
    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), DeskError>;

    /// Handle mouse up event
    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), DeskError>;
    /// Handle mouse wheel event
    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), DeskError>;
}

pub trait KeyboardEventHandler {
    /// Handle keyboard event
    fn handle_keyboard_event(&self, event: &KeyboardEventData) -> Result<(), DeskError> {
        match event.event.as_str() {
            "keydown" => self.handle_key_down(event),
            "keyup" => self.handle_key_up(event),
            _ => {
                log::warn!("Unsupported keyboard event type: {}", event.event);
                Ok(())
            }
        }
    }
    /// Handle key down event
    fn handle_key_down(&self, event: &KeyboardEventData) -> Result<(), DeskError>;

    /// Handle key up event
    fn handle_key_up(&self, event: &KeyboardEventData) -> Result<(), DeskError>;
}
