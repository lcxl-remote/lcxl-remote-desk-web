use std::{collections::BTreeMap, ops::Deref};

use actix_ws::Session;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct SessionState {
    pub session_id: String,
    pub session: Session,
}

pub struct SharedSessionMap(pub RwLock<BTreeMap<String, SessionState>>);

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
