use std::{fs, ops::Deref, path::PathBuf};

use config::{Config, Environment, File};
use desk_signal_facade::model::{
    desk_settings::DeskSettings, security_settings::SecuritySettings, terminal::TerminalSettings,
};
use desk_turn::model::TurnSettings;
use desk_utils::error::DeskErrorCode;
use log::{debug, info};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::error::DeskError;

mod ai_model;
mod collection_policy;
mod log_config;
mod system;
mod turn_client;
mod user;
mod virtual_display;

pub use ai_model::*;
pub use collection_policy::*;
pub use log_config::*;
pub use system::*;
pub use turn_client::*;
pub use user::*;
pub use virtual_display::*;

/// Desk Settings
#[derive(Clone, Debug, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct Settings {
    /// System settings
    pub system: SystemSettings,
    /// Log settings
    pub log: LogSettings,
    /// User settings
    pub user: UserSettings,
    /// Turn server settings
    pub turn: TurnSettings,
    /// Turn client settings(desk server as a turn client)
    pub turn_client: TurnClientSettings,
    /// Desk settings
    pub desk: DeskSettings,

    /// Terminal settings
    pub terminal: TerminalSettings,
    /// Security settings for remote access permissions
    #[serde(default)]
    pub security: SecuritySettings,
    /// Virtual display (Windows IDD) settings
    #[serde(default)]
    pub virtual_display: VirtualDisplaySettings,
    /// AI model gateway settings. Top-level (not under `system`) so the model
    /// `api_key` never reaches the legacy `/settings` response or
    /// `RemoteSystemSettings`.
    #[serde(default)]
    pub ai_model: AiModelSettings,

    /// Edge-side gate on what evidence may leave this host for an AI model
    /// (`allow_logs` / `allow_screen`). Separate from `ai_model` so a thin edge
    /// keeps the gate without holding model credentials; applied locally on every
    /// collection. Default fail-closed (both `false`).
    #[serde(default)]
    pub collection_policy: CollectionPolicySettings,

    /// Command line arguments, come from clap and do not load from or save to config file
    #[serde(skip)]
    pub args: Args,
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
            .add_source(Environment::with_prefix("LRD"))
            .build()?;
        let mut settings = config.try_deserialize::<Settings>()?;
        settings.args = args.clone();
        if settings.system.get_client_id().is_err() {
            settings.system.generate_client_id();
            settings.save()?;
        }

        if settings.turn.static_auth_secret.is_none() {
            let new_secret = Uuid::new_v4().to_string().replace("-", "");
            info!("Generated new TURN static_auth_secret");
            settings.turn.static_auth_secret = Some(new_secret);
            settings.save()?;
        }

        if settings.system.tauri_ipc_token.is_none() {
            let token = Uuid::new_v4().to_string();
            info!("Generated new tauri_ipc_token");
            settings.system.tauri_ipc_token = Some(token);
            settings.save()?;
        }

        if settings.system.session_secret_key.is_none() {
            // Build 144+ bytes of entropy from 4 UUIDs; Key::derive_from handles the rest
            let key_material = format!(
                "{}{}{}{}",
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4(),
                Uuid::new_v4()
            );
            info!("Generated new session_secret_key");
            settings.system.session_secret_key = Some(key_material);
            settings.save()?;
        }

        if let Some(ref locale) = settings.system.locale {
            rust_i18n::set_locale(locale);
            info!("Locale set to: {}", locale);
        }

        // Clamp hand-edited adaptive-resolution knobs to a safe range
        // (warn-log per clamped field). Must happen before any caller
        // hands the settings to the daemon / router / Init reply so the
        // browser only ever sees sanitised values.
        settings.virtual_display.sanitize();

        Ok(settings)
    }

    /// Load settings from the config file **without** the token-generation /
    /// save side effects of [`Settings::new`]. Used where a fresh read of the
    /// persisted policy is needed (e.g. the MCP server re-checking live
    /// permission on each tool call) and writing back is undesirable.
    pub fn load_readonly(args: &Args) -> Result<Self, DeskError> {
        let config = Config::builder()
            .add_source(File::with_name(args.config_file_path.as_str()).required(false))
            .add_source(Environment::with_prefix("LRD"))
            .build()?;
        let mut settings = config.try_deserialize::<Settings>()?;
        settings.args = args.clone();
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

        // Only the path is logged: the serialized TOML carries secrets
        // (api_key, signaling/manager tokens, session key) and would bypass the
        // per-field `Debug` redaction if printed in full.
        debug!("Saving config to: {}", config_file_path.display());
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
