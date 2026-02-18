use std::{fs, ops::Deref, path::PathBuf};

use chrono::{DateTime, Local};
use clap::Parser;
use config::{Config, Environment, File};
use desk_signal_facade::model::{desk_settings::DeskSettings, terminal::TerminalSettings};
use desk_turn::model::TurnSettings;
use desk_utils::error::DeskErrorCode;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use strum_macros::AsRefStr;
use tokio::sync::RwLock;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::error::DeskError;

#[derive(
    clap::ValueEnum, Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq, AsRefStr,
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

#[derive(Parser, Debug, Clone, Default, Serialize, Deserialize)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Name of the person to greet
    #[clap(short, long, default_value = "conf/config")]
    pub config_file_path: String,

    #[clap(short, long, default_value_t, value_enum)]
    pub startup_mode: StartupMode,
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
    /// access logs are printed with the INFO level so ensure it is enabled by default
    pub log_level: String,
    /// Enable Rust backtrace for errors
    pub traceback: bool,

    /// Optional locale setting (e.g., "en", "zh-CN")
    pub locale: Option<String>,
    /// Whether to open the browser automatically on server startup
    pub open_browser_on_startup: bool,
    /// Signaling server url, if not set, it will be "ws://127.0.0.1:{port}/signaling"
    pub signaling_url: Option<String>,
    /// Client ID for telemetry
    pub client_id: Option<String>,
    /// Telemetry consent status
    pub telemetry_consent: Option<bool>,
}

/// User settings
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UserSettings {
    /// login user name
    pub login_user_name: String,
    /// login password
    pub login_password: String,
}

/// Query parameters for listing files.
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct ListSettings {
    /// Page number, start from 1
    pub page_no: i64,
    /// Page count, must be greater than 0
    pub page_count: i64,
    /// Minimum file size
    pub min_file_size: Option<i64>,
    /// Max file size
    pub max_file_size: Option<i64>,
    /// Dir path of the directory containing the file
    pub dir_path: Option<String>,
    /// File name filtering
    pub file_name: Option<String>,
    /// New field for file extension filtering
    pub file_extension: Option<String>,
    /// Optional file extension list filtering, comma(,) separated values.
    pub file_extension_list: Option<String>,
    /// MD5 hash of the file content, used for filtering files by their content.
    pub md5: Option<String>,
    /// Optional time range filter for file creation.
    pub start_created_time: Option<DateTime<Local>>,
    pub end_created_time: Option<DateTime<Local>>,
    /// Optional time range filter for file modification.
    pub start_modified_time: Option<DateTime<Local>>,
    pub end_modified_time: Option<DateTime<Local>>,

    /// Minimum file md5 count
    pub min_md5_count: Option<i64>,
    /// Max file md5 count
    pub max_md5_count: Option<i64>,
    /// Optional order by field.
    pub order_by: Option<String>,
    /// Optional order direction, true for ascending, false for descending. Default is descending.
    pub order_asc: Option<bool>,

    /// Optional filter for duplicate files in a specific directory path. If set, if files within this directory duplicate those outside of it, they will be displayed.
    pub filter_dup_file_by_dir_path: Option<bool>,
}

/// Desk Settings
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Settings {
    /// System settings
    pub system: SystemSettings,
    /// User settings
    pub user: UserSettings,
    /// List settings
    pub list: ListSettings,
    /// Turn settings
    pub turn: TurnSettings,

    /// Desk settings
    pub desk: DeskSettings,

    /// Terminal settings
    pub terminal: TerminalSettings,

    /// Command line arguments
    pub args: Args,
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self {
            enable_ipv6: true,
            port: 8081,
            listen_addr_ipv4: "0.0.0.0".to_string(),
            listen_addr_ipv6: "::".to_string(),
            log_level: "info".to_string(),
            traceback: true,
            locale: None,
            open_browser_on_startup: true,
            signaling_url: None,
            client_id: None,
            telemetry_consent: None,
        }
    }
}

impl Settings {
    pub fn to_turn_server_config(&self) -> Result<turn_server::config::Config, DeskError> {
        let turn_config = turn_server::config::Config {
            turn: turn_server::config::Turn {
                realm: self.turn.realm.clone(),
                interfaces: self.turn.interfaces.clone(),
            },
            api: turn_server::config::Api {
                bind: "127.0.0.1:3000".parse()?,
            },
            log: turn_server::config::Log {
                level: self
                    .system
                    .log_level
                    .as_str()
                    .parse()
                    .unwrap_or(turn_server::config::LogLevel::Info),
            },
            auth: turn_server::config::Auth {
                static_credentials: self.turn.static_credentials.clone(),
                static_auth_secret: self.turn.static_auth_secret.clone(),
            },
        };
        Ok(turn_config)
    }
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            login_user_name: "admin".to_string(),
            login_password: "".to_string(),
        }
    }
}

impl Default for ListSettings {
    fn default() -> Self {
        Self {
            page_no: 1,
            page_count: 20,
            min_file_size: None,
            max_file_size: None,
            dir_path: None,
            file_name: None,
            file_extension: None,
            file_extension_list: None,
            md5: None,
            start_created_time: None,
            end_created_time: None,
            start_modified_time: None,
            end_modified_time: None,
            min_md5_count: Some(2),
            max_md5_count: None,
            order_by: None,
            order_asc: None,
            filter_dup_file_by_dir_path: None,
        }
    }
}

impl Settings {
    pub fn new(args: &Args) -> Result<Self, DeskError> {
        info!(
            "Loading config file from: {}",
            args.config_file_path.as_str()
        );
        // Load settings from config file
        let config = Config::builder()
            .add_source(File::with_name(args.config_file_path.as_str()).required(false))
            .add_source(Environment::with_prefix("DESK"))
            .build()?;
        let mut settings = config.try_deserialize::<Settings>()?;
        settings.args = args.clone();

        if settings.system.client_id.is_none() {
            let new_id = Uuid::new_v4().to_string();
            info!("Generated new client_id: {}", new_id);
            settings.system.client_id = Some(new_id);
            settings.save()?;
        }

        if let Some(ref locale) = settings.system.locale {
            rust_i18n::set_locale(locale);
            info!("Locale set to: {}", locale);
        }
        Ok(settings)
    }

    pub fn save(&self) -> Result<(), DeskError> {
        let mut config_file_path = PathBuf::from(self.args.config_file_path.as_str());
        config_file_path.set_extension("toml");
        // Save settings to config file
        let toml_str = toml::to_string(self)?;
        let parent_path = if let Some(parent_path) = config_file_path.parent() {
            parent_path
        } else {
            return DeskError::custom_error(
                DeskErrorCode::FILE_PATH_NOT_FOUND,
                &format!(
                    "the parent of path '{}' is not found",
                    config_file_path.display()
                ),
            );
        };
        if !parent_path.exists() {
            info!("Creating config directory: {}", parent_path.display());
            fs::create_dir_all(parent_path)?;
        }

        debug!(
            "Saving config to: {}, content: {}",
            config_file_path.display(),
            toml_str
        );
        if let Some(ref locale) = self.system.locale {
            rust_i18n::set_locale(locale);
            info!("Locale set to: {}", locale);
        }
        fs::write(&config_file_path, toml_str)?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct SharedSettings(pub RwLock<Settings>);

impl SharedSettings {
    pub fn from(setting: Settings) -> Self {
        SharedSettings(RwLock::new(setting))
    }
}

impl Deref for SharedSettings {
    type Target = RwLock<Settings>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
