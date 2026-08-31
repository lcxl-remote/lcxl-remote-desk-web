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
    /// External device handle (a non-enumerable UUID) for multi-instance
    /// addressing. Filled by the manager (whose presence registry maps it to the
    /// internal device id); `None` on the OSS single-instance signal server, which
    /// has no device registry. Dual-target field, not a backward-compat field:
    /// control ends address a manager device by this handle and an OSS connection
    /// by `connection_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// Owning manager instance node id for cross-instance routing. Filled by the
    /// manager; `None` on the OSS signal server (single instance, no
    /// cross-instance routing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_node_id: Option<String>,
}

/// Selector for which connections a `FetchConnections` request wants.
///
/// `Personal` returns the requester's own devices; `Org` returns the devices
/// authorized to the named organization (manager only). The OSS single-instance
/// signal server has no organization model and ignores the variant, returning
/// its local connection map regardless.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionScope {
    /// The requester's own devices (default).
    #[default]
    Personal,
    /// Devices authorized to an organization (requires `org_id`).
    Org,
}

/// Optional payload of a `FetchConnections` request. Absent payload defaults to
/// personal scope. The OSS signal server ignores this (single instance, no org).
#[derive(Serialize, Deserialize, Clone, Debug, Default, ToSchema)]
pub struct FetchConnectionsScope {
    /// Which set of connections to return. Defaults to `Personal`.
    #[serde(default)]
    pub scope: ConnectionScope,
    /// Organization id, required when `scope` is `Org`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,
}

/// Connection list information
#[derive(Serialize, Clone, Debug, ToSchema)]
pub struct ConnectionsFetchedData {
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

/// One pending signaling request and the response types allowed to complete it.
/// A frame with the same request id but a different wire role stays pending.
pub struct PendingRequestCallback {
    expected_response_types: &'static [SignalingType],
    sender: oneshot::Sender<SignalingModel>,
}

impl PendingRequestCallback {
    pub fn new(
        expected_response_types: &'static [SignalingType],
        sender: oneshot::Sender<SignalingModel>,
    ) -> Self {
        Self {
            expected_response_types,
            sender,
        }
    }

    pub fn accepts(&self, signaling_type: SignalingType) -> bool {
        self.expected_response_types.contains(&signaling_type)
    }

    pub fn send(self, model: SignalingModel) -> bool {
        self.sender.send(model).is_ok()
    }
}

/// Remove a pending callback only when both its request id and declared
/// response type match. A same-id frame with another wire role must leave the
/// real request pending for its eventual response.
pub(crate) fn take_matching_request_callback(
    callbacks: &mut HashMap<String, PendingRequestCallback>,
    request_id: &str,
    signaling_type: SignalingType,
) -> Option<PendingRequestCallback> {
    if callbacks
        .get(request_id)
        .is_some_and(|pending| pending.accepts(signaling_type))
    {
        callbacks.remove(request_id)
    } else {
        None
    }
}

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
    /// `request_id -> (expected response types, callback)` for the signaling
    /// request-response pattern. A mismatched type never consumes the callback.
    pub request_callback_map: Arc<RwLock<HashMap<String, PendingRequestCallback>>>,
    /// Device code assigned to this connection (if it's a Server type connection)
    pub device_code: Option<String>,
    /// Server-resolved authentication identity for this connection. Filled from
    /// validated token/session state at connection setup; never from
    /// client-supplied fields. Used by fleet authorization and audit attribution.
    pub auth_context: crate::model::auth_context::AuthContext,
}

/// Shared connection map: connection_id -> ConnectionState
pub struct SharedConnectionMap(pub RwLock<BTreeMap<String, ConnectionState>>);

impl Default for SharedConnectionMap {
    fn default() -> Self {
        Self::new()
    }
}

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
        let expected_response_types = crate::service::response_types_for_request(signaling_type);
        if expected_response_types.is_empty() {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::INVALID_PARAMS,
                &format!("Signaling type {signaling_type} has no declared callback response"),
            );
        }
        let signaling_model = SignalingModel::new_request(
            signaling_type,
            Some(self.model.connection_id.clone()),
            data,
        )?;
        let signaling_text = serde_json::to_string(&signaling_model)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.request_callback_map.write().await.insert(
            signaling_model.request_id.clone(),
            PendingRequestCallback::new(expected_response_types, tx),
        );
        if let Err(error) = self.session.write().await.text(signaling_text).await {
            self.request_callback_map
                .write()
                .await
                .remove(&signaling_model.request_id);
            return Err(error.into());
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::signal::RemoteDeskTypeEnum;

    fn sample_version_info() -> VersionInfo {
        VersionInfo {
            api_version: 0,
            build_number: 0,
            commit_hash: String::new(),
            remote_desk_type: RemoteDeskTypeEnum::Server,
            operation_system: Default::default(),
            display_name: None,
            client_id: None,
            token: None,
            debug_build: false,
            repository_url: None,
            available_exec_shells: None,
            max_ai_command_runtime_ms: None,
            exec_pty: false,
            exec_pty_elevation: false,
        }
    }

    #[test]
    fn connection_model_omits_multi_instance_fields_when_none() {
        // OSS single-instance signal server leaves device_id/owner_node_id unset;
        // they must not appear on the wire, so an OSS client never sees them.
        let model = ConnectionModel {
            connection_id: "c1".into(),
            ip: None,
            version_info: sample_version_info(),
            device_id: None,
            owner_node_id: None,
        };
        let json = serde_json::to_string(&model).unwrap();
        assert!(!json.contains("device_id"));
        assert!(!json.contains("owner_node_id"));
    }

    #[test]
    fn connection_model_round_trips_multi_instance_fields() {
        // Manager fills both; they must survive serialize -> deserialize so a
        // control end can address the device by its UUID handle and route by owner
        // node.
        let model = ConnectionModel {
            connection_id: "c1".into(),
            ip: Some("203.0.113.4".into()),
            version_info: sample_version_info(),
            device_id: Some("11111111-1111-4111-8111-111111111111".into()),
            owner_node_id: Some("node-a".into()),
        };
        let json = serde_json::to_string(&model).unwrap();
        let back: ConnectionModel = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.device_id.as_deref(),
            Some("11111111-1111-4111-8111-111111111111")
        );
        assert_eq!(back.owner_node_id.as_deref(), Some("node-a"));
    }

    #[test]
    fn connection_model_deserializes_without_multi_instance_fields() {
        // A payload produced by the OSS signal server (no device fields) must
        // still deserialize on a control end built against the dual-target model.
        let vi = serde_json::to_string(&sample_version_info()).unwrap();
        let json = format!(r#"{{"connection_id":"c1","ip":null,"version_info":{vi}}}"#);
        let model: ConnectionModel = serde_json::from_str(&json).unwrap();
        assert_eq!(model.connection_id, "c1");
        assert!(model.device_id.is_none());
        assert!(model.owner_node_id.is_none());
    }

    #[test]
    fn fetch_connections_scope_defaults_to_personal() {
        assert_eq!(
            FetchConnectionsScope::default().scope,
            ConnectionScope::Personal
        );
        // Empty object also yields personal scope.
        let parsed: FetchConnectionsScope = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.scope, ConnectionScope::Personal);
        assert!(parsed.org_id.is_none());
    }

    #[test]
    fn fetch_connections_scope_parses_org() {
        let parsed: FetchConnectionsScope =
            serde_json::from_str(r#"{"scope":"org","org_id":"org-7"}"#).unwrap();
        assert_eq!(parsed.scope, ConnectionScope::Org);
        assert_eq!(parsed.org_id.as_deref(), Some("org-7"));
    }

    #[test]
    fn pending_callback_accepts_only_its_declared_response_type() {
        let mut callbacks = HashMap::new();
        let (sender, receiver) = oneshot::channel();
        callbacks.insert(
            "request-1".to_string(),
            PendingRequestCallback::new(
                crate::service::response_types_for_request(SignalingType::ListTerminalCommands),
                sender,
            ),
        );

        assert!(
            take_matching_request_callback(
                &mut callbacks,
                "request-1",
                SignalingType::TerminalStarted,
            )
            .is_none()
        );
        assert!(callbacks.contains_key("request-1"));

        let pending = take_matching_request_callback(
            &mut callbacks,
            "request-1",
            SignalingType::TerminalCommandsListed,
        )
        .expect("declared terminal-list response should complete the request");
        assert!(!callbacks.contains_key("request-1"));

        let response = SignalingModel::success_response::<()>(
            "request-1",
            SignalingType::TerminalCommandsListed,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(pending.send(response));
        assert_eq!(
            receiver.blocking_recv().unwrap().signaling_type,
            SignalingType::TerminalCommandsListed
        );
    }
}
