use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Remote system settings — a subset of the desk server's system configuration
/// that can be queried and updated remotely via signaling.
///
/// This is intentionally a simplified view. Fields like `client_id` and
/// `telemetry_consent` that are local-only are excluded.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
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
    /// Signaling server url
    pub signaling_url: Option<String>,
    /// Auto start the application on system login
    pub auto_start: Option<bool>,
    /// API Token for connecting to manager's signaling server
    pub manager_api_token: Option<String>,
}
