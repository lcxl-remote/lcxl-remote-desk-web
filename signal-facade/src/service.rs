use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use actix_web::web;
use actix_ws::{AggregatedMessage, AggregatedMessageStream, Session};
use bytes::Bytes;
use bytestring::ByteString;
use desk_utils::error::DeskErrorCode;
use futures_util::StreamExt;
use serde_json;
use tokio::{runtime::Handle, sync::RwLock};

use crate::{
    error::DeskSignalFacadeError,
    model::{
        connection::{
            ConnectionModel, ConnectionState, ConnectionsFetchedData, FetchConnectionsScope,
            SharedConnectionMap,
        },
        signal::{
            ForwardSignalingSender, LcxlRTCIceServer, RemoteAccessInitializedData,
            RemoteDeskTypeEnum, RequestRemoteModel, SignalingModel, SignalingType, SignalingUser,
            TurnProvider,
        },
        version::VersionInfo,
    },
};

/// TTL (seconds) for the REST TURN credential injected into a forwarded
/// REQUEST_REMOTE. 24h comfortably covers a single desk session; a longer
/// session re-issues a fresh credential when the host reconnects.
const REQUEST_REMOTE_TURN_TTL_SECS: u64 = 86_400;

/// Build the TURN REST ICE server to inject into a forwarded REQUEST_REMOTE,
/// keyed on the **recipient** (`to_connection_id`) so the desk server/host — not
/// the requesting browser — receives usable credentials. Returns `None` when
/// there is no recipient or the provider cannot issue a credential. Pure (no
/// I/O), so it is unit-testable without a WebSocket session.
async fn build_request_remote_ice(
    model: &SignalingModel,
    turn: Option<&Arc<dyn TurnProvider>>,
    ttl_secs: u64,
) -> Option<LcxlRTCIceServer> {
    let to_connection_id = model.to_connection_id.as_deref()?;
    turn?.get_rest_ice_servers(to_connection_id, ttl_secs).await
}

/// Rebuild a `RequestRemoteAccess` after optionally injecting recipient TURN data.
/// Keeping this seam separate makes every browser-supplied admission field part
/// of the regression surface instead of relying on an authorizer to preserve it.
fn rebuild_request_remote_with_ice(
    model: &SignalingModel,
    ice_server: Option<LcxlRTCIceServer>,
) -> Result<SignalingModel, DeskSignalFacadeError> {
    let mut data = model.get_data_with_default::<RequestRemoteModel>()?;
    if let Some(ice_server) = ice_server {
        data.ice_servers.push(ice_server);
    }
    Ok(SignalingModel::new(
        model.request_id.as_str(),
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(serde_json::to_value(data)?),
        model.response_state.clone(),
    ))
}

/// Add recipient TURN credentials to a successful remote-access initialization.
/// Business failures deliberately carry no success payload and must pass through
/// unchanged so the controller can act on their typed `response_state`.
fn rebuild_remote_access_initialized_with_ice(
    model: &SignalingModel,
    ice_server: Option<LcxlRTCIceServer>,
) -> Result<SignalingModel, DeskSignalFacadeError> {
    if model
        .response_state
        .as_ref()
        .is_some_and(|state| !state.is_success())
    {
        return Ok(model.clone());
    }

    let mut data = model.get_data::<RemoteAccessInitializedData>()?;
    if let Some(ice_server) = ice_server
        && !ice_server.urls.is_empty()
    {
        data.ice_servers.push(ice_server);
    }
    Ok(SignalingModel::new(
        model.request_id.as_str(),
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(serde_json::to_value(data)?),
        model.response_state.clone(),
    ))
}

#[cfg(test)]
mod request_remote_ice_tests;

/// Owner-plane host-management signaling frames that only the host owner may
/// direct at a host: host system info / status and display-mode changes. They
/// have **no** worker-side `meet` gate, so the central forward path is their
/// authorization point — a capability-scoped code-session (routed as
/// `device_user`) can never originate one (see
/// [`ConnectionState::forward_to_peer`]). Session-scoped media tuning
/// (`UpdateDeskSettings`) is deliberately excluded — it is not host config.
pub(crate) fn is_owner_plane_management_frame(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::GetSystemInfo | SignalingType::ChangeDisplaySettings
    )
}

mod contracts;
pub use contracts::*;

mod routing;
pub use routing::*;

// ====== SignalingHandler ======

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageControl {
    Continue,
    Close,
}

/// Generic signaling handler. Usable by both signal server and manager.
pub struct SignalingHandler<U: SignalingUser> {
    pub connection_state: ConnectionState,
    pub connection_map: web::Data<SharedConnectionMap>,
    pub user: U,
    pub turn: Option<Arc<dyn TurnProvider>>,
    pub credential_policy: CredentialPolicy,
    /// Fleet policy decision point for the control-end AI frames. `Some` only in
    /// the manager (which wraps the frames with an authorization decision);
    /// `None` in the signal server, where the frames relay unwrapped.
    pub control_authorizer: Option<Arc<dyn ControlFrameAuthorizer>>,
    /// Capability-ceiling stamp seam for `RequestRemoteAccess` frames. `Some` in both
    /// the manager and the signal server (each stamps owner sessions with no
    /// ceiling and redeemed grants with their per-code ceiling, default-denying
    /// otherwise); `None` only where the host applies no central trust, in which
    /// case requests relay unstamped.
    pub request_remote_authorizer: Option<Arc<dyn RequestRemoteAuthorizer>>,
    /// Owner-plane management-frame gate. `Some` only in the manager (which
    /// default-denies owner-plane frames from a non-owner grant-holder); `None` in
    /// the signal server, where those frames relay through `forward_to_peer`'s own
    /// code-session denial unchanged.
    pub owner_plane_authorizer: Option<Arc<dyn OwnerPlaneAuthorizer>>,
    /// Audit persistence observer for inbound `AiAuditEvent` frames. `Some` only
    /// in the manager (which persists them); `None` elsewhere, where they are
    /// ignored.
    pub audit_observer: Option<Arc<dyn AuditObserver>>,
    /// Remote-collect response consumer for inbound `CollectResponse` frames.
    /// `Some` only in the manager (which feeds them into its orchestrator's
    /// pending store); `None` elsewhere, where they are ignored.
    pub collect_observer: Option<Arc<dyn CollectObserver>>,
    /// Central-agent execution result consumer for inbound `EdgeExecResult`
    /// frames. Manager feeds its distributed execution ledger; OSS Signal feeds
    /// its single-node SQLite task ledger.
    pub edge_exec_observer: Option<Arc<dyn EdgeExecObserver>>,
    /// Reconcile-reply consumer for an inbound `ExecStateReply` the central brain
    /// itself asked for. Both Manager and OSS Signal use it to recover a missed
    /// live result from the host's authoritative ledger.
    pub exec_state_reply_observer: Option<Arc<dyn ExecStateReplyObserver>>,
    /// Remote read-tool response consumer for inbound `RemoteToolResponse` frames.
    /// `Some` only in the manager (which feeds them into its remote-tool pending
    /// store); `None` elsewhere, where they are ignored.
    pub remote_tool_observer: Option<Arc<dyn RemoteToolObserver>>,
    /// Support-code minter for inbound `RequestSupportCode` frames. `Some` only in
    /// the manager (which mints a code for the requesting host's device and pushes
    /// it back); `None` elsewhere, where the frame is ignored.
    pub support_code_minter: Option<Arc<dyn SupportCodeMinter>>,
    pub remote_access_admission_authorizer: Option<Arc<dyn RemoteAccessAdmissionAuthorizer>>,
    pub host_remote_access_controller: Option<Arc<dyn HostRemoteAccessController>>,
    /// Resolver for `FetchConnections` requests. `Some` only in the manager
    /// (which returns a cluster-wide, scope-authorized list from presence);
    /// `None` in the signal server, where the handler falls back to the local
    /// connection map.
    pub fetch_connections_resolver: Option<Arc<dyn FetchConnectionsResolver>>,
    /// Cross-instance relay for frames whose target connection this instance does
    /// not hold. `Some` only in the manager (which routes the frame to the owning
    /// instance via the connection-location registry); `None` in the signal server,
    /// where a local miss is a genuine "connection not found" (single instance).
    pub peer_relay: Option<Arc<dyn PeerFrameRelay>>,
}

impl<U: SignalingUser> Drop for SignalingHandler<U> {
    fn drop(&mut self) {
        let handle = match Handle::try_current() {
            Ok(handle) => handle,
            Err(e) => {
                log::warn!(
                    "Failed to get tokio handle in SignalingContext::drop: {}",
                    e
                );
                return;
            }
        };
        let connection_id = self.connection_state.model.connection_id.clone();
        let connection_map = self.connection_map.clone();
        let blocking_handle = handle.clone();
        let removed_value = futures::executor::block_on(async move {
            blocking_handle
                .spawn_blocking(move || connection_map.blocking_write().remove(&connection_id))
                .await
        });
        match removed_value {
            Ok(None) => log::error!(
                "Failed to remove connection from map: connection {} not found",
                self.connection_state.model.connection_id
            ),
            Ok(Some(connection_state)) => {
                log::info!("Removed connection from map: {:?}", connection_state.model)
            }
            Err(err) => log::error!("Failed to remove connection from map: {:?}", err),
        }

        // Active cleanup signal: a Browser leaving has broken every PC
        // it was the remote of. Fan a `ConnectionRemoved` out to all
        // remaining `Server`-type peers so their daemon-side PC
        // managers can release per-`connection_id` resources (DXGI
        // duplication, encoder, IPC senders) immediately, instead of
        // waiting for the multi-second ICE `disconnected → failed`
        // fallback. Run in a background task so a slow / blocked peer
        // can't stall the drop path. We only fan out for Browser
        // departures: a Server leaving means there's nothing on the
        // other side that still cares; a signaling-only or manager
        // peer never owned PC state in the first place.
        if self.connection_state.model.version_info.remote_desk_type == RemoteDeskTypeEnum::Browser
        {
            let connection_id = self.connection_state.model.connection_id.clone();
            let connection_map = self.connection_map.clone();
            handle.spawn(async move {
                broadcast_connection_removed_to_servers(&connection_id, &connection_map).await;
            });
        }
    }
}

/// Fan a `SignalingType::ConnectionRemoved` notification out to every
/// `Server`-type connection currently in the map, identifying the
/// departed peer via `connection_id` (placed in the outgoing model's
/// `from_connection_id`). Failures per peer are logged at WARN — they
/// can't be propagated up because this runs detached from the drop
/// path.
///
/// Pulled out of `SignalingHandler::drop` so the broadcast stays
/// testable in isolation and the drop body itself doesn't grow more
/// async logic.
pub async fn broadcast_connection_removed_to_servers(
    connection_id: &str,
    connection_map: &SharedConnectionMap,
) {
    let server_states: Vec<ConnectionState> = {
        let map_guard = connection_map.read().await;
        map_guard
            .values()
            .filter(|s| s.model.version_info.remote_desk_type == RemoteDeskTypeEnum::Server)
            .cloned()
            .collect()
    };

    if server_states.is_empty() {
        log::debug!(
            "ConnectionRemoved for {connection_id}: no Server peers in map, skipping broadcast"
        );
        return;
    }

    // Build the inert template once; `send_to_peer` rewrites the
    // `from`/`to` fields per recipient when serialising.
    let template = SignalingModel::new(
        &uuid::Uuid::new_v4().to_string(),
        SignalingType::ConnectionRemoved,
        None,
        None,
        None,
        None,
    );

    for state in server_states {
        if let Err(e) = state.send_to_peer(connection_id, &template).await {
            log::warn!(
                "ConnectionRemoved fan-out for {connection_id} → {}: {e}",
                state.model.connection_id,
            );
        }
    }
}

impl<U: SignalingUser> SignalingHandler<U> {
    /// Initialize a new SignalingHandler.
    #[allow(clippy::too_many_arguments)]
    pub async fn init(
        connection_id: String,
        client_version_info: VersionInfo,
        connection_map: web::Data<SharedConnectionMap>,
        ws_session: Session,
        user: U,
        ip: Option<String>,
        turn: Option<Arc<dyn TurnProvider>>,
        device_code: Option<String>,
        auth_context: crate::model::auth_context::AuthContext,
        credential_policy: CredentialPolicy,
        server_api_version: i32,
    ) -> Result<Self, DeskSignalFacadeError> {
        log::info!(
            "Init new SignalingContext, connection id: {}",
            connection_id
        );
        if client_version_info.api_version > server_api_version {
            log::warn!(
                "Client API version({}) is higher than server's({}). This may cause compatibility issues.",
                client_version_info.api_version,
                server_api_version
            );
        }

        let connection_model = ConnectionModel {
            connection_id: connection_id.clone(),
            version_info: client_version_info.clone(),
            ip,
            // Multi-instance routing fields are resolved from the manager's
            // presence registry (the `FetchConnectionsResolver` seam), not from
            // the per-instance local connection map. The locally constructed
            // model carries `None`; on the OSS signal server they stay `None`.
            device_id: None,
            owner_node_id: None,
        };

        let connection_state = ConnectionState {
            model: connection_model,
            session: Arc::new(RwLock::new(ws_session)),
            terminal_connection_ids: Arc::new(RwLock::new(std::collections::HashSet::new())),
            request_callback_map: Arc::new(RwLock::new(std::collections::HashMap::new())),
            device_code,
            auth_context,
        };

        connection_map
            .write()
            .await
            .insert(connection_id.clone(), connection_state.clone());
        Ok(Self {
            connection_state,
            connection_map,
            user,
            turn,
            credential_policy,
            control_authorizer: None,
            request_remote_authorizer: None,
            owner_plane_authorizer: None,
            audit_observer: None,
            collect_observer: None,
            edge_exec_observer: None,
            exec_state_reply_observer: None,
            remote_tool_observer: None,
            support_code_minter: None,
            remote_access_admission_authorizer: None,
            host_remote_access_controller: None,
            fetch_connections_resolver: None,
            peer_relay: None,
        })
    }

    /// Attach a fleet control-frame authorizer (the manager PDP). The signal
    /// server never calls this, leaving AI frames relayed unwrapped.
    pub fn with_control_authorizer(mut self, authorizer: Arc<dyn ControlFrameAuthorizer>) -> Self {
        self.control_authorizer = Some(authorizer);
        self
    }

    /// Attach the `RequestRemoteAccess` capability-ceiling stamp seam. The manager and
    /// the signal server both call this so every relayed `RequestRemoteAccess` carries a
    /// trusted stamp; a handler left without one relays requests unstamped.
    pub fn with_request_remote_authorizer(
        mut self,
        authorizer: Arc<dyn RequestRemoteAuthorizer>,
    ) -> Self {
        self.request_remote_authorizer = Some(authorizer);
        self
    }

    /// Attach the owner-plane management-frame gate (the manager owner check). The
    /// signal server never calls this, leaving those frames gated only by
    /// `forward_to_peer`'s code-session denial.
    pub fn with_owner_plane_authorizer(
        mut self,
        authorizer: Arc<dyn OwnerPlaneAuthorizer>,
    ) -> Self {
        self.owner_plane_authorizer = Some(authorizer);
        self
    }

    /// Attach an audit observer (the manager persistence sink). The signal
    /// server never calls this, so inbound audit frames are ignored there.
    pub fn with_audit_observer(mut self, observer: Arc<dyn AuditObserver>) -> Self {
        self.audit_observer = Some(observer);
        self
    }

    /// Attach a remote-collect response consumer (the manager orchestrator's
    /// pending store). The signal server never calls this, so inbound
    /// `CollectResponse` frames are ignored there.
    pub fn with_collect_observer(mut self, observer: Arc<dyn CollectObserver>) -> Self {
        self.collect_observer = Some(observer);
        self
    }

    /// Attach a fleet-execution result consumer (the manager execution pending
    /// store). The signal server never calls this, so inbound `EdgeExecResult`
    /// frames are ignored there.
    pub fn with_edge_exec_observer(mut self, observer: Arc<dyn EdgeExecObserver>) -> Self {
        self.edge_exec_observer = Some(observer);
        self
    }

    /// Attach a reconcile-reply consumer (the manager's state-query pending
    /// store). The signal server never calls this, so an unrouted inbound
    /// `ExecStateReply` is ignored there.
    pub fn with_exec_state_reply_observer(
        mut self,
        observer: Arc<dyn ExecStateReplyObserver>,
    ) -> Self {
        self.exec_state_reply_observer = Some(observer);
        self
    }

    /// Attach a remote-tool response consumer (the manager's remote-tool pending
    /// store). The signal server never calls this, so inbound `RemoteToolResponse`
    /// frames are ignored there.
    pub fn with_remote_tool_observer(mut self, observer: Arc<dyn RemoteToolObserver>) -> Self {
        self.remote_tool_observer = Some(observer);
        self
    }

    /// Attach a support-code minter (the manager). The signal server never calls
    /// this, so inbound `RequestSupportCode` frames are ignored there.
    pub fn with_support_code_minter(mut self, minter: Arc<dyn SupportCodeMinter>) -> Self {
        self.support_code_minter = Some(minter);
        self
    }

    pub fn with_remote_access_admission_authorizer(
        mut self,
        authorizer: Arc<dyn RemoteAccessAdmissionAuthorizer>,
    ) -> Self {
        self.remote_access_admission_authorizer = Some(authorizer);
        self
    }

    pub fn with_host_remote_access_controller(
        mut self,
        controller: Arc<dyn HostRemoteAccessController>,
    ) -> Self {
        self.host_remote_access_controller = Some(controller);
        self
    }

    /// Attach a cluster-wide `FetchConnections` resolver (the manager's
    /// presence-backed, scope-authorized list). The signal server never calls
    /// this, so `FetchConnections` falls back to the local connection map.
    pub fn with_fetch_connections_resolver(
        mut self,
        resolver: Arc<dyn FetchConnectionsResolver>,
    ) -> Self {
        self.fetch_connections_resolver = Some(resolver);
        self
    }

    /// Attach a cross-instance peer-frame relay (the manager's connection-location
    /// routed internal hop). The signal server never calls this, so a frame whose
    /// target connection is not held locally is a genuine "connection not found".
    pub fn with_peer_relay(mut self, relay: Arc<dyn PeerFrameRelay>) -> Self {
        self.peer_relay = Some(relay);
        self
    }

    /// Forward a signaling message to target peer
    pub async fn forward_to_peer(
        &self,
        signaling_model: &SignalingModel,
        ignore_connection_not_found: bool,
    ) -> Result<(), DeskSignalFacadeError> {
        // Device user restriction logic
        if self.user.get_access() == Some("device_user")
            && let Some(target_connection) = self.user.get_target_connection_id()
        {
            let to_connection_id = signaling_model.check_and_get_to_connection_id()?;
            if to_connection_id != target_connection {
                return DeskSignalFacadeError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!(
                        "Permission denied: cannot send message to {}",
                        to_connection_id
                    ),
                );
            }
            // A capability-scoped code-session (a redeemed device / support code,
            // routed as `device_user`) is never the host owner. Refuse to forward
            // owner-plane host-management frames to it: host system info / status
            // and display-mode changes
            // have **no** worker-side `meet` gate, so the central is their sole
            // authorization point. door1 denies them for an *admitted* capped
            // session; blocking here also closes the pre-`RequestRemoteAccess` window,
            // where the host has no admission record yet and would otherwise pass
            // them. Session media tuning (`UpdateDeskSettings`) is
            // session-scoped, not host config, so it is intentionally not listed.
            if is_owner_plane_management_frame(signaling_model.signaling_type) {
                return DeskSignalFacadeError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!(
                        "Permission denied: a capability-scoped session cannot send {:?}",
                        signaling_model.signaling_type
                    ),
                );
            }
        }

        let pending_callback = {
            let mut callbacks = self.connection_state.request_callback_map.write().await;
            let has_same_request_id = callbacks.contains_key(&signaling_model.request_id);
            let pending = crate::model::connection::take_matching_request_callback(
                &mut callbacks,
                &signaling_model.request_id,
                signaling_model.signaling_type,
            );
            if pending.is_none() && has_same_request_id {
                log::warn!(
                    "Ignoring mismatched {} for pending request {}; callback remains active",
                    signaling_model.signaling_type,
                    signaling_model.request_id
                );
            }
            pending
        };
        if let Some(pending) = pending_callback {
            if !pending.send(signaling_model.clone()) {
                return Err(DeskSignalFacadeError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "Failed to send response to peer",
                ));
            }
            return Ok(());
        }
        // No pending request-callback matched here. The daemon fans every
        // browser-bound frame out to all of the host's upstream links, so a
        // response can reach a central that neither owns the origin callback nor
        // holds the target connection. Such a copy is not an error (see
        // `classify_unmatched_forward`).
        match classify_unmatched_forward(signaling_model, ignore_connection_not_found) {
            UnmatchedForward::Drop => {
                log::debug!(
                    "Dropping orphaned {} response (no local callback, no target); it is a \
                     broadcast copy destined for another upstream",
                    signaling_model.signaling_type
                );
                Ok(())
            }
            // A request with no target is a real protocol error; reuse the shared
            // check to produce the canonical message.
            UnmatchedForward::MissingTarget => {
                signaling_model.check_and_get_to_connection_id().map(|_| ())
            }
            UnmatchedForward::Deliver {
                to,
                ignore_not_found,
            } => {
                let from_connection_id = self.connection_state.model.connection_id.clone();

                // Local fast path: the target connection is held by this instance.
                if deliver_to_local_peer(
                    &self.connection_map,
                    &from_connection_id,
                    to,
                    signaling_model,
                )
                .await?
                {
                    return Ok(());
                }

                // Not held locally: try the cross-instance relay, else honor the
                // ignore flag / surface SESSION_NOT_FOUND.
                relay_or_not_found(
                    &self.peer_relay,
                    to,
                    &from_connection_id,
                    signaling_model,
                    ignore_not_found,
                )
                .await
            }
        }
    }

    /// Handle incoming signaling message
    pub async fn handle_message(
        &mut self,
        text: ByteString,
    ) -> Result<MessageControl, DeskSignalFacadeError> {
        log::debug!("Received text message: {}", text);
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        if self.connection_state.auth_context.auth_kind
            == crate::model::auth_context::AuthKind::CookieAuth
            && contracts::signaling_role(signaling_model.signaling_type)
                == contracts::SignalingRole::Response
        {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "browser connections cannot originate response-only signaling types",
            );
        }
        if signaling_model.is_request()
            && remote_access_frame_requires_unlocked(signaling_model.signaling_type)
            && let Some(authorizer) = self.remote_access_admission_authorizer.clone()
        {
            match authorizer
                .authorize(
                    &self.connection_state,
                    &self.connection_map,
                    &signaling_model,
                )
                .await
            {
                RemoteAccessAdmissionOutcome::Allow => {}
                RemoteAccessAdmissionOutcome::Reject { code, message } => {
                    return DeskSignalFacadeError::custom_error(code, &message);
                }
            }
        }
        let mut control = MessageControl::Continue;
        match signaling_model.signaling_type {
            SignalingType::SendHeartbeat => {
                let (response, should_close) = match &self.credential_policy {
                    CredentialPolicy::Plain => (
                        SignalingModel::success_response::<()>(
                            &signaling_model.request_id,
                            SignalingType::HeartbeatAcknowledged,
                            None,
                            Some(self.connection_state.model.connection_id.clone()),
                            None,
                        )?,
                        false,
                    ),
                    CredentialPolicy::ManagerToken(authorizer) => {
                        match authorizer
                            .authorize_heartbeat(&self.connection_state)
                            .await
                        {
                            CredentialHeartbeatOutcome::Proof(proof) => (
                                SignalingModel::success_response(
                                    &signaling_model.request_id,
                                    SignalingType::HeartbeatAcknowledged,
                                    None,
                                    Some(self.connection_state.model.connection_id.clone()),
                                    Some(&proof),
                                )?,
                                false,
                            ),
                            CredentialHeartbeatOutcome::TerminalRevoked(reason) => {
                                log::warn!(
                                    "Manager credential terminally revoked for connection {}: \
                                     {reason:?}",
                                    self.connection_state.model.connection_id
                                );
                                (
                                    SignalingModel::error(
                                        &signaling_model.request_id,
                                        SignalingType::HeartbeatAcknowledged,
                                        None,
                                        Some(
                                            self.connection_state.model.connection_id.clone(),
                                        ),
                                        DeskErrorCode::MANAGER_CREDENTIAL_REVOKED,
                                        "Manager credential is no longer valid",
                                    )?,
                                    true,
                                )
                            }
                            CredentialHeartbeatOutcome::Suspended(reason) => {
                                log::warn!(
                                    "Manager credential suspended for connection {}: {reason:?}",
                                    self.connection_state.model.connection_id
                                );
                                (
                                    SignalingModel::error(
                                        &signaling_model.request_id,
                                        SignalingType::HeartbeatAcknowledged,
                                        None,
                                        Some(
                                            self.connection_state.model.connection_id.clone(),
                                        ),
                                        DeskErrorCode::MANAGER_CREDENTIAL_SUSPENDED,
                                        "Manager credential is temporarily unavailable",
                                    )?,
                                    true,
                                )
                            }
                            CredentialHeartbeatOutcome::SnapshotStale
                            | CredentialHeartbeatOutcome::BackendUnavailable => (
                                SignalingModel::success_response::<()>(
                                    &signaling_model.request_id,
                                    SignalingType::HeartbeatAcknowledged,
                                    None,
                                    Some(
                                        self.connection_state.model.connection_id.clone(),
                                    ),
                                    None,
                                )?,
                                false,
                            ),
                        }
                    }
                };
                self.connection_state
                    .session
                    .write()
                    .await
                    .text(serde_json::to_string(&response)?)
                    .await?;
                if should_close {
                    control = MessageControl::Close;
                }
            }
            SignalingType::HeartbeatAcknowledged => {
                log::warn!(
                    "Received server-originated heartbeat response from client {}; dropping",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::FetchConnections => {
                let connection_map = if let Some(resolver) = &self.fetch_connections_resolver {
                    // Manager: cluster-wide, presence-backed, scope-authorized
                    // list. The scope payload is optional and defaults to
                    // personal.
                    let scope = signaling_model.get_data_with_default::<FetchConnectionsScope>()?;
                    let models = resolver.resolve(&self.connection_state, scope).await?;
                    models
                        .into_iter()
                        .map(|model| (model.connection_id.clone(), model))
                        .collect()
                } else {
                    // Signal server: single-instance local map.
                    let connection_map = self.connection_map.read().await;
                    connection_map
                        .iter()
                        .map(|item| (item.0.clone(), item.1.model.clone()))
                        .collect()
                };
                let connection_list = ConnectionsFetchedData {
                    current_connection_id: self.connection_state.model.connection_id.clone(),
                    connection_map,
                };

                log::info!("Sending connection list to client: {:?}", connection_list);
                let response = SignalingModel::success_response(
                    &signaling_model.request_id,
                    SignalingType::ConnectionsFetched,
                    None,
                    None,
                    Some(&connection_list),
                )?;
                self.connection_state.send_response(None, &response).await?;
            }
            SignalingType::ConnectionsFetched => {
                log::warn!(
                    "Received connection list signaling type: {}, it should not be received",
                    signaling_model.signaling_type
                );
            }
            SignalingType::ConnectionRemoved => {
                // ConnectionRemoved is server → peer only — emitted from
                // `SignalingHandler::drop` via
                // `broadcast_connection_removed_to_servers`. A client
                // sending it inbound is a protocol error; swallow with
                // a warning so daemon-side cleanup state can't be
                // forged.
                log::warn!(
                    "Received connection removed signaling type from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::SendTerminalInput => {
                let from_connection_id = &self.connection_state.model.connection_id;
                if signaling_model.is_request()
                    && !self
                        .connection_state
                        .terminal_connection_ids
                        .read()
                        .await
                        .contains(from_connection_id)
                {
                    return DeskSignalFacadeError::custom_error(
                        DeskErrorCode::SYSTEM_ERROR,
                        &format!(
                            "Connection {} is not a terminal, can not send data to terminal",
                            from_connection_id
                        ),
                    );
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }
            SignalingType::ResizeTerminal => {
                let from_connection_id = &self.connection_state.model.connection_id;
                if signaling_model.is_request()
                    && !self
                        .connection_state
                        .terminal_connection_ids
                        .read()
                        .await
                        .contains(from_connection_id)
                {
                    return DeskSignalFacadeError::custom_error(
                        DeskErrorCode::SYSTEM_ERROR,
                        &format!(
                            "Connection {} is not a terminal, can not resize terminal",
                            from_connection_id
                        ),
                    );
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }

            SignalingType::TerminalOutputProduced | SignalingType::TerminalClosed => {
                self.forward_to_peer(&signaling_model, true).await?;
            }

            SignalingType::IceCandidate => {
                let fallback_ip = self
                    .connection_state
                    .model
                    .ip
                    .as_deref()
                    .and_then(parse_ip_from_peer_addr);
                if let Some(ip) = fallback_ip.map(|ip| {
                    if ip.is_ipv6() && ip.is_loopback() {
                        IpAddr::from([127, 0, 0, 1])
                    } else {
                        ip
                    }
                }) && let Some(rewritten) = rewrite_mdns_candidate_with_ip(&signaling_model, ip)
                {
                    self.forward_to_peer(&rewritten, false).await?;
                    return Ok(MessageControl::Continue);
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }

            SignalingType::RequestRemoteAccess => {
                // Inject a TURN REST ICE server for the RECIPIENT (the desk
                // server / host this REQUEST_REMOTE is forwarded to) so it can
                // gather srflx/relay candidates for NAT traversal. Keyed on
                // `to_connection_id`, not the sender: a browser requester has no
                // usable TURN identity, which would otherwise leave the host
                // with no ICE servers (`iceServers=0`).
                let ice_server = build_request_remote_ice(
                    &signaling_model,
                    self.turn.as_ref(),
                    REQUEST_REMOTE_TURN_TTL_SECS,
                )
                .await;
                let new_signaling_model =
                    rebuild_request_remote_with_ice(&signaling_model, ice_server)?;
                // Stamp the trusted capability ceiling (owner → none / grant →
                // ceiling / neither → default-deny) before relaying. With no
                // authorizer the frame relays unstamped.
                let to_forward = if let Some(authorizer) = self.request_remote_authorizer.clone() {
                    match authorizer
                        .authorize(&self.connection_state, &self.connection_map, &new_signaling_model)
                        .await
                    {
                        RequestRemoteOutcome::Forward(m) => m,
                        RequestRemoteOutcome::Reject { code, message } => {
                            return DeskSignalFacadeError::custom_error(code, &message);
                        }
                    }
                } else {
                    new_signaling_model
                };
                self.forward_to_peer(&to_forward, false).await?;
            }

            SignalingType::RemoteAccessInitialized => {
                // A typed business failure has no initialization payload. Forward
                // it before attempting success-only parsing/TURN injection so the
                // controller receives errors such as ACTION_NEED_RETRY instead of
                // a locally synthesized BLANK_SIGNALING_DATA failure.
                if signaling_model
                    .response_state
                    .as_ref()
                    .is_some_and(|state| !state.is_success())
                {
                    self.forward_to_peer(&signaling_model, false).await?;
                    return Ok(MessageControl::Continue);
                }

                // TODO need to support static auth secret
                // ice servers
                // username is connection_id
                // password is client id
                let mut ice_server = None;
                let client_id_opt = self.connection_state.model.version_info.client_id.clone();
                if let Some(client_id) = client_id_opt {
                    if let Some(turn) = &self.turn {
                        let candidate = turn
                            .get_ice_servers(
                                &self.connection_state.model.connection_id,
                                &client_id,
                            )
                            .await;
                        if !candidate.urls.is_empty() {
                            ice_server = Some(candidate);
                        } else {
                            log::warn!(
                                "Skipping empty TURN ICE servers for connection {}",
                                self.connection_state.model.connection_id
                            );
                        }
                    } else {
                        log::warn!(
                            "TURN settings unavailable, skip injecting TURN ICE for connection {}",
                            self.connection_state.model.connection_id
                        );
                    }
                }
                let new_signaling_model =
                    rebuild_remote_access_initialized_with_ice(&signaling_model, ice_server)?;
                self.forward_to_peer(&new_signaling_model, false).await?;
            }
            // Owner-plane device-management frames. These carry no capability
            // ceiling and are meaningful only for the target device's owner
            // (system info, virtual-display mode change). In the manager a
            // `CookieAuth` browser is owner or grant-holder depending on the
            // target, so the owner-plane authorizer default-denies any non-owner
            // (a capped grant-holder must never reach them) before relaying. The
            // signal server leaves the authorizer unset; its own code-session
            // denial in `forward_to_peer` (`is_owner_plane_management_frame`)
            // still applies, so its behaviour is unchanged. Kept in lock-step with
            // `is_owner_plane_management_frame`.
            SignalingType::GetSystemInfo | SignalingType::ChangeDisplaySettings => {
                if let Some(authorizer) = self.owner_plane_authorizer.clone() {
                    match authorizer
                        .authorize(
                            &self.connection_state,
                            &self.connection_map,
                            &signaling_model,
                        )
                        .await
                    {
                        OwnerPlaneOutcome::Allow => {}
                        OwnerPlaneOutcome::Reject { code, message } => {
                            return DeskSignalFacadeError::custom_error(code, &message);
                        }
                    }
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }

            // Owner-plane responses are outbound-only and must pass the same
            // manager fence as their requests. The manager verifies the bound
            // host, expected response type, request id, and original browser;
            // the OSS signal keeps its existing direct relay behavior.
            SignalingType::SystemInfoRetrieved | SignalingType::DisplaySettingsChanged => {
                if let Some(authorizer) = self.owner_plane_authorizer.clone() {
                    match authorizer
                        .authorize(
                            &self.connection_state,
                            &self.connection_map,
                            &signaling_model,
                        )
                        .await
                    {
                        OwnerPlaneOutcome::Allow => {}
                        OwnerPlaneOutcome::Reject { code, message } => {
                            return DeskSignalFacadeError::custom_error(code, &message);
                        }
                    }
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }

            // Forwarding types
            SignalingType::Offer
            | SignalingType::Answer
            | SignalingType::RequireControl
            | SignalingType::ControlAccepted
            | SignalingType::ControlDenied
            | SignalingType::ReleaseControl
            | SignalingType::ControlReleased
            | SignalingType::CloseRemoteSession
            | SignalingType::UpdateDeskSettings
            | SignalingType::ListFiles
            | SignalingType::FilesListed
            | SignalingType::DeleteFile
            | SignalingType::FileDeleted
            | SignalingType::ListTerminalCommands
            | SignalingType::TerminalCommandsListed
            | SignalingType::SetPrivateScreenVisibility
            | SignalingType::PrivateScreenVisibilitySet
            | SignalingType::PrivateScreenStateChanged
            | SignalingType::TerminalStarted
            | SignalingType::AudioPlaybackFailed
            | SignalingType::MediaPipelineStateChanged
            | SignalingType::RetryMediaPipeline
            | SignalingType::MediaPipelineRetryCompleted
            | SignalingType::DesktopSwitching
            | SignalingType::DesktopReady
            // AI host → control-end responses are plain relayed types (no
            // authorization injection on the reply path).
            | SignalingType::AgentCapabilityCompleted
            | SignalingType::DiagnosisUpdated
            | SignalingType::TerminalCopilotUpdated
            | SignalingType::TerminalCompletionsGenerated
            | SignalingType::ExecutionPreviewGenerated
            | SignalingType::ExecutionCompleted
            => {
                // Generic forwarding
                self.forward_to_peer(&signaling_model, false).await?;
            }

            // Browser-owned executions carry a browser target and relay there.
            // Centrally-owned edge executions intentionally carry no peer target:
            // their authoritative result is consumed by the central observer, so
            // lifecycle progress is advisory and must not be forwarded to `None`.
            SignalingType::ExecutionProgressUpdated => {
                if signaling_model.to_connection_id.is_some() {
                    self.forward_to_peer(&signaling_model, false).await?;
                } else {
                    log::trace!(
                        "Received central ExecLifecycle for {}, no peer relay needed",
                        signaling_model.request_id
                    );
                }
            }

            // A state reply answers either a browser's query or the manager's own
            // reconcile. A browser-initiated query carries the browser as the
            // reply's target, so it relays there; a reconcile the manager issued
            // has no peer target, and its answer is consumed here instead of being
            // forwarded to no one. The signal server has no reconcile consumer, so
            // an unrouted reply is simply dropped.
            SignalingType::ExecutionStateReported => {
                if signaling_model.to_connection_id.is_some() {
                    self.forward_to_peer(&signaling_model, false).await?;
                } else if let Some(observer) = self.exec_state_reply_observer.clone() {
                    observer
                        .on_exec_state_reply(&self.connection_state, &signaling_model)
                        .await;
                }
            }

            // AI audit event (host → manager only). Consumed by the manager's
            // audit observer for persistence; never relayed to a peer (it must
            // not re-enter the control-end broadcast lane). Ignored where no
            // observer is attached (the signal server).
            SignalingType::ReportAiAuditEvent => {
                if let Some(observer) = self.audit_observer.clone() {
                    observer
                        .on_audit_event(&self.connection_state, &signaling_model)
                        .await;
                }
            }

            // AI control-end → host request frames. In the manager these pass
            // through the fleet policy decision point, which authorizes and
            // wraps them in an `AuthorizedControlPayload`; in the signal server
            // (no authorizer) they relay unwrapped, exactly like before.
            //
            // `DiagnoseCancel` is included so the manager can run it centrally:
            // diagnosis is orchestrated on the manager (thin-edge model), so a
            // start-over/abort must cancel the manager's pending collection and
            // record the cancellation rather than be relayed to a host that has
            // no diagnose task. With no authorizer (signal server) it relays to
            // the host that is running the diagnosis, exactly like before.
            // `ResolveExec` is included so the manager can consume an agentic exec
            // approval centrally (it owns the durable work item). A host exec's
            // ResolveExec is relayed unwrapped by the authorizer (`Forward`); with no
            // authorizer (signal server) it relays plainly, exactly like before.
            SignalingType::InvokeAgentCapability
            | SignalingType::DiagnoseDevice
            | SignalingType::CancelDiagnosis
            | SignalingType::AskTerminalCopilot
            | SignalingType::CancelTerminalCopilot
            | SignalingType::GenerateTerminalCompletions
            | SignalingType::PreviewExecution
            // `ExecControl` acts on a command that is already running, so it goes
            // through the authorizer rather than relaying: stopping someone else's
            // execution is a decision, and one that has to be recorded.
            | SignalingType::ControlExecution
            | SignalingType::ResolveExecution => {
                let to_forward = if let Some(authorizer) = self.control_authorizer.clone() {
                    match authorizer
                        .authorize(&self.connection_state, &self.connection_map, &signaling_model)
                        .await
                    {
                        ControlFrameOutcome::Forward(m) => m,
                        ControlFrameOutcome::Reject { code, message } => {
                            return DeskSignalFacadeError::custom_error(code, &message);
                        }
                        // The authorizer ran the frame itself (manager-side
                        // orchestration); nothing is relayed to the host.
                        ControlFrameOutcome::Handled => return Ok(MessageControl::Continue),
                    }
                } else {
                    signaling_model
                };
                self.forward_to_peer(&to_forward, false).await?;
            }

            SignalingType::SyncCommandTemplates => {
                // Manager → daemon only, originated server-side and written
                // directly to the desk-server's session. A client sending it
                // inbound to the signaling server is a protocol error; swallow
                // it so a control end cannot forge a template sync.
                log::warn!(
                    "Received command-template sync from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::SyncCommandBlocklist => {
                // Manager → daemon only, originated server-side and written
                // directly to the desk-server's session. A client sending it
                // inbound to the signaling server is a protocol error; swallow
                // it so a control end cannot forge a blocklist sync.
                log::warn!(
                    "Received command-blocklist sync from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::CollectEvidence => {
                // Manager → daemon only, originated server-side and written
                // directly to the desk-server's session. A client sending it
                // inbound to the signaling server is a protocol error; swallow it
                // so a control end cannot forge an evidence-collection request.
                log::warn!(
                    "Received remote-collect request from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::EvidenceCollectionUpdated => {
                // Desk-server daemon → manager only. Consumed by the manager
                // orchestrator's pending store; never relayed to a peer (it must
                // not re-enter the control-end broadcast lane). Ignored where no
                // orchestrator consumer is attached (the signal server).
                if let Some(observer) = self.collect_observer.clone() {
                    observer
                        .on_collect_response(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::ExecuteEdgePlan => {
                // Manager → daemon only, originated server-side and written
                // directly to the desk-server's session (the manager is the PDP).
                // A client sending it inbound to the signaling server is a
                // protocol error; swallow it so a control end cannot forge a
                // sealed execution plan.
                log::warn!(
                    "Received fleet-exec request from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::InvokeRemoteTool => {
                // Manager owner instance → daemon only, written directly to the
                // desk-server's session. A client sending it inbound to the
                // signaling server is a protocol error; swallow it so a control end
                // cannot forge a remote tool invocation.
                log::warn!(
                    "Received remote-tool request from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::RemoteToolOutputUpdated => {
                // Desk-server daemon → manager only. Consumed by the manager
                // remote-tool pending store; never relayed to a peer. Ignored where
                // no remote-tool consumer is attached (the signal server).
                if let Some(observer) = self.remote_tool_observer.clone() {
                    observer
                        .on_remote_tool_response(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::EdgeExecutionCompleted => {
                // Desk-server daemon → manager only. Consumed by the manager
                // execution pending store; never relayed to a peer (it must not
                // re-enter the control-end broadcast lane). Ignored where no
                // execution consumer is attached (the signal server).
                if let Some(observer) = self.edge_exec_observer.clone() {
                    observer
                        .on_fleet_exec_result(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::Error => {
                log::warn!("Received error from client: {:?}", signaling_model);
            }
            SignalingType::Unknown => {
                log::warn!("Received unknown signaling type");
            }
            SignalingType::StartTerminal | SignalingType::CloseTerminal => {
                // This not send by client, it is send by signal server, so it is not need to handle, and should not be received
                log::warn!(
                    "Received start/close terminal signaling type: {}, it should not be received",
                    signaling_model.signaling_type
                );
            }
            SignalingType::RequestSupportCode => {
                // Host (desk server) → central brain: mint a support code for the
                // requesting connection's device and push it back. Consumed by the
                // manager's minter; ignored where none is attached (a plain signal
                // never mints support codes).
                if let Some(minter) = self.support_code_minter.clone() {
                    minter
                        .on_request_support_code(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::RevokeSupportCode => {
                // Host (desk server) → central brain: the local user ended support;
                // revoke the code so it can no longer be redeemed. Consumed by the
                // manager (which verifies ownership first); ignored elsewhere.
                if let Some(minter) = self.support_code_minter.clone() {
                    minter
                        .on_revoke_support_code(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::UpdateRemoteAccessLock => {
                if let Some(controller) = self.host_remote_access_controller.clone() {
                    controller
                        .on_lock_request(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::TerminateRemotePeer => {
                if let Some(controller) = self.host_remote_access_controller.clone() {
                    controller
                        .on_terminate_peer_request(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::RemoteAccessLockUpdated
            | SignalingType::RemotePeerTerminationResolved => {
                log::warn!(
                    "Received server-originated remote-access ack from client {}; dropping",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::SupportCodeIssued => {
                // Manager → host only (the issued support code, pushed over the
                // host's regular `Server` upstream). It is server-originated, so a
                // connection sending it inbound is misbehaving; drop it.
                log::warn!(
                    "Received SupportCodeIssued from a client; it is server-originated and must \
                     not be sent inbound — dropping"
                );
            }
            SignalingType::RevokeAccessGrant => {
                // Manager → host only (a grant-session teardown pushed after a
                // dial-code regeneration). It is server-originated, so a connection
                // sending it inbound is misbehaving; drop it.
                log::warn!(
                    "Received RevokeAccessGrant from a client; it is server-originated and must \
                     not be sent inbound — dropping"
                );
            }
        }
        Ok(control)
    }

    pub async fn binary(&mut self, _bin: Bytes) -> Result<(), DeskSignalFacadeError> {
        log::debug!("Received binary message: {} bytes", _bin.len());
        Ok(())
    }

    pub async fn ping(&mut self, bin: Bytes) -> Result<(), DeskSignalFacadeError> {
        self.connection_state
            .session
            .write()
            .await
            .pong(&bin)
            .await?;
        Ok(())
    }

    pub async fn do_handle_signaling(
        &mut self,
        mut stream: AggregatedMessageStream,
    ) -> Result<(), DeskSignalFacadeError> {
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(AggregatedMessage::Text(text)) => {
                    // echo text message
                    match self.handle_message(text).await {
                        Ok(MessageControl::Continue) => {}
                        Ok(MessageControl::Close) => {
                            let session = self.connection_state.session.read().await.clone();
                            let _ = session.close(None).await;
                            break;
                        }
                        Err(e) => {
                            log::error!("Error handling signaling message: {}", e);
                        }
                    }
                }

                Ok(AggregatedMessage::Binary(bin)) => {
                    // echo binary message
                    if let Err(e) = self.binary(bin).await {
                        log::error!("Error handling binary message: {}", e);
                    }
                }

                Ok(AggregatedMessage::Ping(msg)) => {
                    // respond to PING frame with PONG frame
                    self.ping(msg).await?;
                }
                Ok(AggregatedMessage::Pong(_)) => {
                    // ignore PONG frames
                }
                Ok(AggregatedMessage::Close(close_reason)) => {
                    log::warn!("WS close frame received: {:?}", close_reason);
                    break;
                }
                Err(e) => {
                    log::error!("WS error: {}", e);
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
