use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Build and API information reported by a peer. Missing reports stay unknown.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
pub struct VersionInfo {
    pub api_version: i32,
    pub build_number: i32,
    pub commit_hash: String,
    /// A reported debug marker; false is not proof of a release build.
    #[serde(default)]
    pub debug_build: bool,
}
