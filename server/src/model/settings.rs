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

use crate::durable_file::{FileMode, durable_atomic_write};
use crate::error::DeskError;

mod ai_policy;
mod collection_policy;
mod log_config;
mod system;
mod turn_client;
mod user;
mod virtual_display;

pub use ai_policy::*;
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
    /// Edge-local AI execution policy: the local ceiling on how far a centrally
    /// authorized AI action may go on this device. Model provider credentials
    /// live on the central signaling brain, not here.
    #[serde(default)]
    pub ai_policy: AiExecutionPolicy,

    /// Edge-side gate on what evidence may leave this host for an AI model
    /// (`allow_logs` / `allow_screen`). Separate from `ai_policy` so a thin edge
    /// keeps the data-egress gate independently of the execution ceiling; applied
    /// locally on every collection. Default fail-closed (both `false`).
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

        let configured_locale = settings.system.locale.take();
        settings.system.locale = configured_locale
            .as_deref()
            .and_then(crate::locale::canonicalize)
            .map(str::to_string);
        if configured_locale.is_some() && settings.system.locale.is_none() {
            log::warn!(
                "Unsupported configured locale {:?}; falling back to {}",
                configured_locale,
                crate::locale::DEFAULT_LOCALE
            );
        }
        if configured_locale.as_deref() != settings.system.locale.as_deref()
            && settings.system.locale.is_some()
        {
            settings.save()?;
        }
        let locale = crate::locale::set_global_locale(
            settings
                .system
                .locale
                .as_deref()
                .unwrap_or(crate::locale::DEFAULT_LOCALE),
        )
        .expect("Settings locale was normalized");
        info!("Locale set to: {locale}");

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
        self.save_inner()
    }

    fn save_inner(&self) -> Result<(), DeskError> {
        let mut config_file_path = PathBuf::from(self.args.config_file_path.as_str());
        config_file_path.set_extension("toml");
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

        // One process writes this file at a time. The daemon owns the security
        // policy and the locale, and a session worker no longer writes either,
        // so there is nothing left to merge — but the lock still keeps two
        // concurrent saves from interleaving. The lock file is intentionally
        // stable and retained.
        let mut lock_path = config_file_path.clone();
        lock_path.set_extension("locale.lock");
        let lock_file = std::fs::File::options()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock_file.lock()?;

        let toml_str = toml::to_string(self)?;

        // Only the path is logged: the serialized TOML carries secrets
        // (api_key, signaling/manager tokens, session key) and would bypass the
        // per-field `Debug` redaction if printed in full.
        debug!("Saving config to: {}", config_file_path.display());
        // Replaced rather than truncated in place, so a failed save leaves the
        // previous configuration on disk instead of an empty or partial file —
        // every caller that reports failure relies on that. `Preserve` keeps the
        // file reachable by whichever role wrote it first: the daemon runs as
        // SYSTEM / root while a portable host runs as the desktop user, and both
        // read this same path.
        durable_atomic_write(&config_file_path, toml_str.as_bytes(), FileMode::Preserve)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_at(path: &std::path::Path, locale: &str) -> Settings {
        let mut settings = Settings::default();
        settings.args.config_file_path = path.to_string_lossy().into_owned();
        settings.system.locale = Some(locale.to_string());
        settings
    }

    /// A save writes what the caller holds, locale included. The daemon is the
    /// only writer of the locale now, so there is no second process whose stale
    /// copy this would have to defend against — the previous read-back-and-keep
    /// behaviour would instead make a genuine change silently not stick.
    #[test]
    fn a_save_persists_the_locale_it_carries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        settings_at(&path, "en-US").save().unwrap();

        let mut next = settings_at(&path, "zh-CN");
        next.log.log_level = "debug".to_string();
        next.save().unwrap();

        let loaded = Settings::load_readonly(&next.args).unwrap();
        assert_eq!(loaded.system.locale.as_deref(), Some("zh-CN"));
        assert_eq!(loaded.log.log_level, "debug");
    }

    /// STUN stopped being separately switchable, so configuration files written
    /// earlier still carry an `enable_stun` key. Loading one must ignore the
    /// stale key rather than fail — a rejected `[turn]` section would silently
    /// put the host back on default TURN settings.
    #[test]
    fn a_config_carrying_the_removed_stun_switch_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            path.with_extension("toml"),
            r#"
[turn]
realm = "example.org"
enable_stun = false
enable_turn = false
"#,
        )
        .unwrap();

        let args = Args {
            config_file_path: path.to_string_lossy().into_owned(),
            ..Args::default()
        };
        let loaded = Settings::load_readonly(&args).unwrap();

        assert_eq!(loaded.turn.realm, "example.org");
        assert!(
            !loaded.turn.enable_turn,
            "the switch that survived must still apply"
        );
    }

    /// The TURN switch decides whether a host relays, and it is read at startup,
    /// so a configuration that never mentions it has to land on "on".
    #[test]
    fn a_config_without_a_turn_section_enables_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(path.with_extension("toml"), "[system]\nport = 8080\n").unwrap();

        let args = Args {
            config_file_path: path.to_string_lossy().into_owned(),
            ..Args::default()
        };
        let loaded = Settings::load_readonly(&args).unwrap();

        assert!(loaded.turn.enable_turn);
    }

    /// Callers treat a failed `save` as "nothing changed" — the settings
    /// controllers report the error and leave the live values alone. That only
    /// holds if the file on disk is still the previous configuration rather
    /// than the empty file a truncating write would leave behind.
    #[test]
    fn a_failed_save_leaves_the_previous_configuration_on_disk() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config");
            let committed = settings_at(&path, "en-US");
            committed.save().unwrap();
            let before = std::fs::read_to_string(path.with_extension("toml")).unwrap();

            // An unwritable directory stands in for the disk filling up: the
            // replacement cannot be created, so the save has to fail.
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            // Root ignores directory permissions, so the save would succeed and
            // the assertion below would prove nothing.
            let ignores_permissions = std::fs::File::create(dir.path().join("probe")).is_ok();

            let outcome = if ignores_permissions {
                None
            } else {
                let mut changed = settings_at(&path, "en-US");
                changed.log.log_level = "trace".to_string();
                Some(changed.save())
            };

            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

            if let Some(outcome) = outcome {
                assert!(
                    outcome.is_err(),
                    "an unwritable directory must fail the save"
                );
                assert_eq!(
                    std::fs::read_to_string(path.with_extension("toml")).unwrap(),
                    before,
                    "the configuration file must survive a failed save intact"
                );
            }
        }
    }

    #[test]
    fn ordinary_save_does_not_change_the_process_locale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let committed = settings_at(&path, "en-US");
        committed.save().unwrap();
        crate::locale::set_global_locale("en-US").unwrap();

        let stale = settings_at(&path, "zh-CN");
        stale.save().unwrap();

        assert_eq!(crate::locale::current_locale(), "en-US");
    }
}
