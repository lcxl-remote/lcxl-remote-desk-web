use clap::Parser;
use desk_utils::error::DeskErrorCode;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::DeskError;

/// The startup mode lives in the shared signaling model, not here: a host
/// reports it to control ends inside `SystemInfo`, and the manager reports its
/// own the same way, so one declaration keeps the two from drifting. Re-exported
/// under the settings path it has always been imported from.
pub use desk_signal_facade::model::startup_mode::StartupMode;

/// Command line arguments
#[derive(Parser, Debug, Clone, Default, Serialize, Deserialize)]
#[command(ignore_errors = true, version, about, long_about = None, group(
    clap::ArgGroup::new("frontend_mode")
        .args(["prod_frontend", "dev_frontend"])
        .multiple(false) // only one of them can be set
))]
pub struct Args {
    /// Config file path
    #[clap(short, long)]
    pub config_file_path: Option<PathBuf>,

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
// `Debug` is hand-written (not derived) so the secret fields
// (`signaling_token` / `manager_api_token` / `local_signaling_token` /
// `tauri_ipc_token` / `session_secret_key`) are masked. These are credentials
// — `local_signaling_token` is now also a host signaling credential — so the
// many `{:?}` log sites (query/update settings, sysinfo, startup) must never
// leak the raw values.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
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
    /// Whether the host should keep the manager link connected. This is a
    /// host-local UI toggle that lets a user disable the manager connection
    /// without clearing `manager_url` / `manager_api_token`, so the address is
    /// retained for a later re-enable. `None` / `Some(true)` = enabled;
    /// `Some(false)` = explicitly disabled. Host-local only: no signaling frame
    /// can read or write the host's system settings.
    pub manager_enabled: Option<bool>,
    /// Whether the local shell shows ongoing remote-access status.
    pub host_access_indicator_enabled: bool,
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

    /// Whether the host refuses to dial a remote signaling server / manager over
    /// a plaintext scheme (`ws://` / `http://`) when the target resolves to a
    /// *public* (internet-routable) address. Defaults to `true` (secure): a
    /// public plaintext dial would carry the API token and all signaling in the
    /// clear across untrusted networks. Loopback / private / LAN targets are
    /// always reachable over plaintext regardless of this switch (a self-hosted
    /// signaling server on a LAN commonly has no TLS), and the cloud-metadata
    /// hard floor is always blocked. Set to `false` only as a deliberate escape
    /// hatch for a trusted-network deployment that intentionally runs a public
    /// endpoint without TLS. A plain `bool` (not `Option`) so an update payload
    /// that omits it fails secure to `true`.
    pub require_secure_signaling: bool,
}

/// Render an `Option<String>` secret for `Debug`: keep the `Some`/`None`
/// distinction (useful for diagnosing "is it set?") but never the value.
fn redacted(value: &Option<String>) -> &'static str {
    match value {
        Some(_) => "Some(\"***\")",
        None => "None",
    }
}

impl std::fmt::Debug for SystemSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemSettings")
            .field("enable_ipv6", &self.enable_ipv6)
            .field("port", &self.port)
            .field("listen_addr_ipv4", &self.listen_addr_ipv4)
            .field("listen_addr_ipv6", &self.listen_addr_ipv6)
            .field("locale", &self.locale)
            .field("signaling_url", &self.signaling_url)
            .field(
                "signaling_token",
                &format_args!("{}", redacted(&self.signaling_token)),
            )
            .field("manager_url", &self.manager_url)
            .field("manager_enabled", &self.manager_enabled)
            .field(
                "host_access_indicator_enabled",
                &self.host_access_indicator_enabled,
            )
            .field("client_id", &self.client_id)
            .field("telemetry_consent", &self.telemetry_consent)
            .field("auto_start", &self.auto_start)
            .field(
                "manager_api_token",
                &format_args!("{}", redacted(&self.manager_api_token)),
            )
            .field(
                "local_signaling_token",
                &format_args!("{}", redacted(&self.local_signaling_token)),
            )
            .field(
                "tauri_ipc_token",
                &format_args!("{}", redacted(&self.tauri_ipc_token)),
            )
            .field(
                "session_secret_key",
                &format_args!("{}", redacted(&self.session_secret_key)),
            )
            .field(
                "worker_heartbeat_watchdog_enabled",
                &self.worker_heartbeat_watchdog_enabled,
            )
            .field(
                "worker_heartbeat_timeout_secs",
                &self.worker_heartbeat_timeout_secs,
            )
            .field(
                "webrtc_ice_disconnected_timeout_secs",
                &self.webrtc_ice_disconnected_timeout_secs,
            )
            .field(
                "webrtc_ice_failed_timeout_secs",
                &self.webrtc_ice_failed_timeout_secs,
            )
            .field("require_secure_signaling", &self.require_secure_signaling)
            .finish()
    }
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
    /// token, the Tauri IPC token and the session signing key.
    ///
    /// `client_id` and `local_signaling_token` are restored only when the
    /// incoming value is absent, so a payload that legitimately carries them
    /// (they are present in the settings response) still takes effect.
    ///
    /// `tauri_ipc_token` and `session_secret_key` are restored
    /// **unconditionally**, overriding any incoming value. These two are dropped
    /// from the settings response ([`Self::without_internal_secrets`]), so the
    /// console never holds them to send back — an incoming value can only be a
    /// spurious or malicious one. Letting an explicit value win would let an
    /// authenticated settings write pin an attacker-known cookie signing key or
    /// IPC identity, which survives a restart and enables session forgery.
    ///
    /// `telemetry_consent` is owned by the dedicated consent endpoint and is
    /// also restored unconditionally. This prevents an unrelated settings save
    /// from clearing consent when the general form omits the field, and prevents
    /// that broad endpoint from bypassing the dedicated consent workflow.
    pub fn preserve_internal_fields(&mut self, previous: &SystemSettings) {
        if self.client_id.is_none() {
            self.client_id = previous.client_id.clone();
        }
        if self.local_signaling_token.is_none() {
            self.local_signaling_token = previous.local_signaling_token.clone();
        }
        self.tauri_ipc_token = previous.tauri_ipc_token.clone();
        self.session_secret_key = previous.session_secret_key.clone();
        self.telemetry_consent = previous.telemetry_consent;
    }

    /// A copy safe to serialize into an HTTP response: the secrets the console
    /// has no use for are dropped.
    ///
    /// Redacting here rather than with `skip_serializing` on the fields is
    /// deliberate — the same `Serialize` impl writes `config.toml`
    /// ([`Settings::save`](crate::model::settings::Settings)), so skipping a
    /// field would stop persisting it. The session key and IPC token would then
    /// be regenerated on every restart, invalidating every session and changing
    /// the IPC identity.
    ///
    /// Only the two purely-internal secrets are dropped. The other three the
    /// masked `Debug` covers must stay in the response, and each for its own
    /// reason: `signaling_token` and `manager_api_token` are typed by the user
    /// into the connection form, which reloads them to edit; and
    /// `local_signaling_token` is shown deliberately, so the operator can
    /// configure a desk server against the co-located signaling server. Dropping
    /// any of those breaks a feature rather than closing an exposure.
    ///
    /// Round-tripping stays safe because both dropped fields arrive back as
    /// `None` and are restored by [`Self::preserve_internal_fields`].
    pub fn without_internal_secrets(&self) -> SystemSettings {
        SystemSettings {
            tauri_ipc_token: None,
            session_secret_key: None,
            ..self.clone()
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
            manager_enabled: None,
            host_access_indicator_enabled: true,
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
            // Secure by default: refuse public plaintext signaling dials.
            require_secure_signaling: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_access_indicator_defaults_to_enabled() {
        assert!(SystemSettings::default().host_access_indicator_enabled);
        let decoded: SystemSettings = toml::from_str("").expect("decode defaults");
        assert!(decoded.host_access_indicator_enabled);
    }

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
    fn preserve_internal_fields_respects_explicit_response_visible_values() {
        // `client_id` and `local_signaling_token` are returned in the settings
        // response, so a payload may legitimately carry them; an explicit value
        // still wins.
        let previous = SystemSettings {
            client_id: Some("old".to_string()),
            local_signaling_token: Some("old-lst".to_string()),
            ..SystemSettings::default()
        };
        let mut incoming = SystemSettings {
            client_id: Some("new".to_string()),
            local_signaling_token: Some("new-lst".to_string()),
            ..SystemSettings::default()
        };

        incoming.preserve_internal_fields(&previous);

        assert_eq!(incoming.client_id.as_deref(), Some("new"));
        assert_eq!(incoming.local_signaling_token.as_deref(), Some("new-lst"));
    }

    #[test]
    fn preserve_internal_fields_rejects_explicit_internal_secret_overrides() {
        // The two purely-internal secrets are redacted from the response, so an
        // incoming value is spurious/malicious. An explicit override must NOT win
        // — otherwise an authenticated settings write could pin an attacker-known
        // session signing key / IPC token that survives a restart.
        let previous = SystemSettings {
            tauri_ipc_token: Some("real-ipc".to_string()),
            session_secret_key: Some("real-session-key".to_string()),
            ..SystemSettings::default()
        };
        let mut incoming = SystemSettings {
            tauri_ipc_token: Some("attacker-ipc".to_string()),
            session_secret_key: Some("attacker-key".to_string()),
            ..SystemSettings::default()
        };

        incoming.preserve_internal_fields(&previous);

        assert_eq!(incoming.tauri_ipc_token.as_deref(), Some("real-ipc"));
        assert_eq!(
            incoming.session_secret_key.as_deref(),
            Some("real-session-key")
        );
    }

    #[test]
    fn preserve_internal_fields_keeps_telemetry_consent_when_omitted() {
        let previous = SystemSettings {
            telemetry_consent: Some(true),
            ..SystemSettings::default()
        };
        let mut incoming = SystemSettings::default();

        incoming.preserve_internal_fields(&previous);

        assert_eq!(incoming.telemetry_consent, Some(true));
    }

    #[test]
    fn preserve_internal_fields_rejects_telemetry_consent_override() {
        let previous = SystemSettings {
            telemetry_consent: Some(true),
            ..SystemSettings::default()
        };
        let mut incoming = SystemSettings {
            telemetry_consent: Some(false),
            ..SystemSettings::default()
        };

        incoming.preserve_internal_fields(&previous);

        assert_eq!(incoming.telemetry_consent, Some(true));
    }

    #[test]
    fn api_view_drops_only_the_purely_internal_secrets() {
        let settings = SystemSettings {
            signaling_token: Some("sig-secret".to_string()),
            manager_api_token: Some("mgr-secret".to_string()),
            local_signaling_token: Some("local-secret".to_string()),
            tauri_ipc_token: Some("tauri-secret".to_string()),
            session_secret_key: Some("session-secret".to_string()),
            ..SystemSettings::default()
        };

        let view = settings.without_internal_secrets();

        assert_eq!(view.tauri_ipc_token, None);
        assert_eq!(view.session_secret_key, None);
        // The console needs these three: two are edited in the connection form,
        // and the local token is displayed on purpose.
        assert_eq!(view.signaling_token.as_deref(), Some("sig-secret"));
        assert_eq!(view.manager_api_token.as_deref(), Some("mgr-secret"));
        assert_eq!(view.local_signaling_token.as_deref(), Some("local-secret"));
    }

    /// The response body must not contain the dropped values in any form — the
    /// point is what crosses the wire, not what the struct field holds.
    #[test]
    fn serialized_api_view_carries_no_internal_secret() {
        let settings = SystemSettings {
            tauri_ipc_token: Some("tauri-secret".to_string()),
            session_secret_key: Some("session-secret".to_string()),
            ..SystemSettings::default()
        };

        let json = serde_json::to_string(&settings.without_internal_secrets()).unwrap();

        assert!(!json.contains("tauri-secret"), "leaked in: {json}");
        assert!(!json.contains("session-secret"), "leaked in: {json}");
    }

    /// Redaction must not reach persistence. `config.toml` is written through
    /// this same `Serialize`, so a `skip_serializing` on the fields would stop
    /// storing them — regenerating the session key on every restart (logging
    /// everyone out) and changing the IPC identity.
    #[test]
    fn persistence_still_serializes_the_secrets() {
        let settings = SystemSettings {
            tauri_ipc_token: Some("tauri-secret".to_string()),
            session_secret_key: Some("session-secret".to_string()),
            ..SystemSettings::default()
        };

        let toml_str = toml::to_string(&settings).unwrap();

        assert!(toml_str.contains("tauri-secret"));
        assert!(toml_str.contains("session-secret"));
    }

    /// The console reads the redacted view and posts the whole struct back, so
    /// the dropped fields return as `None`. They must survive that round trip.
    #[test]
    fn round_tripping_the_api_view_keeps_the_stored_secrets() {
        let stored = SystemSettings {
            tauri_ipc_token: Some("tauri-secret".to_string()),
            session_secret_key: Some("session-secret".to_string()),
            ..SystemSettings::default()
        };

        let mut incoming = stored.without_internal_secrets();
        incoming.preserve_internal_fields(&stored);

        assert_eq!(incoming.tauri_ipc_token.as_deref(), Some("tauri-secret"));
        assert_eq!(
            incoming.session_secret_key.as_deref(),
            Some("session-secret")
        );
    }

    #[test]
    fn debug_masks_secret_fields() {
        let settings = SystemSettings {
            signaling_token: Some("sig-secret".to_string()),
            manager_api_token: Some("mgr-secret".to_string()),
            local_signaling_token: Some("local-secret".to_string()),
            tauri_ipc_token: Some("tauri-secret".to_string()),
            session_secret_key: Some("session-secret".to_string()),
            ..SystemSettings::default()
        };

        let rendered = format!("{:?}", settings);

        // No raw secret value leaks through Debug (covers every {:?} log site).
        for secret in [
            "sig-secret",
            "mgr-secret",
            "local-secret",
            "tauri-secret",
            "session-secret",
        ] {
            assert!(
                !rendered.contains(secret),
                "Debug output leaked secret `{secret}`: {rendered}"
            );
        }
        // The Some/None distinction is preserved for diagnostics.
        assert!(rendered.contains("local_signaling_token: Some(\"***\")"));
        assert!(rendered.contains("session_secret_key: Some(\"***\")"));
    }

    #[test]
    fn debug_renders_unset_secrets_as_none() {
        let settings = SystemSettings::default();
        let rendered = format!("{:?}", settings);
        assert!(rendered.contains("local_signaling_token: None"));
        assert!(rendered.contains("manager_api_token: None"));
    }

    #[test]
    fn manager_enabled_defaults_to_none_and_is_not_a_secret() {
        let settings = SystemSettings::default();
        assert_eq!(settings.manager_enabled, None);
        // `manager_enabled` is not a credential, so Debug shows the real value.
        let rendered = format!("{:?}", settings);
        assert!(rendered.contains("manager_enabled: None"));
    }

    #[test]
    fn manager_enabled_disabled_survives_toml_round_trip() {
        let settings = SystemSettings {
            manager_enabled: Some(false),
            ..SystemSettings::default()
        };
        let serialized = toml::to_string(&settings).expect("serialize");
        let reloaded: SystemSettings = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(reloaded.manager_enabled, Some(false));
    }

    #[test]
    fn require_secure_signaling_defaults_true_and_fails_secure_when_absent() {
        // Secure by default.
        assert!(SystemSettings::default().require_secure_signaling);
        // A config / update payload that omits the field (older console, hand-edited
        // TOML) deserializes to the secure default rather than `false`.
        let reloaded: SystemSettings = toml::from_str("port = 8081").expect("deserialize");
        assert!(
            reloaded.require_secure_signaling,
            "omitted field must fail secure to true"
        );
    }

    #[test]
    fn require_secure_signaling_disabled_survives_toml_round_trip() {
        // The deliberate escape hatch persists across a save/load.
        let settings = SystemSettings {
            require_secure_signaling: false,
            ..SystemSettings::default()
        };
        let serialized = toml::to_string(&settings).expect("serialize");
        let reloaded: SystemSettings = toml::from_str(&serialized).expect("deserialize");
        assert!(!reloaded.require_secure_signaling);
    }
}
