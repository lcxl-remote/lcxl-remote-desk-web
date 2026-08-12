use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const DATA_CHANNEL_LABEL_MOUSE_EVENT: &str = "mouse_event";
pub const DATA_CHANNEL_LABEL_MOUSE_MOVE_EVENT: &str = "mouse_move_event";
pub const DATA_CHANNEL_LABEL_KEYBOARD_EVENT: &str = "keyboard_event";
pub const DATA_CHANNEL_LABEL_CLIPBOARD_EVENT: &str = "clipboard_event";
pub const DATA_CHANNEL_LABEL_FILE_TRANSFER_EVENT: &str = "file_transfer_event";
pub const DATA_CHANNEL_LABEL_WHITEBOARD_EVENT: &str = "whiteboard_event";
pub const DATA_CHANNEL_LABEL_CURSOR_SYNC_EVENT: &str = "cursor_sync_event";

/// Cursor sync data structure
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
#[serde(default)]
pub struct CursorSyncData {
    /// PNG image data (Base64)
    pub base64_png: String,
    /// Mouse hotspot X coordinate (offset within the image)
    pub hotspot_x: i32,
    /// Mouse hotspot Y coordinate (offset within the image)
    pub hotspot_y: i32,
    /// Whether the mouse is visible
    pub visible: bool,
    /// Graphic hash or ID, used to detect shape changes
    pub shape_id: u64,
    /// Remote screen width
    pub screen_width: u32,
    /// Remote screen height
    pub screen_height: u32,
    /// True when the cursor pixel is already composited into the
    /// captured desktop frame (DXGI software-cursor path). The
    /// front-end keeps showing the local CSS cursor sprite for a
    /// responsive feel — meaning the user sees two cursors (the
    /// low-latency local sprite plus the lagging OS cursor in the
    /// video frame) — but the embedded flag drives a one-off toast
    /// so the user understands the source of the second cursor.
    /// Defaults to false on older payloads.
    pub embedded: bool,
}

/// Signal request control data
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, Default)]
#[serde(default)]
pub struct SignalRequestControlData {
    /// whether to accept file transfer
    pub accept_file_transfer: bool,
    /// whether to accept clipboard sync
    pub accept_clipboard_sync: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardEventData {
    pub r#type: String, // "text", "image_start", "image_chunk", "image_end", "error"
    pub content: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub total_bytes: Option<u64>,
    pub chunk_count: Option<u32>,
    pub index: Option<u32>,
}
