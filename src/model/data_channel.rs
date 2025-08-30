use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const DATA_CHANNEL_LABEL_MOUSE_EVENT: &str = "mouse_event";
pub const DATA_CHANNEL_LABEL_KEYBOARD_EVENT: &str = "keyboard_event";
pub const DATA_CHANNEL_LABEL_CLIPBOARD_EVENT: &str = "clipboard_event";
pub const DATA_CHANNEL_LABEL_FILE_TRANSFER_EVENT: &str = "file_transfer_event";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema, Default)]
#[serde(default)]
pub struct SignalRequestControlData {
    pub accept: bool,
    pub accept_file_transfer: bool,
    pub accept_clipboard_sync: bool,
}
