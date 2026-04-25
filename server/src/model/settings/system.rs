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
        }
    }
}
