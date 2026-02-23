use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Deref,
    sync::Arc,
};

use actix_ws::Session;
use desk_signal_facade::model::{session::SessionModel, signal::SignalingModel};
use tokio::sync::{RwLock, oneshot};

/// session state
#[derive(Clone)]
pub struct SessionState {
    /// session model
    pub model: SessionModel,
    /// actix web socket session
    pub session: Arc<RwLock<Session>>,
    /// terminal session id(from_session_id) set, when from_session(browser) is closed, signal server should send close message to desk server to close related terminal processes
    pub terminal_session_ids: Arc<RwLock<HashSet<String /*from_session_id */>>>,
    /// request_id -> oneshot::Sender<SignalingModel>
    pub request_callback_map:
        Arc<RwLock<HashMap<String /* request_id */, oneshot::Sender<SignalingModel>>>>,
    /// device code assigned to this session (if it's a Server session)
    pub device_code: Option<String>,
}

pub struct SharedSessionMap(pub RwLock<BTreeMap<String /* session_id */, SessionState>>);

impl SharedSessionMap {
    pub fn from(session_map: BTreeMap<String, SessionState>) -> Self {
        SharedSessionMap(RwLock::new(session_map))
    }
}

impl Deref for SharedSessionMap {
    type Target = RwLock<BTreeMap<String, SessionState>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
