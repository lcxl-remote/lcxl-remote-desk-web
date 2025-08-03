use std::{collections::HashMap, fs, ops::Deref, path::PathBuf};

use ::serde::{Deserialize, Serialize};
use chrono::{DateTime, Local};
use clap::Parser;
use config::{Config, Environment, File};
use log::{debug, info};
use tokio::sync::RwLock;
use turn_server::config::Interface;
use utoipa::{IntoParams, ToSchema};

use crate::{desk_error::DeskError, model::record_audio::SelectedAudioDevice};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Name of the person to greet
    #[arg(short, long, default_value = "conf/config")]
    config_file_path: String,
}
/// System settings for the application. This struct is used to load and save settings from a configuration file.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct SystemSettings {
    /// Path to the configuration file. If not specified, a new one will be created in the "conf" directory.
    pub config_file_path: String,
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

/// Desk settings
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct DeskSettings {
    /// Enable D3D debug mode
    pub enable_d3d_debug: bool,
    /// Video device index
    pub video_device_index: u32,
    /// Video encode bitrate in bps (bits per second)
    pub video_encode_bps: u32,
    /// Enable adaptive web page resolution
    pub adaptive_web_page_resolution: bool,
    /// Video zoom ratio (e.g., 50 for 50% zoom)
    pub video_zoom_ratio: u32,
    /// Enable mouse display on the screen
    pub show_mouse: bool,
    /// Video encoder name, None for auto detection
    pub video_encoder: Option<String>,
    /// Selected audio device
    pub audio_device: Option<SelectedAudioDevice>,
    /// Audio encoder name, None for auto detection
    pub audio_encoder: Option<String>,
}

impl Default for DeskSettings {
    fn default() -> Self {
        Self {
            enable_d3d_debug: false,
            video_device_index: 0,
            video_encode_bps: 10_1000_1000,
            adaptive_web_page_resolution: false,
            video_zoom_ratio: 100,
            show_mouse: true,
            video_encoder: None,
            audio_device: None,
            audio_encoder: None,
        }
    }
}

/// Turn Server Settings
/// See also `turn_server::config::Config`

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TurnSettings {
    /// turn server realm
    pub realm: String,

    /// turn server listen interfaces
    pub interfaces: Vec<Interface>,

    /// static user password
    ///
    /// This option can be used to specify the
    /// static identity authentication information used by the turn server for
    /// verification. Note: this is a high-priority authentication method, turn
    /// The server will try to use static authentication first, and then use
    /// external control service authentication.
    pub static_credentials: HashMap<String, String>,
    /// Static authentication key value (string) that applies only to the TURN
    /// REST API.
    ///
    /// If set, the turn server will not request external services via the HTTP
    /// Hooks API to obtain the key.
    pub static_auth_secret: Option<String>,
}

impl Default for TurnSettings {
    fn default() -> Self {
        Self {
            realm: "localhost".to_string(),
            interfaces: vec![],
            static_credentials: HashMap::new(),
            static_auth_secret: None,
        }
    }
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
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self {
            config_file_path: "conf/config".to_string(),
            enable_ipv6: true,
            port: 8081,
            listen_addr_ipv4: "0.0.0.0".to_string(),
            listen_addr_ipv6: "::".to_string(),
            log_level: "info".to_string(),
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
                bind: "127.0.0.1:3000".parse().unwrap(),
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
        settings.system.config_file_path = args.config_file_path.clone();
        Ok(settings)
    }

    pub fn save(&self) -> Result<(), DeskError> {
        let mut config_file_path = PathBuf::from(self.system.config_file_path.as_str());
        config_file_path.set_extension("toml");
        // Save settings to config file
        let toml_str = toml::to_string(self)?;
        if !config_file_path.parent().unwrap().exists() {
            info!(
                "Creating config directory: {}",
                config_file_path.parent().unwrap().display()
            );
            fs::create_dir_all(config_file_path.parent().unwrap())?;
        }

        debug!(
            "Saving config to: {}, content: {}",
            config_file_path.display(),
            toml_str
        );
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
