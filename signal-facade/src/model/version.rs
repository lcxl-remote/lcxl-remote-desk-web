use serde::{Deserialize, Serialize};

/// Version information for the API.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VersionInfo {
    /// The version of the API. This is a simple integer that increments when API is changed.
    pub api_version: i32,
    /// The full version string, including any additional information like build metadata or release tags.
    pub full_version: Option<String>,
}
