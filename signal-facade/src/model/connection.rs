use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::version::VersionInfo;

/// Connection information
#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct ConnectionModel {
    /// Connection ID
    pub connection_id: String,
    /// IP address of the connection
    pub ip: Option<String>,
    /// Version info of the connection
    pub version_info: VersionInfo,
}

/// Connection list information
#[derive(Serialize, Clone, Debug, ToSchema)]
pub struct ConnectionList {
    /// Current connection ID
    pub current_connection_id: String,
    /// Connection map
    pub connection_map: BTreeMap<String, ConnectionModel>,
}

use crate::model::signal::SignalingModel;
use actix_ws::Session;
use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    sync::Arc,
};
use tokio::sync::{RwLock, oneshot};

/// Connection state for a single WebSocket connection.
/// Used by both signal server and manager.
#[derive(Clone)]
pub struct ConnectionState {
    /// Connection model (id, ip, version info)
    pub model: ConnectionModel,
    /// Actix WebSocket session
    pub session: Arc<RwLock<Session>>,
    /// Terminal connection IDs (from_connection_id set).
    /// When a browser connection is closed, signal server should notify
    /// the desk server to close related terminal processes.
    pub terminal_connection_ids: Arc<RwLock<HashSet<String>>>,
    /// request_id -> oneshot::Sender<SignalingModel>
    /// For request-response pattern over signaling
    pub request_callback_map: Arc<RwLock<HashMap<String, oneshot::Sender<SignalingModel>>>>,
    /// Device code assigned to this connection (if it's a Server type connection)
    pub device_code: Option<String>,
}

/// Shared connection map: connection_id -> ConnectionState
pub struct SharedConnectionMap(pub RwLock<BTreeMap<String, ConnectionState>>);

impl SharedConnectionMap {
    pub fn new() -> Self {
        SharedConnectionMap(RwLock::new(BTreeMap::new()))
    }
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

use crate::error::DeskSignalFacadeError;
use crate::model::signal::{ForwardSignalingSender, SignalingType};
use desk_utils::error::DeskErrorCode;
use std::time::Duration;

impl ForwardSignalingSender for ConnectionState {
    async fn send_response(
        &self,
        from_connection_id: Option<String>,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::success_response(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            from_connection_id,
            Some(self.model.connection_id.clone()),
            signaling_model.get_raw_data().as_ref(),
        )?;
        self.session
            .write()
            .await
            .text(serde_json::to_string(&signaling_model)?)
            .await?;
        Ok(())
    }

    async fn send_to_peer(
        &self,
        from_connection_id: &str,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        let signaling_model = SignalingModel::new(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            Some(from_connection_id.to_owned()),
            Some(self.model.connection_id.clone()),
            signaling_model.get_raw_data().clone(),
            signaling_model.response_state.clone(),
        );
        self.session
            .write()
            .await
            .text(serde_json::to_string(&signaling_model)?)
            .await?;

        Ok(())
    }

    async fn request_peer_with_callback<T>(
        &self,
        signaling_type: SignalingType,
        data: Option<&T>,
        timeout: Option<Duration>,
    ) -> Result<SignalingModel, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize + Sync,
    {
        let signaling_model = SignalingModel::new_request(
            signaling_type,
            Some(self.model.connection_id.clone()),
            data,
        )?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.session
            .write()
            .await
            .text(serde_json::to_string(&signaling_model)?)
            .await?;
        self.request_callback_map
            .write()
            .await
            .insert(signaling_model.request_id.clone(), tx);

        // TODO: timeout should be configured in the config file
        let timeout = timeout.unwrap_or(Duration::from_secs(30));
        let result = tokio::time::timeout(timeout, rx).await;
        match result {
            Ok(Ok(signaling_model)) => Ok(signaling_model),
            Ok(Err(e)) => {
                // try to remove the request callback map
                let _ = self
                    .request_callback_map
                    .write()
                    .await
                    .remove(&signaling_model.request_id);
                DeskSignalFacadeError::custom_error(DeskErrorCode::TIMEOUT, &e.to_string())
            }
            Err(e) => {
                // try to remove the request callback map
                let _ = self
                    .request_callback_map
                    .write()
                    .await
                    .remove(&signaling_model.request_id);
                DeskSignalFacadeError::custom_error(DeskErrorCode::TIMEOUT, &e.to_string())
            }
        }
    }
}
