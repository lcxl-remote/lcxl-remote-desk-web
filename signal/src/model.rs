use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Deref,
    sync::Arc,
};

use actix_ws::Session;
use desk_signal_facade::model::{connection::ConnectionModel, signal::SignalingModel};
use tokio::sync::{RwLock, oneshot};

/// connection state
#[derive(Clone)]
pub struct ConnectionState {
    /// connection model
    pub model: ConnectionModel,
    /// actix web socket session
    pub session: Arc<RwLock<Session>>,
    /// terminal connection id(from_connection_id) set, when from_connection(browser) is closed, signal server should send close message to desk server to close related terminal processes
    pub terminal_connection_ids: Arc<RwLock<HashSet<String /*from_connection_id */>>>,
    /// request_id -> oneshot::Sender<SignalingModel>
    pub request_callback_map:
        Arc<RwLock<HashMap<String /* request_id */, oneshot::Sender<SignalingModel>>>>,
    /// device code assigned to this connection (if it's a Server connection)
    pub device_code: Option<String>,
}

pub struct SharedConnectionMap(pub RwLock<BTreeMap<String /* connection_id */, ConnectionState>>);

impl SharedConnectionMap {
    pub fn from(connection_map: BTreeMap<String, ConnectionState>) -> Self {
        SharedConnectionMap(RwLock::new(connection_map))
    }
}

impl Deref for SharedConnectionMap {
    type Target = RwLock<BTreeMap<String, ConnectionState>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
