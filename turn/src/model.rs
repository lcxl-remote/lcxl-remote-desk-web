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
    pub fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer {
        let mut urls = vec![];
        for interface in self.interfaces.iter() {
            urls.push(format!(
                "turn:{}?transport={}",
                interface.external,
                if interface.transport == TurnTransport::UDP {
                    "udp"
                } else {
                    "tcp"
                }
            ));
        }
        let ice_server = LcxlRTCIceServer {
            urls: urls,
            username: username.to_owned(),
            credential: credential.to_owned(),
        };

        ice_server
    }
}

impl desk_signal_facade::model::signal::TurnProvider for TurnSettings {
    fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer {
        self.get_ice_servers(username, credential)
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
