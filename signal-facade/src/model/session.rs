use std::collections::BTreeMap;

use serde::Serialize;
use utoipa::ToSchema;

/// Session information
#[derive(Serialize, Clone, Debug, ToSchema)]
pub struct SessionInfo {
    /// Session ID
    pub session_id: String,
}

/// Session list information
#[derive(Serialize, Clone, Debug, ToSchema)]
pub struct SessionList {
    /// Current session ID
    pub current_session_id: String,
    /// Session map
    pub session_map: BTreeMap<String, SessionInfo>,
}
