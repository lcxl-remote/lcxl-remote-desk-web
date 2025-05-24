use serde::{Deserialize, Serialize};
use utoipa::IntoParams;

#[derive(Debug, Clone, Serialize, Deserialize, IntoParams)]
pub struct StartTerminalSession {
    pub terminal_command: Vec<String>,
}
