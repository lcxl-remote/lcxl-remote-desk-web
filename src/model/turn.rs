use std::{str::FromStr, sync::Arc, time::Instant};

use serde::{Deserialize, Serialize};
use turn_server::{
    config::{Config, Interface, Transport},
    observer::Observer,
    statistics::Statistics,
    turn::{Service, SessionAddr},
};
use utoipa::{IntoParams, ToSchema};

use crate::desk_error::{CustomDeskError, DeskError};

use super::common::ErrorCode;

pub struct ApiState {
    pub config: Arc<Config>,
    pub service: Service<Observer>,
    pub statistics: Statistics,
    pub uptime: Instant,
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TurnTransport {
    TCP = 0,
    UDP = 1,
}

impl FromStr for TurnTransport {
    type Err = DeskError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            "udp" => Self::UDP,
            "tcp" => Self::TCP,
            _ => return Err(DeskError::CustomError(CustomDeskError::new(ErrorCode::SYSTEM_ERROR, format!("unknown transport: {value}")))),
        })
    }
}

impl From<Transport> for TurnTransport {
    fn from(value: Transport) -> Self {
        match value {
            Transport::UDP => TurnTransport::UDP,
            Transport::TCP => TurnTransport::TCP,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, ToSchema)]
pub struct TurnInterface {
    pub transport: TurnTransport,
    /// turn server listen address
    pub bind: String,
    /// external address
    ///
    /// specify the node external address and port.
    /// for the case of exposing the service to the outside,
    /// you need to manually specify the server external IP
    /// address and service listening port.
    pub external: String,
}

impl From<Interface> for TurnInterface {
    fn from(value: Interface) -> Self {
        TurnInterface {
            transport: value.transport.into(),
            bind: value.bind.to_string(),
            external: value.external.to_string(),
        }
    }
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

impl Into<SessionAddr> for TurnQueryParams {
    fn into(self) -> SessionAddr {
        SessionAddr {
            address: self.address.parse().unwrap(),
            interface: self.interface.parse().unwrap(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct TurnSession {
    pub username: String,
    pub permissions: Vec<u16>,
    pub channels: Vec<u16>,
    pub port: Option<u16>,
    pub expires: u64,
}

#[derive(Serialize, ToSchema)]
pub struct TurnSessionStatistics {
    pub received_bytes: usize,
    pub send_bytes: usize,
    pub received_pkts: usize,
    pub send_pkts: usize,
    pub error_pkts: usize,
}
