use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Data model for EnablePrivateScreen signaling
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EnablePrivateScreenData {
    pub enable: bool,
}

/// Data model for PrivateScreenStateChanged signaling
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PrivateScreenStateChangedData {
    pub visible: bool,
    pub is_supported: bool,
    pub error_msg: Option<String>,
}
