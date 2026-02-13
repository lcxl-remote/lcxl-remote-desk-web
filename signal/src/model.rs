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
    /// terminal session id(to_session_id) set, when session is closed, terminal should send close message to close related terminal processes
    pub terminal_session_ids: Arc<RwLock<HashSet<String /*to_session_id */>>>,
    /// request_id -> oneshot::Sender<SignalingModel>
    pub request_callback_map:
        Arc<RwLock<HashMap<String /* request_id */, oneshot::Sender<SignalingModel>>>>,
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
