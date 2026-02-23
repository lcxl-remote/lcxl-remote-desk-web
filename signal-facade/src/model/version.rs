use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::model::{os::OperationSystemEnum, signal::RemoteDeskTypeEnum};

/// Version information for the API.
#[derive(Serialize, Deserialize, Clone, Debug, IntoParams, ToSchema)]
pub struct VersionInfo {
    /// The version of the API. This is a simple integer that increments when API is changed.
    pub api_version: i32,
    /// The build number of the server.
    pub build_number: i32,
    /// The commit hash of the server.
    pub commit_hash: String,
    /// Remote desk type associated with the version.
    pub remote_desk_type: RemoteDeskTypeEnum,
    /// Operation system associated with the version.
    pub operation_system: OperationSystemEnum,
    /// Display name of the remote desk.
    pub display_name: Option<String>,
    /// Client ID of the server.
    pub client_id: Option<String>,
}

impl VersionInfo {
    pub fn new(
        api_version: i32,
        build_number: i32,
        commit_hash: String,
        remote_desk_type: RemoteDeskTypeEnum,
        display_name: Option<String>,
        client_id: Option<String>,
    ) -> Self {
        Self {
            api_version,
            build_number,
            commit_hash,
            remote_desk_type,
            operation_system: OperationSystemEnum::default(),
            display_name,
            client_id,
        }
    }
}
