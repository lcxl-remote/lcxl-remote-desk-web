use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Remote system settings — a subset of the desk server's system configuration
/// that can be queried and updated remotely via signaling.
///
/// This is intentionally a simplified view. Fields like `client_id` and
/// `telemetry_consent` that are local-only are excluded.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Serialize,
    ToSchema,
    Default,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
#[serde(default)]
pub struct RemoteSystemSettings {
    /// Enable IPv6 support
    pub enable_ipv6: bool,
    /// Port number for the server to bind to
    pub port: u16,
    /// Listen ipv4 address for the server to bind to
    pub listen_addr_ipv4: String,
    /// Listen ipv6 address for the server to bind to
    pub listen_addr_ipv6: String,
    /// Optional locale setting (e.g., "en", "zh-CN")
    pub locale: Option<String>,
    /// Remote signaling server url
    pub signaling_url: Option<String>,
    /// Token for authenticating with the remote signaling server
    pub signaling_token: Option<String>,
    /// Remote manager server url
    pub manager_url: Option<String>,
    /// Auto start the application on system login
    pub auto_start: Option<bool>,
    /// API Token for connecting to manager's signaling server
    pub manager_api_token: Option<String>,
}

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn remote_system_settings_round_trips_wincode() {
        let original = RemoteSystemSettings {
            enable_ipv6: true,
            port: 8443,
            listen_addr_ipv4: "0.0.0.0".to_string(),
            listen_addr_ipv6: "::".to_string(),
            locale: Some("zh-CN".to_string()),
            signaling_url: Some("wss://signal.example".to_string()),
            signaling_token: Some("tok".to_string()),
            manager_url: Some("https://mgr.example".to_string()),
            auto_start: Some(true),
            manager_api_token: Some("mtok".to_string()),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: RemoteSystemSettings =
            wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.enable_ipv6, original.enable_ipv6);
        assert_eq!(back.port, 8443);
        assert_eq!(back.locale.as_deref(), Some("zh-CN"));
        assert_eq!(back.auto_start, Some(true));
        assert_eq!(back.manager_api_token.as_deref(), Some("mtok"));
    }

    /// `RemoteSystemSettings` defaults every field. Verify the
    /// `Default::default()` instance survives a wincode round-trip
    /// — this is the "all-None / all-zero" extreme that exercises
    /// the optional fields' `None`-tag encoding.
    #[test]
    fn remote_system_settings_default_round_trips_wincode() {
        let original = RemoteSystemSettings::default();
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: RemoteSystemSettings =
            wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.enable_ipv6, original.enable_ipv6);
        assert_eq!(back.port, original.port);
        assert!(back.locale.is_none());
        assert!(back.auto_start.is_none());
    }
}
