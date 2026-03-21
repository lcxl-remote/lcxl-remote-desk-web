use clap::Parser;
use serde::{Deserialize, Serialize};
use strum_macros::AsRefStr;
use utoipa::ToSchema;

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
    /// Default mode, includes both signaling server and desk server
    #[default]
    Default,
    /// Signaling mode, include signaling server and turn server
    Signaling,
    /// Desk Server only
    DeskServer,
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
    /// Signaling server url, if not set, it will be "ws://127.0.0.1:{port}/signaling"
    pub signaling_url: Option<String>,
    /// Client ID for telemetry
    pub client_id: Option<String>,
    /// Telemetry consent status
    pub telemetry_consent: Option<bool>,
    /// Auto start the application on system login
    pub auto_start: Option<bool>,
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
            client_id: None,
            telemetry_consent: None,
            auto_start: None,
        }
    }
}
