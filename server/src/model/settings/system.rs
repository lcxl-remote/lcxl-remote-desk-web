use clap::Parser;
use desk_utils::error::DeskErrorCode;
use serde::{Deserialize, Serialize};
use strum_macros::AsRefStr;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::DeskError;

#[derive(
    clap::ValueEnum,
    Clone,
    Default,
    Debug,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    AsRefStr,
    ToSchema,
)]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum StartupMode {
    /// Default mode, includes both signaling server and desk server (Portable)
    #[default]
    Default,
    /// Signaling mode, include signaling server and turn server
    Signaling,
    /// Desk Server only
    DeskServer,
    /// System service daemon (SYSTEM / root) - manages Worker lifecycle
    ServiceDaemon,
    /// Session worker process - launched by ServiceDaemon in target desktop
    SessionWorker,
    /// Read-only MCP server over stdio (local AI assistant integration). stdin /
    /// stdout carry the MCP JSON-RPC framing, so this mode must never log to
    /// stdout (see `is_headless_startup_mode`).
    McpStdio,
}

/// Command line arguments
#[derive(Parser, Debug, Clone, Default, Serialize, Deserialize)]
#[command(ignore_errors = true, version, about, long_about = None, group(
    clap::ArgGroup::new("frontend_mode")
        .args(["prod_frontend", "dev_frontend"])
        .multiple(false) // only one of them can be set
))]
pub struct Args {
    /// Config file path
    #[clap(short, long, default_value = "conf/config")]
    pub config_file_path: String,

    /// Startup mode
    #[clap(short, long, default_value_t, value_enum)]
    pub startup_mode: StartupMode,

    /// Production frontend
    #[arg(long)]
    pub prod_frontend: bool,

    /// Development frontend
    #[arg(long)]
    pub dev_frontend: bool,

    /// Start in hidden mode (used for auto-start)
    #[arg(long)]
    pub hidden: bool,

    /// IPC pipe name for SessionWorker mode (provided by ServiceDaemon)
    #[arg(long)]
    pub pipe: Option<String>,
}

/// System settings for the application. This struct is used to load and save settings from a configuration file.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct SystemSettings {
    /// Enable IPv6 support
    pub enable_ipv6: bool,
    /// port number for the server to bind to
    pub port: u16,
    /// listen ipv4 address for the server to bind to
    pub listen_addr_ipv4: String,
    /// listen ipv6 address for the server to bind to
    pub listen_addr_ipv6: String,

    /// Optional locale setting (e.g., "en", "zh-CN")
    pub locale: Option<String>,
    /// Remote signaling server url for connecting to a standalone signaling server
    pub signaling_url: Option<String>,
    /// Token for authenticating with the remote signaling server
    pub signaling_token: Option<String>,
    /// Remote manager server url for connecting to an enterprise manager
    pub manager_url: Option<String>,
    /// Client ID for telemetry
    client_id: Option<String>,
    /// Telemetry consent status
    pub telemetry_consent: Option<bool>,
    /// Auto start the application on system login
    pub auto_start: Option<bool>,
    /// API Token for connecting to manager's signaling server
    pub manager_api_token: Option<String>,
    /// Local signaling server token, auto-generated and persisted.
    /// Used by the local desk server to authenticate with the co-located signaling server.
    pub local_signaling_token: Option<String>,
    /// Token for authenticating the Tauri IPC WebSocket connection (/ws/tauri_ipc).
    /// Auto-generated and persisted on first startup.
    pub tauri_ipc_token: Option<String>,
    /// Stable cookie signing key for session middleware (hex-encoded).
    /// Auto-generated and persisted so sessions survive daemon restarts.
    pub session_secret_key: Option<String>,

    /// Whether the daemon should kill+restart a session worker that
    /// stops sending heartbeats. Defaults to enabled. Set to `false`
    /// when investigating worker hangs so the stuck process stays
    /// alive long enough to attach a debugger / capture a stack dump.
    pub worker_heartbeat_watchdog_enabled: Option<bool>,

    /// Number of seconds without a worker heartbeat before the
    /// watchdog declares the worker stuck and triggers a restart.
    /// Workers send heartbeats every 5s; the default of 30s gives
    /// roughly 6 missed beats of slack so transient spikes don't
    /// trigger spurious restarts.
    pub worker_heartbeat_timeout_secs: Option<u64>,

    /// Override for the daemon-side WebRTC ICE `disconnected` timeout
    /// (the duration without ICE traffic before an agent flips
    /// `Connected → Disconnected`). `None` means use the built-in
    /// default; see `pc_manager::DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS`.
    ///
    /// Lowering it makes the daemon-side cleanup hook fire sooner when
    /// a browser closes the tab, which is what frees the worker's DXGI
    /// duplication for the next session. Lower it too far and a real
    /// network blip will tear down a healthy session.
    ///
    /// Not surfaced in the settings UI yet — edit the config file
    /// directly or rely on the default.
    pub webrtc_ice_disconnected_timeout_secs: Option<u64>,

    /// Override for the daemon-side WebRTC ICE `failed` timeout (the
    /// duration in `Disconnected` before an agent flips to `Failed`).
    /// `None` means use the built-in default; see
    /// `pc_manager::DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS`.
    ///
    /// Together with `webrtc_ice_disconnected_timeout_secs` this caps
    /// how long the daemon waits before reclaiming the
    /// per-`connection_id` resources. The pair-active signaling-side
    /// `ConnectionRemoved` notification (when present) bypasses both
    /// timeouts and triggers cleanup in milliseconds; this fallback
    /// only matters when signaling itself is gone too.
    ///
    /// Not surfaced in the settings UI yet — edit the config file
    /// directly or rely on the default.
    pub webrtc_ice_failed_timeout_secs: Option<u64>,
}

impl SystemSettings {
    /// Get client id, if not set, return error
    pub fn get_client_id(&self) -> Result<String, DeskError> {
        if let Some(client_id) = &self.client_id {
            Ok(client_id.clone())
        } else {
            Err(DeskError::new_custom_error(
                DeskErrorCode::CLIENT_ID_NOT_FOUND,
                "client_id is not set",
            ))
        }
    }

    pub fn get_or_generate_client_id(&mut self) -> String {
        if let Some(client_id) = &self.client_id {
            client_id.clone()
        } else {
            self.generate_client_id()
        }
    }

    pub fn generate_client_id(&mut self) -> String {
        let new_id = Uuid::new_v4().to_string();
        log::info!("Generated new client_id: {}", new_id);
        self.client_id = Some(new_id.clone());
        new_id
    }

    /// Carry over the auto-generated, internally-managed fields that the console
    /// settings form never sends in its payload. Without this, a full-struct
    /// replace from `POST /settings` resets them to `None`, which drops the
    /// persisted `client_id` (silently breaking the manager signaling proxy, as
    /// it returns early before even attempting to connect), the local signaling
    /// token, the Tauri IPC token and the session signing key. Each field is
    /// only restored when the incoming value is absent, so an explicit override
    /// (should the payload ever carry one) still wins.
    pub fn preserve_internal_fields(&mut self, previous: &SystemSettings) {
        if self.client_id.is_none() {
            self.client_id = previous.client_id.clone();
        }
        if self.local_signaling_token.is_none() {
            self.local_signaling_token = previous.local_signaling_token.clone();
        }
        if self.tauri_ipc_token.is_none() {
            self.tauri_ipc_token = previous.tauri_ipc_token.clone();
        }
        if self.session_secret_key.is_none() {
            self.session_secret_key = previous.session_secret_key.clone();
        }
    }
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self {
            enable_ipv6: true,
            port: 8081,
            listen_addr_ipv4: "0.0.0.0".to_string(),
            listen_addr_ipv6: "::".to_string(),
            locale: None,
            signaling_url: None,
            signaling_token: None,
            manager_url: None,
            client_id: None,
            telemetry_consent: None,
            auto_start: None,
            manager_api_token: None,
            local_signaling_token: None,
            tauri_ipc_token: None,
            session_secret_key: None,
            worker_heartbeat_watchdog_enabled: None,
            worker_heartbeat_timeout_secs: None,
            webrtc_ice_disconnected_timeout_secs: None,
            webrtc_ice_failed_timeout_secs: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_internal_fields_keeps_secrets_when_update_omits_them() {
        let previous = SystemSettings {
            client_id: Some("cid".to_string()),
            local_signaling_token: Some("lst".to_string()),
            tauri_ipc_token: Some("tit".to_string()),
            session_secret_key: Some("ssk".to_string()),
            ..SystemSettings::default()
        };

        // Simulates the console payload: a real form field is set, the
        // auto-generated internal fields are absent (deserialized to None).
        let mut incoming = SystemSettings {
            manager_url: Some("ws://manager/api/desk/signaling".to_string()),
            ..SystemSettings::default()
        };

        incoming.preserve_internal_fields(&previous);

        assert_eq!(incoming.client_id.as_deref(), Some("cid"));
        assert_eq!(incoming.local_signaling_token.as_deref(), Some("lst"));
        assert_eq!(incoming.tauri_ipc_token.as_deref(), Some("tit"));
        assert_eq!(incoming.session_secret_key.as_deref(), Some("ssk"));
        // The actual form field still takes effect.
        assert_eq!(
            incoming.manager_url.as_deref(),
            Some("ws://manager/api/desk/signaling")
        );
    }

    #[test]
    fn preserve_internal_fields_respects_explicit_incoming_values() {
        let previous = SystemSettings {
            client_id: Some("old".to_string()),
            ..SystemSettings::default()
        };
        let mut incoming = SystemSettings {
            client_id: Some("new".to_string()),
            ..SystemSettings::default()
        };

        incoming.preserve_internal_fields(&previous);

        assert_eq!(incoming.client_id.as_deref(), Some("new"));
    }
}
