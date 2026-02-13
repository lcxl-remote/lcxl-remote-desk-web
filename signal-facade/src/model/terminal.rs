use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
pub struct StartTerminalSession {
    /// The command to start the terminal session. with the format of "path/to/executable,arg1,arg2"
    pub command: String,
    /// Optional session ID to resume or identify a session.
    pub session_id: Option<String>,
}

/// Terminal list
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TerminalList {
    /// terminal command list
    pub commands: Vec<Vec<String>>,

    /// current terminal index
    pub current: usize,
}

/// Terminal settings
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TerminalSettings {
    pub current_terminal: Option<Vec<String>>,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            current_terminal: None,
        }
    }
}
/// SignalingType::SendDataToTerminal
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TerminalInputData {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TerminalOutputData {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TerminalResizeData {
    pub rows: u16,
    pub cols: u16,
}
