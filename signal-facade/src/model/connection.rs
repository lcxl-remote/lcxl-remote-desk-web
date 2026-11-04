use crate::model::peer::{HostExecCapabilities, PeerInfo};
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
    pub version_info: Option<VersionInfo>,
    pub peer: PeerInfo,
    pub exec_capabilities: HostExecCapabilities,
    pub ai_assistant_settings: Option<desk_agent_protocol::ai_assistant::AiAssistantSettings>,
    /// Stable public target: the OSS client identity or Manager device UUID.
    /// Browser connections have no device handle. This value grants no authority.
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
/// only the local Server connections authorized by the requester's identity.
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
#[derive(Serialize, Deserialize, Clone, Debug, Default, ToSchema, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Notify, RwLock, oneshot};

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
    pub lifecycle: Arc<ConnectionLifecycle>,
    /// Connection model (id, ip, version info)
    pub model: Arc<ConnectionModel>,
    /// Private host audience; never serialized in a connection response.
    pub client_id: Option<String>,
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

impl ConnectionState {
    pub fn ensure_active(&self) -> Result<(), DeskSignalFacadeError> {
        if self.lifecycle.is_retired() {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::TARGET_CONNECTION_CHANGED,
                "Target connection changed",
            );
        }
        Ok(())
    }
    pub async fn send_binary(&self, bytes: bytes::Bytes) -> Result<(), DeskSignalFacadeError> {
        self.ensure_active()?;
        let mut session = self.session.write().await;
        self.ensure_active()?;
        session.binary(bytes).await?;
        Ok(())
    }

    /// Public connection metadata never carries a node credential.
    pub fn public_model(&self) -> ConnectionModel {
        self.model.as_ref().clone()
    }
}

/// Retirement is shared by every snapshot of a connection. Cleanup is claimed
/// once, including cancellation paths that only reach the handler's Drop.
#[derive(Default)]
pub struct ConnectionLifecycle {
    retired: AtomicBool,
    cleanup_started: AtomicBool,
    wake: Notify,
}

impl ConnectionLifecycle {
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Acquire)
    }

    pub fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.wake.notify_waiters();
    }

    pub(crate) fn claim_cleanup(&self) -> Option<ConnectionCleanupClaim<'_>> {
        self.cleanup_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| ConnectionCleanupClaim {
                lifecycle: self,
                completed: false,
            })
    }

    pub async fn retired(&self) {
        loop {
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_retired() {
                return;
            }
            notified.await;
        }
    }
}

/// A cancelled cleanup remains eligible for the handler's Drop fallback.
pub(crate) struct ConnectionCleanupClaim<'a> {
    lifecycle: &'a ConnectionLifecycle,
    completed: bool,
}
impl ConnectionCleanupClaim<'_> {
    pub fn complete(mut self) {
        self.completed = true;
    }
}
impl Drop for ConnectionCleanupClaim<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.lifecycle
                .cleanup_started
                .store(false, Ordering::Release);
        }
    }
}

/// The OSS identity index and connection table commit under the same short lock.
/// Manager uses the table only; its authoritative ownership remains in Redis.
pub struct ConnectionRegistry {
    connections: BTreeMap<String, ConnectionState>,
    current_clients: HashMap<String, String>,
    exclusive_oss: bool,
}

impl Deref for ConnectionRegistry {
    type Target = BTreeMap<String, ConnectionState>;
    fn deref(&self) -> &Self::Target {
        &self.connections
    }
}

impl ConnectionRegistry {
    fn indexed_client(state: &ConnectionState) -> Option<&str> {
        (state.auth_context.auth_kind == crate::model::auth_context::AuthKind::TokenAuth
            && state.model.peer.remote_desk_type == RemoteDeskTypeEnum::Server)
            .then_some(state.client_id.as_deref())
            .flatten()
            .filter(|id| !id.is_empty())
    }

    pub fn insert(&mut self, id: String, state: ConnectionState) -> Option<ConnectionState> {
        let mut retired = self.remove(&id);
        if self.exclusive_oss
            && let Some(client) = Self::indexed_client(&state)
        {
            let old_id = self.current_clients.get(client).cloned();
            if let Some(old_id) = old_id {
                retired = self.remove(&old_id).or(retired);
            }
            self.current_clients.insert(client.to_owned(), id.clone());
        }
        self.connections.insert(id, state);
        retired
    }

    pub fn remove(&mut self, id: &str) -> Option<ConnectionState> {
        let removed = self.connections.remove(id)?;
        removed.lifecycle.retire();
        if let Some(client) = Self::indexed_client(&removed)
            && self
                .current_clients
                .get(client)
                .is_some_and(|current| current == id)
        {
            self.current_clients.remove(client);
        }
        Some(removed)
    }

    pub fn clear(&mut self) {
        for state in self.connections.values() {
            state.lifecycle.retire();
        }
        self.connections.clear();
        self.current_clients.clear();
    }

    pub fn current_oss_server(&self, client_id: &str) -> Option<&ConnectionState> {
        self.current_clients
            .get(client_id)
            .and_then(|id| self.connections.get(id))
    }
}

/// No mutable map dereference: mutations must maintain the identity index.
pub struct SharedConnectionMap(
    RwLock<ConnectionRegistry>,
    #[cfg(feature = "test-support")] std::sync::Mutex<Vec<(u64, u64)>>,
);
impl Default for SharedConnectionMap {
    fn default() -> Self {
        Self::new()
    }
}
impl SharedConnectionMap {
    pub fn new() -> Self {
        Self::from(BTreeMap::new())
    }
    pub fn new_oss() -> Self {
        Self(
            RwLock::new(ConnectionRegistry {
                connections: BTreeMap::new(),
                current_clients: HashMap::new(),
                exclusive_oss: true,
            }),
            #[cfg(feature = "test-support")]
            Default::default(),
        )
    }
    pub fn from(connections: BTreeMap<String, ConnectionState>) -> Self {
        Self(
            RwLock::new(ConnectionRegistry {
                connections,
                current_clients: HashMap::new(),
                exclusive_oss: false,
            }),
            #[cfg(feature = "test-support")]
            Default::default(),
        )
    }

    /// Explicit test instrumentation; absent from production builds.
    #[cfg(feature = "test-support")]
    pub fn take_publication_timings(&self) -> Vec<(u64, u64)> {
        std::mem::take(&mut *self.1.lock().unwrap())
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn record_publication(&self, wait: std::time::Duration, hold: std::time::Duration) {
        self.1
            .lock()
            .unwrap()
            .push((wait.as_nanos() as u64, hold.as_nanos() as u64));
    }
}
impl Deref for SharedConnectionMap {
    type Target = RwLock<ConnectionRegistry>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

use crate::error::DeskSignalFacadeError;
use crate::model::signal::{ForwardSignalingSender, RemoteDeskTypeEnum, SignalingType};
use desk_utils::error::DeskErrorCode;
use std::time::Duration;

impl ForwardSignalingSender for ConnectionState {
    async fn send_response(
        &self,
        from_connection_id: Option<String>,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        self.ensure_active()?;
        let signaling_model = SignalingModel::new(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            from_connection_id,
            Some(self.model.connection_id.clone()),
            signaling_model.get_raw_data().clone(),
            signaling_model.response_state.clone(),
        );
        let text = serde_json::to_string(&signaling_model)?;
        let mut session = self.session.write().await;
        self.ensure_active()?;
        session.text(text).await?;
        Ok(())
    }

    async fn send_to_peer(
        &self,
        from_connection_id: &str,
        signaling_model: &SignalingModel,
    ) -> Result<(), DeskSignalFacadeError> {
        self.ensure_active()?;
        let signaling_model = SignalingModel::new(
            &signaling_model.request_id,
            signaling_model.signaling_type,
            Some(from_connection_id.to_owned()),
            Some(self.model.connection_id.clone()),
            signaling_model.get_raw_data().clone(),
            signaling_model.response_state.clone(),
        );
        let text = serde_json::to_string(&signaling_model)?;
        let mut session = self.session.write().await;
        self.ensure_active()?;
        session.text(text).await?;

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
        self.ensure_active()?;
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
        let sent = async {
            let mut session = self.session.write().await;
            self.ensure_active()?;
            session.text(signaling_text).await?;
            Ok::<_, DeskSignalFacadeError>(())
        }
        .await;
        if let Err(error) = sent {
            self.request_callback_map
                .write()
                .await
                .remove(&signaling_model.request_id);
            return Err(error);
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
            debug_build: false,
        }
    }

    #[test]
    fn connection_model_omits_multi_instance_fields_when_none() {
        // A browser connection has no device or owning-instance handle.
        let model = ConnectionModel {
            connection_id: "c1".into(),
            ip: None,
            version_info: Some(sample_version_info()),
            peer: PeerInfo {
                remote_desk_type: RemoteDeskTypeEnum::Server,
                operation_system: super::super::os::OperationSystemEnum::Other,
                display_name: None,
            },
            exec_capabilities: Default::default(),
            ai_assistant_settings: None,
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
            version_info: Some(sample_version_info()),
            peer: PeerInfo {
                remote_desk_type: RemoteDeskTypeEnum::Server,
                operation_system: super::super::os::OperationSystemEnum::Other,
                display_name: None,
            },
            exec_capabilities: Default::default(),
            ai_assistant_settings: None,
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
        let json = format!(
            r#"{{"connection_id":"c1","ip":null,"version_info":{vi},"peer":{{"remote_desk_type":"server","operation_system":"Other","display_name":null}},"exec_capabilities":{{"available_exec_shells":[],"exec_pty":false,"exec_pty_elevation":false,"max_ai_command_runtime_ms":null}}}}"#
        );
        let model: ConnectionModel = serde_json::from_str(&json).unwrap();
        assert_eq!(model.connection_id, "c1");
        assert!(model.device_id.is_none());
        assert!(model.owner_node_id.is_none());
    }

    #[tokio::test]
    async fn retirement_wakes_all_waiters_and_accepts_late_waiters() {
        let lifecycle = ConnectionLifecycle::default();
        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(lifecycle.retired(), lifecycle.retired(), async {
                tokio::task::yield_now().await;
                lifecycle.retire();
            });
            lifecycle.retired().await;
        })
        .await
        .unwrap();
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
