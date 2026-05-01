use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
pub struct StartTerminalSession {
    /// The command to start the terminal session. with the format of "path/to/executable,arg1,arg2"
    pub command: String,
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
#[derive(Default)]
pub struct TerminalSettings {
    pub current_terminal: Option<Vec<String>>,
}


/// List terminal query path
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct ListTerminalPath {
    /// connection id
    pub connection_id: String,
}

/// Start terminal query path
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct StartTerminalPath {
    /// connection id
    pub connection_id: String,
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
