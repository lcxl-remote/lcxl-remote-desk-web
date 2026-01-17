use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::version::VersionInfo;

/// Session information
#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct SessionModel {
    /// Session ID
    pub session_id: String,
    /// Version info of the session
    pub version_info: VersionInfo,
}

/// Session list information
#[derive(Serialize, Clone, Debug, ToSchema)]
pub struct SessionList {
    /// Current session ID
    pub current_session_id: String,
    /// Session map
    pub session_map: BTreeMap<String, SessionModel>,
}
