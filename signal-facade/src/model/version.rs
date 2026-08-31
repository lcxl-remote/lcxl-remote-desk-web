use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::model::{os::OperationSystemEnum, signal::RemoteDeskTypeEnum};

/// Version information for the API.
#[derive(Serialize, Deserialize, Clone, Debug, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
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
    /// Authentication token for server nodes or API clients.
    pub token: Option<String>,
    /// Whether this binary was compiled with debug assertions (a debug build).
    /// Defaults to `false` for peers that do not report it.
    #[serde(default)]
    pub debug_build: bool,
    /// Source repository URL this binary was built from, when known.
    /// Defaults to `None` for peers that do not report it.
    #[serde(default)]
    pub repository_url: Option<String>,
    /// Canonical shell names the host has verified as usable by the AI command
    /// executor. This is narrower than the interactive-terminal list: only
    /// interpreters supported by the free-form classifier are reported.
    #[serde(default)]
    pub available_exec_shells: Option<String>,
    /// Edge-local total wall-time ceiling for one AI command. The central brain
    /// advertises this to the model and reproduces it when sealing a plan; the
    /// edge re-applies the current value at PEP time.
    #[serde(default)]
    pub max_ai_command_runtime_ms: Option<u32>,
    /// Current host support for the one-shot exec-PTY transport. This is a
    /// capability/readiness bit, not a protocol version.
    #[serde(default)]
    pub exec_pty: bool,
    /// Current host support for root-contained interactive sudo/doas execution.
    /// This is separately reported because ordinary PTY support does not imply
    /// that a privileged command can be contained safely.
    #[serde(default)]
    pub exec_pty_elevation: bool,
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
            token: None,
            // `cfg!(debug_assertions)` is resolved against the build profile of
            // the binary that constructs this info, so it reports our own build.
            debug_build: cfg!(debug_assertions),
            repository_url: None,
            available_exec_shells: None,
            max_ai_command_runtime_ms: None,
            exec_pty: false,
            exec_pty_elevation: false,
        }
    }

    /// Store canonical shell names in the query-safe comma-separated wire field.
    pub fn set_available_exec_shells(&mut self, shells: &[String]) {
        self.available_exec_shells = (!shells.is_empty()).then(|| shells.join(","));
    }

    /// Decode the host-reported query field into individual shell names.
    pub fn available_exec_shell_list(&self) -> Vec<String> {
        self.available_exec_shells
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|shell| !shell.is_empty())
            .map(str::to_string)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::signal::RemoteDeskTypeEnum;

    #[test]
    fn new_stamps_build_profile_and_no_repo() {
        let info = VersionInfo::new(1, 42, "abc".into(), RemoteDeskTypeEnum::Server, None, None);
        assert_eq!(info.debug_build, cfg!(debug_assertions));
        assert_eq!(info.repository_url, None);
    }

    #[test]
    fn query_round_trip_carries_new_fields() {
        let mut info =
            VersionInfo::new(1, 42, "abc".into(), RemoteDeskTypeEnum::Server, None, None);
        info.debug_build = true;
        info.repository_url = Some("https://example.test/repo.git".into());
        info.set_available_exec_shells(&["powershell".into(), "pwsh".into()]);
        info.max_ai_command_runtime_ms = Some(600_000);
        info.exec_pty = true;
        info.exec_pty_elevation = true;

        let query = serde_urlencoded::to_string(&info).unwrap();
        let decoded: VersionInfo = serde_urlencoded::from_str(&query).unwrap();

        assert!(decoded.debug_build);
        assert_eq!(
            decoded.repository_url.as_deref(),
            Some("https://example.test/repo.git")
        );
        assert_eq!(
            decoded.available_exec_shell_list(),
            vec!["powershell".to_string(), "pwsh".to_string()]
        );
        assert_eq!(decoded.max_ai_command_runtime_ms, Some(600_000));
        assert!(decoded.exec_pty);
        assert!(decoded.exec_pty_elevation);
    }

    #[test]
    fn missing_new_fields_fall_back_to_defaults() {
        // A peer built before these fields existed omits them from the query;
        // `#[serde(default)]` must keep deserialization succeeding.
        let query = "api_version=1&build_number=0&commit_hash=x\
            &remote_desk_type=server&operation_system=Windows";
        let decoded: VersionInfo = serde_urlencoded::from_str(query).unwrap();

        assert!(!decoded.debug_build);
        assert_eq!(decoded.repository_url, None);
        assert!(decoded.available_exec_shell_list().is_empty());
        assert_eq!(decoded.max_ai_command_runtime_ms, None);
        assert!(!decoded.exec_pty);
        assert!(!decoded.exec_pty_elevation);
    }

    #[test]
    fn openapi_parameters_are_query_parameters() {
        use utoipa::openapi::path::ParameterIn;

        let parameters = VersionInfo::into_params(|| None);

        assert!(!parameters.is_empty());
        assert!(
            parameters
                .iter()
                .all(|parameter| parameter.parameter_in == ParameterIn::Query)
        );
    }
}
