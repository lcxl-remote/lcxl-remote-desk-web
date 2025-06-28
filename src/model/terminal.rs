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
}
