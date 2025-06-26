use serde::{Deserialize, Serialize};
use utoipa::IntoParams;

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
pub struct StartTerminalSession {
    /// The command to start the terminal session. with the format of "path/to/executable,arg1,arg2"
    pub command: String,
}
