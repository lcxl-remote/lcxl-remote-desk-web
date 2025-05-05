use std::{net::SocketAddr, sync::Arc, time::Instant};

use serde::{Deserialize, Serialize};
use turn_server::{
    config::{Config, Interface},
    observer::Observer,
    statistics::Statistics,
    turn::{Service, SessionAddr},
};

pub struct ApiState {
    pub config: Arc<Config>,
    pub service: Service<Observer>,
    pub statistics: Statistics,
    pub uptime: Instant,
}

#[derive(Serialize)]
pub struct TurnInfo {
    pub software: String,
    pub uptime: u64,
    pub interfaces: Vec<Interface>,
    pub port_capacity: usize,
    pub port_allocated: usize,
}

#[derive(Deserialize)]
pub struct TurnQueryParams {
    pub address: SocketAddr,
    pub interface: SocketAddr,
}

impl Into<SessionAddr> for TurnQueryParams {
    fn into(self) -> SessionAddr {
        SessionAddr {
            address: self.address,
            interface: self.interface,
        }
    }
}

#[derive(Serialize)]
pub struct TurnSession {
    pub username: String,
    pub permissions: Vec<u16>,
    pub channels: Vec<u16>,
    pub port: Option<u16>,
    pub expires: u64,
}

#[derive(Serialize)]
pub struct TurnSessionStatistics {
    pub received_bytes: usize,
    pub send_bytes: usize,
    pub received_pkts: usize,
    pub send_pkts: usize,
    pub error_pkts: usize,
}
