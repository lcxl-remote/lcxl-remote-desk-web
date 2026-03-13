use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Security settings for controlling remote access permissions.
///
/// Each field uses `Option<bool>`:
///   - `None`  — not configured (GUI: prompt user; headless: deny)
///   - `Some(true)`  — always allow
///   - `Some(false)` — always deny
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema, Default)]
#[serde(default)]
pub struct SecuritySettings {
    /// Allow remote desktop control (mouse/keyboard input)
    pub allow_remote_control: Option<bool>,
    /// Allow clipboard synchronization
    pub allow_clipboard_sync: Option<bool>,
    /// Allow enabling private screen mode
    pub allow_private_screen: Option<bool>,
    /// Allow whiteboard overlay
    pub allow_whiteboard: Option<bool>,
    /// Allow remote terminal access
    pub allow_terminal: Option<bool>,
    /// Allow file browsing (list/delete files via signaling)
    pub allow_file_browse: Option<bool>,
    /// Allow file transfer (upload/download via DataChannel)
    pub allow_file_transfer: Option<bool>,
}
