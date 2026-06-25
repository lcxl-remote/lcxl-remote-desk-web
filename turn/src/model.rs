use std::{collections::HashMap, str::FromStr, sync::Arc, time::Instant};

use desk_signal_facade::model::signal::LcxlRTCIceServer;
use serde::{Deserialize, Serialize};
use turn::server::Server;
use utoipa::{IntoParams, ToSchema};

use crate::error::DeskTurnError;

#[rustfmt::skip]
pub static SOFTWARE: &str = concat!(
    "lcxl-remote-desk-turn-rs.",
    env!("CARGO_PKG_VERSION")
);

/// TURN API state.
pub struct TurnApiState {
    pub uptime: Instant,
    pub statistics: Arc<std::sync::RwLock<Statistics>>,
    pub settings: TurnSettings,
    pub server: Server,
}

#[derive(Default, Debug)]
pub struct Statistics {
    pub global: TurnSessionStatistics,
    pub sessions: HashMap<std::net::SocketAddr, TurnSessionStatistics>,
    /// Per-connection cumulative counters, keyed by the signaling
    /// `connection_id` resolved at TURN auth time. This is the dimension a
    /// usage flusher diffs and persists; it is independent of `SocketAddr`
    /// (which drifts with NAT rebinding).
    pub by_connection: HashMap<String, TurnSessionStatistics>,
    /// Maps a TURN client's source address to its `connection_id`, populated by
    /// the auth handler. `TrackedUdpConn` consults this to fold per-address
    /// bytes into `by_connection`.
    addr_to_conn: HashMap<std::net::SocketAddr, String>,
}

impl Statistics {
    /// Bind a TURN client's `src_addr` to its `connection_id` (called at auth
    /// time). Last-writer-wins so that NAT rebinding / address reuse rebinds the
    /// address to whichever connection most recently authenticated from it.
    pub fn record_binding(&mut self, src_addr: std::net::SocketAddr, connection_id: &str) {
        self.addr_to_conn
            .insert(src_addr, connection_id.to_string());
    }

    /// Independent-clone snapshot of the per-connection cumulative counters, for
    /// a flusher to diff against its own baseline without holding the lock.
    pub fn snapshot_by_connection(&self) -> HashMap<String, TurnSessionStatistics> {
        self.by_connection.clone()
    }

    /// The `connection_id` currently bound to `addr`, if any. Primarily for
    /// diagnostics and tests asserting the auth handler bound the right key.
    pub fn connection_of(&self, addr: &std::net::SocketAddr) -> Option<&str> {
        self.addr_to_conn.get(addr).map(String::as_str)
    }

    /// Fold a received-direction sample into `by_connection` when `addr` has a
    /// known binding. The per-address / global counters are updated by the
    /// caller; this only handles the connection dimension.
    pub fn record_recv(&mut self, addr: std::net::SocketAddr, bytes: usize) {
        if let Some(conn_id) = self.addr_to_conn.get(&addr) {
            let entry = self.by_connection.entry(conn_id.clone()).or_default();
            entry.received_bytes += bytes;
            entry.received_pkts += 1;
        }
    }

    /// Fold a sent-direction sample into `by_connection` when `target` has a
    /// known binding.
    pub fn record_send(&mut self, target: std::net::SocketAddr, bytes: usize) {
        if let Some(conn_id) = self.addr_to_conn.get(&target) {
            let entry = self.by_connection.entry(conn_id.clone()).or_default();
            entry.send_bytes += bytes;
            entry.send_pkts += 1;
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TurnTransport {
    TCP = 0,
    UDP = 1,
}

impl FromStr for TurnTransport {
    type Err = DeskTurnError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "udp" => Self::UDP,
            "tcp" => Self::TCP,
            _ => return Err(DeskTurnError::IllegalTransport(value.to_string())),
        })
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, ToSchema)]
pub struct TurnInterface {
    pub transport: TurnTransport,
    /// turn server listen address
    pub listen: String,
    /// external address
    ///
    /// specify the node external address and port.
    /// for the case of exposing the service to the outside,
    /// you need to manually specify the server external IP
    /// address and service listening port.
    pub external: String,
}

#[derive(Serialize, ToSchema)]
pub struct TurnInfo {
    pub software: String,
    pub uptime: u64,
    pub interfaces: Vec<TurnInterface>,
    pub port_capacity: usize,
    pub port_allocated: usize,
}

#[derive(Deserialize, IntoParams)]
pub struct TurnQueryParams {
    pub address: String,
    pub interface: String,
}

#[derive(Serialize, ToSchema)]
pub struct TurnSession {
    pub username: String,
    pub permissions: Vec<u16>,
    pub channels: Vec<u16>,
    pub port: Option<u16>,
    pub expires: u64,
}

#[derive(Serialize, ToSchema, Default, Debug, Clone)]
pub struct TurnSessionStatistics {
    pub received_bytes: usize,
    pub send_bytes: usize,
    pub received_pkts: usize,
    pub send_pkts: usize,
    pub error_pkts: usize,
}

/// Turn Server Settings
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct TurnSettings {
    /// turn server realm
    pub realm: String,

    /// turn server listen interfaces
    pub interfaces: Vec<TurnInterface>,

    /// static user password
    pub static_credentials: HashMap<String, String>,

    /// Static authentication key value (string) that applies only to the TURN
    /// REST API.
    pub static_auth_secret: Option<String>,

    /// enable stun server
    pub enable_stun: bool,

    /// enable turn server
    pub enable_turn: bool,

    /// Minimum port for TURN relay
    pub relay_min_port: u16,

    /// Maximum port for TURN relay
    pub relay_max_port: u16,
}

impl TurnSettings {
    /// `turn:{external}?transport=...` URLs for every configured interface.
    fn turn_urls(&self) -> Vec<String> {
        self.interfaces
            .iter()
            .map(|interface| {
                format!(
                    "turn:{}?transport={}",
                    interface.external,
                    if interface.transport == TurnTransport::UDP {
                        "udp"
                    } else {
                        "tcp"
                    }
                )
            })
            .collect()
    }

    pub fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer {
        LcxlRTCIceServer {
            urls: self.turn_urls(),
            username: username.to_owned(),
            credential: credential.to_owned(),
        }
    }

    /// Build an ICE server carrying a freshly-signed TURN REST credential
    /// (username `{expiration}:{name}`, password `HMAC(secret, username)`) valid
    /// for `ttl_secs`. Returns `None` when there is no `static_auth_secret` or no
    /// TURN interface to advertise, so callers never inject an unusable entry.
    pub fn get_rest_ice_servers(&self, name: &str, ttl_secs: u64) -> Option<LcxlRTCIceServer> {
        let secret = self.static_auth_secret.as_ref()?;
        let urls = self.turn_urls();
        if urls.is_empty() {
            return None;
        }
        let (username, credential) =
            crate::utils::generate_turn_credentials(secret, name, ttl_secs);
        Some(LcxlRTCIceServer {
            urls,
            username,
            credential,
        })
    }
}

#[cfg(test)]
mod statistics_tests {
    use super::*;

    fn addr(port: u16) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn bound_addr_folds_into_by_connection() {
        let mut stats = Statistics::default();
        stats.record_binding(addr(1000), "conn-a");

        stats.record_recv(addr(1000), 100);
        stats.record_send(addr(1000), 40);

        let conn = stats.by_connection.get("conn-a").expect("connection entry");
        assert_eq!(conn.received_bytes, 100);
        assert_eq!(conn.received_pkts, 1);
        assert_eq!(conn.send_bytes, 40);
        assert_eq!(conn.send_pkts, 1);
    }

    #[test]
    fn unbound_addr_does_not_touch_by_connection() {
        let mut stats = Statistics::default();
        stats.record_recv(addr(2000), 100);
        stats.record_send(addr(2000), 40);
        assert!(stats.by_connection.is_empty());
    }

    #[test]
    fn addr_reuse_is_last_writer_wins() {
        let mut stats = Statistics::default();
        stats.record_binding(addr(3000), "conn-old");
        stats.record_recv(addr(3000), 10);
        // Same address rebinds to a new connection (NAT reuse).
        stats.record_binding(addr(3000), "conn-new");
        stats.record_recv(addr(3000), 50);

        assert_eq!(
            stats.by_connection.get("conn-old").unwrap().received_bytes,
            10
        );
        assert_eq!(
            stats.by_connection.get("conn-new").unwrap().received_bytes,
            50
        );
    }

    #[test]
    fn snapshot_is_independent_clone() {
        let mut stats = Statistics::default();
        stats.record_binding(addr(4000), "conn-x");
        stats.record_recv(addr(4000), 25);

        let snap = stats.snapshot_by_connection();
        // Mutating the live stats must not change the snapshot.
        stats.record_recv(addr(4000), 75);

        assert_eq!(snap.get("conn-x").unwrap().received_bytes, 25);
        assert_eq!(
            stats.by_connection.get("conn-x").unwrap().received_bytes,
            100
        );
    }
}

#[cfg(test)]
mod rest_ice_server_tests {
    use super::*;

    fn settings(secret: Option<&str>, with_interface: bool) -> TurnSettings {
        let interfaces = if with_interface {
            vec![TurnInterface {
                listen: "0.0.0.0:3478".to_owned(),
                external: "192.168.50.5:3478".to_owned(),
                transport: TurnTransport::UDP,
            }]
        } else {
            vec![]
        };
        TurnSettings {
            interfaces,
            static_auth_secret: secret.map(str::to_owned),
            ..TurnSettings::default()
        }
    }

    #[test]
    fn none_without_secret_or_interface() {
        assert!(
            settings(None, true)
                .get_rest_ice_servers("host-1", 60)
                .is_none()
        );
        assert!(
            settings(Some("s"), false)
                .get_rest_ice_servers("host-1", 60)
                .is_none()
        );
    }

    #[test]
    fn some_with_secret_and_interface() {
        let ice = settings(Some("s"), true)
            .get_rest_ice_servers("host-1", 60)
            .expect("ice server");
        assert_eq!(ice.urls, vec!["turn:192.168.50.5:3478?transport=udp"]);
        // username = "{expiration}:host-1"
        assert!(ice.username.ends_with(":host-1"));
        assert!(
            ice.username
                .split(':')
                .next()
                .unwrap()
                .parse::<u64>()
                .is_ok()
        );
    }
}

#[async_trait::async_trait]
impl desk_signal_facade::model::signal::TurnProvider for TurnSettings {
    async fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer {
        // Delegates to the inherent (sync, pure) method of the same name;
        // inherent methods take precedence in resolution, so this does not
        // recurse into the trait method.
        self.get_ice_servers(username, credential)
    }

    async fn get_rest_ice_servers(&self, name: &str, ttl_secs: u64) -> Option<LcxlRTCIceServer> {
        self.get_rest_ice_servers(name, ttl_secs)
    }
}

impl Default for TurnSettings {
    fn default() -> Self {
        Self {
            realm: "localhost".to_string(),
            interfaces: vec![],
            static_credentials: HashMap::new(),
            static_auth_secret: None,
            enable_stun: true,
            enable_turn: false,
            relay_min_port: 50000,
            relay_max_port: 50050,
        }
    }
}
