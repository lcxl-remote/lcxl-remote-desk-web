use std::{collections::HashMap, str::FromStr, sync::Arc, time::Instant};

use desk_signal_facade::model::signal::LcxlRTCIceServer;
use serde::{Deserialize, Serialize};
use turn::server::Server;
use utoipa::{IntoParams, ToSchema};

use crate::error::DeskTurnError;
use crate::interface::RejectedTurnInterface;

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

    /// Fold a received-direction sample of class `class` into `by_connection`
    /// when `addr` has a known binding. The per-address / global counters are
    /// updated by the caller; this only handles the connection dimension.
    pub fn record_recv(
        &mut self,
        addr: std::net::SocketAddr,
        bytes: usize,
        class: TurnTrafficClass,
    ) {
        if let Some(conn_id) = self.addr_to_conn.get(&addr) {
            self.by_connection
                .entry(conn_id.clone())
                .or_default()
                .add_recv(bytes, class);
        }
    }

    /// Fold a sent-direction sample of class `class` into `by_connection` when
    /// `target` has a known binding.
    pub fn record_send(
        &mut self,
        target: std::net::SocketAddr,
        bytes: usize,
        class: TurnTrafficClass,
    ) {
        if let Some(conn_id) = self.addr_to_conn.get(&target) {
            self.by_connection
                .entry(conn_id.clone())
                .or_default()
                .add_send(bytes, class);
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

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq, ToSchema)]
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

/// Why this process is or is not relaying right now.
///
/// "Not relaying" is an answer, not a failure, so the states name the reason
/// instead of collapsing into one error: an operator who switched TURN off, a
/// startup mode that never hosts it, and a host that cannot start the runtime
/// need different things done about them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TurnRuntimeState {
    /// A runtime is serving; `interfaces` and `uptime_secs` describe it.
    Running,
    /// Switched off by the operator.
    Disabled,
    /// This startup mode never hosts a TURN runtime.
    Unsupported,
    /// Switched on, but there is no interface to serve on.
    NotConfigured,
    /// Meant to run and not serving yet, with nothing having gone wrong. A save
    /// returns before the supervisor has bound anything, so this is the normal
    /// answer for the moment right after one.
    Starting,
    /// Meant to run and could not; `last_error` says why, and the supervisor
    /// keeps retrying.
    Failed,
}

/// Runtime status of the TURN service on this host.
///
/// Reports what is *running*, never what is configured — the configuration is
/// served by the settings endpoint, and conflating the two is how an operator
/// ends up believing a relay is up because it was saved.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TurnRuntimeInfo {
    pub state: TurnRuntimeState,
    /// Build identifier of this TURN implementation.
    pub software: String,
    /// The interfaces the running runtime serves; empty unless
    /// [`TurnRuntimeState::Running`].
    pub interfaces: Vec<TurnInterface>,
    /// Configured interfaces this host refuses to serve, and why.
    ///
    /// Reported in every state, because it is exactly when the runtime is *not*
    /// running that the reason matters most — a host whose every interface was
    /// rejected reports [`TurnRuntimeState::NotConfigured`], which on its own
    /// would read as "you configured nothing" to an operator who configured
    /// three.
    pub rejected_interfaces: Vec<RejectedTurnInterface>,
    /// Seconds since the running runtime started; `None` unless
    /// [`TurnRuntimeState::Running`].
    pub uptime_secs: Option<u64>,
    /// Why the last start attempt failed; `None` unless
    /// [`TurnRuntimeState::Failed`].
    pub last_error: Option<String>,
}

#[derive(Deserialize, IntoParams)]
pub struct TurnQueryParams {
    pub address: String,
    pub interface: String,
}

/// Traffic class of a single TURN client-facing datagram. `Relay` carries
/// relayed application data (ChannelData / Send / Data indication) and is the
/// billable dimension; `Control` covers STUN Binding and TURN control messages
/// (Allocate / Refresh / CreatePermission / ChannelBind) plus anything malformed,
/// and is retained for observability only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnTrafficClass {
    Relay,
    Control,
}

/// Byte/packet counters for one traffic class in both directions.
#[derive(Serialize, ToSchema, Default, Debug, Clone, Copy)]
pub struct TurnDirectionalCounters {
    pub received_bytes: usize,
    pub send_bytes: usize,
    pub received_pkts: usize,
    pub send_pkts: usize,
}

#[derive(Serialize, ToSchema, Default, Debug, Clone)]
pub struct TurnSessionStatistics {
    /// Relayed application data (ChannelData + Send/Data indications). Billable.
    pub relay: TurnDirectionalCounters,
    /// STUN Binding + TURN control (Allocate/Refresh/CreatePermission/ChannelBind)
    /// and any malformed datagram. Observability only, never billed.
    pub control: TurnDirectionalCounters,
    pub error_pkts: usize,
}

impl TurnSessionStatistics {
    /// Mutable counters for the given traffic class.
    fn counters_mut(&mut self, class: TurnTrafficClass) -> &mut TurnDirectionalCounters {
        match class {
            TurnTrafficClass::Relay => &mut self.relay,
            TurnTrafficClass::Control => &mut self.control,
        }
    }

    /// Fold one received-direction sample into the counters of `class`.
    pub fn add_recv(&mut self, bytes: usize, class: TurnTrafficClass) {
        let counters = self.counters_mut(class);
        counters.received_bytes += bytes;
        counters.received_pkts += 1;
    }

    /// Fold one sent-direction sample into the counters of `class`.
    pub fn add_send(&mut self, bytes: usize, class: TurnTrafficClass) {
        let counters = self.counters_mut(class);
        counters.send_bytes += bytes;
        counters.send_pkts += 1;
    }
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

    /// Run the TURN service on this host.
    ///
    /// The switch covers the whole runtime, STUN included: both are served by
    /// one server, and there is no half-lifecycle that keeps STUN alive without
    /// TURN. Turning it off is the host-side counterpart of the manager's
    /// cluster-wide kill switch — without it a host would have no way to stop
    /// relaying at all.
    pub enable_turn: bool,

    /// Minimum port for TURN relay
    pub relay_min_port: u16,

    /// Maximum port for TURN relay
    pub relay_max_port: u16,
}

impl TurnSettings {
    /// `turn:{external}?transport=udp` URLs for every interface actually served.
    ///
    /// Empty while the service is switched off: the interfaces stay configured
    /// so the operator can switch it back on, but advertising a relay nobody is
    /// serving only makes peers spend their ICE budget on candidates that can
    /// never connect. The same reasoning excludes entries this host refuses to
    /// bind — a TCP entry, or one whose address does not parse.
    fn turn_urls(&self) -> Vec<String> {
        if !self.enable_turn {
            return Vec::new();
        }
        crate::interface::plan_turn_interfaces(&self.interfaces)
            .servable
            .iter()
            .map(|servable| servable.turn_url())
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
    fn bound_addr_folds_relay_and_control_separately() {
        let mut stats = Statistics::default();
        stats.record_binding(addr(1000), "conn-a");

        stats.record_recv(addr(1000), 100, TurnTrafficClass::Relay);
        stats.record_send(addr(1000), 40, TurnTrafficClass::Relay);
        stats.record_recv(addr(1000), 7, TurnTrafficClass::Control);
        stats.record_send(addr(1000), 3, TurnTrafficClass::Control);

        let conn = stats.by_connection.get("conn-a").expect("connection entry");
        assert_eq!(conn.relay.received_bytes, 100);
        assert_eq!(conn.relay.received_pkts, 1);
        assert_eq!(conn.relay.send_bytes, 40);
        assert_eq!(conn.relay.send_pkts, 1);
        assert_eq!(conn.control.received_bytes, 7);
        assert_eq!(conn.control.send_bytes, 3);
        assert_eq!(conn.control.received_pkts, 1);
        assert_eq!(conn.control.send_pkts, 1);
    }

    #[test]
    fn unbound_addr_does_not_touch_by_connection() {
        let mut stats = Statistics::default();
        stats.record_recv(addr(2000), 100, TurnTrafficClass::Relay);
        stats.record_send(addr(2000), 40, TurnTrafficClass::Control);
        assert!(stats.by_connection.is_empty());
    }

    #[test]
    fn addr_reuse_is_last_writer_wins() {
        let mut stats = Statistics::default();
        stats.record_binding(addr(3000), "conn-old");
        stats.record_recv(addr(3000), 10, TurnTrafficClass::Relay);
        // Same address rebinds to a new connection (NAT reuse).
        stats.record_binding(addr(3000), "conn-new");
        stats.record_recv(addr(3000), 50, TurnTrafficClass::Relay);

        assert_eq!(
            stats
                .by_connection
                .get("conn-old")
                .unwrap()
                .relay
                .received_bytes,
            10
        );
        assert_eq!(
            stats
                .by_connection
                .get("conn-new")
                .unwrap()
                .relay
                .received_bytes,
            50
        );
    }

    #[test]
    fn snapshot_is_independent_clone() {
        let mut stats = Statistics::default();
        stats.record_binding(addr(4000), "conn-x");
        stats.record_recv(addr(4000), 25, TurnTrafficClass::Relay);

        let snap = stats.snapshot_by_connection();
        // Mutating the live stats must not change the snapshot.
        stats.record_recv(addr(4000), 75, TurnTrafficClass::Relay);

        assert_eq!(snap.get("conn-x").unwrap().relay.received_bytes, 25);
        assert_eq!(
            stats
                .by_connection
                .get("conn-x")
                .unwrap()
                .relay
                .received_bytes,
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

    /// Switching the service off has to stop it being advertised too. A peer
    /// handed these URLs would gather relay candidates against a host that is
    /// not listening, and spend its whole ICE budget failing to reach them.
    #[test]
    fn a_disabled_service_advertises_nothing() {
        let off = TurnSettings {
            enable_turn: false,
            ..settings(Some("s"), true)
        };
        assert!(off.get_rest_ice_servers("host-1", 60).is_none());
        assert!(
            off.get_ice_servers("conn-1", "client-1").urls.is_empty(),
            "the caller drops an entry with no URLs, so this is what keeps it out"
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
            // On by default: a host that configured TURN interfaces expects them
            // to be served, and NAT traversal succeeds far more often with a
            // relay available. Operators who do not want to relay turn it off.
            enable_turn: true,
            relay_min_port: 50000,
            relay_max_port: 50050,
        }
    }
}

#[cfg(test)]
mod turn_settings_tests {
    use super::*;

    /// A host that has not said anything about TURN gets it: the switch is read
    /// at startup, so this default decides whether a fresh install relays at
    /// all. `#[serde(default)]` on the struct means an absent key lands here
    /// too, which is what a configuration file written before the switch
    /// existed looks like.
    #[test]
    fn turn_is_on_unless_the_operator_says_otherwise() {
        assert!(TurnSettings::default().enable_turn);
    }
}
