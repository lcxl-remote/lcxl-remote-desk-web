use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::version::VersionInfo;

/// Connection information
#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct ConnectionModel {
    /// Connection ID
    pub connection_id: String,
    /// IP address of the connection
    pub ip: Option<String>,
    /// Version info of the connection
    pub version_info: VersionInfo,
}

/// Connection list information
#[derive(Serialize, Clone, Debug, ToSchema)]
pub struct ConnectionList {
    /// Current connection ID
    pub current_connection_id: String,
    /// Connection map
    pub connection_map: BTreeMap<String, ConnectionModel>,
}
