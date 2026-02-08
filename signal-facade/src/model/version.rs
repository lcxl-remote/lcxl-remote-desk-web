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
}

impl VersionInfo {
    pub fn new(
        api_version: i32,
        build_number: i32,
        commit_hash: String,
        remote_desk_type: RemoteDeskTypeEnum,
    ) -> Self {
        Self {
            api_version,
            build_number,
            commit_hash,
            remote_desk_type,
            operation_system: OperationSystemEnum::default(),
        }
    }
}
