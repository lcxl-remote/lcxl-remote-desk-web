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
            ConnectionList, ConnectionModel, ConnectionState, FetchConnectionsScope,
            SharedConnectionMap,
        },
        signal::{
            ForwardSignalingSender, InitSignalingData, LcxlRTCIceServer, RemoteDeskTypeEnum,
            RequestRemoteModel, SignalingModel, SignalingType, SignalingUser, TurnProvider,
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

#[cfg(test)]
mod request_remote_ice_tests {
    use super::*;

    /// Fake provider that records nothing — `get_ice_servers` must never be hit
    /// (REQUEST_REMOTE must use the REST path), and `get_rest_ice_servers`
    /// echoes the requested name into the username so the test can assert the
    /// recipient identity was used.
    struct FakeTurn {
        issue: bool,
    }
    #[async_trait::async_trait]
    impl TurnProvider for FakeTurn {
        async fn get_ice_servers(&self, _username: &str, _credential: &str) -> LcxlRTCIceServer {
            unreachable!("REQUEST_REMOTE injection must use get_rest_ice_servers")
        }
        async fn get_rest_ice_servers(&self, name: &str, _ttl: u64) -> Option<LcxlRTCIceServer> {
            self.issue.then(|| LcxlRTCIceServer {
                urls: vec!["turn:host:3478?transport=udp".to_string()],
                username: format!("9999999999:{name}"),
                credential: "pw".to_string(),
            })
        }
    }

    fn model(to: Option<&str>) -> SignalingModel {
        SignalingModel::new(
            "req-1",
            SignalingType::RequestRemote,
            Some("browser-conn".to_string()),
            to.map(str::to_string),
            None,
            None,
        )
    }

    fn provider(issue: bool) -> Arc<dyn TurnProvider> {
        Arc::new(FakeTurn { issue })
    }

    #[tokio::test]
    async fn injects_for_recipient_via_trait_object() {
        let turn = provider(true);
        let ice = build_request_remote_ice(&model(Some("host-1")), Some(&turn), 60)
            .await
            .expect("ice server");
        // Username embeds the RECIPIENT id, proving recipient identity is used
        // (not the sender `browser-conn`), through the trait-object override.
        assert!(ice.username.ends_with(":host-1"));
    }

    #[tokio::test]
    async fn none_without_recipient() {
        let turn = provider(true);
        assert!(
            build_request_remote_ice(&model(None), Some(&turn), 60)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn none_without_turn() {
        assert!(
            build_request_remote_ice(&model(Some("host-1")), None, 60)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn none_when_provider_declines() {
        let turn = provider(false);
        assert!(
            build_request_remote_ice(&model(Some("host-1")), Some(&turn), 60)
                .await
                .is_none()
        );
    }
}

// ====== DeviceCodeService trait ======

/// Trait for device code operations.  
/// Signal implements this with SQLite DB.
/// Manager can return None (no device codes in manager).
pub trait DeviceCodeService: Send + Sync {
    fn get_or_create_device_code(
        &self,
        client_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>, DeskSignalFacadeError>> + Send;
}

/// A no-op implementation that always returns None
pub struct NoOpDeviceCodeService;

impl DeviceCodeService for NoOpDeviceCodeService {
    async fn get_or_create_device_code(
        &self,
        _client_id: &str,
    ) -> Result<Option<String>, DeskSignalFacadeError> {
        Ok(None)
    }
}

// ====== NodeTokenValidator trait ======

/// Trait for validating node tokens (e.g. manager API tokens).
pub trait NodeTokenValidator: Send + Sync {
    fn validate_node_token<'a>(
        &'a self,
        token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>>;
}

// ====== ControlFrameAuthorizer trait ======

/// Outcome of authorizing a control-end AI frame before it is relayed to the
/// host.
pub enum ControlFrameOutcome {
    /// Relay this (possibly wrapped) model to the peer.
    Forward(SignalingModel),
    /// Reject the frame; an error response is returned to the sender.
    Reject {
        code: DeskErrorCode,
        message: String,
    },
    /// The authorizer fully handled the frame; do **not** relay it to the peer.
    /// The manager uses this for `Diagnose` in the thin-edge model: instead of
    /// forwarding the question to the host, it runs the orchestration centrally
    /// (asking the host only for read-only evidence). The signal server never
    /// returns this (it has no authorizer), so its relay behaviour is unchanged.
    Handled,
}

/// Authorizes (and optionally wraps) the control-end AI frames
/// (`AgentRequest` / `Diagnose` / `ConfirmExec`) during relay. The manager
/// implements this as the fleet policy decision point: it resolves the actor
/// (the sending `actor` connection) and the target host (looked up in
/// `connection_map`), evaluates the policy matrix, and wraps the frame in an
/// `AuthorizedControlPayload`. The signal server leaves this unset (no fleet
/// PDP), so frames relay unwrapped.
pub trait ControlFrameAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ControlFrameOutcome> + Send + 'a>>;
}

// ====== AuditObserver trait ======

/// Consumes inbound `AiAuditEvent` frames for persistence. The manager
/// implements this to write the audit row (after re-deriving the trusted
/// subject from the reporting connection's `AuthContext` and applying the
/// retention-level filter); the signal server leaves it unset, so audit frames
/// are simply ignored there. `source` is the reporting connection (a
/// token-authenticated desk server).
pub trait AuditObserver: Send + Sync {
    fn on_audit_event<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== CollectObserver trait ======

/// Consumes inbound `CollectResponse` frames (chunks of an evidence snapshot, or
/// a wholesale error) from a desk-server daemon. The manager implements this to
/// route the chunk into its orchestrator's pending store, keyed by `request_id`
/// and validated against the connection that the matching `CollectRequest` was
/// pushed to; the signal server leaves it unset, so the frames are ignored there.
/// `source` is the reporting connection (a token-authenticated desk server).
pub trait CollectObserver: Send + Sync {
    fn on_collect_response<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== EdgeExecObserver trait ======

/// Consumes inbound `EdgeExecResult` frames (a structured
/// `EdgeExecDisposition` for one fleet execution attempt) from a desk-server
/// daemon. The manager implements this to route the result into its execution
/// pending store, keyed by the per-attempt `request_id` and validated against
/// the connection that the matching `EdgeExecRequest` was pushed to; the signal
/// server leaves it unset, so the frames are ignored there. `source` is the
/// reporting connection (a token-authenticated desk server).
pub trait EdgeExecObserver: Send + Sync {
    fn on_fleet_exec_result<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== RemoteToolObserver trait ======

/// Consumes inbound `RemoteToolResponse` frames (chunks of an already-redacted
/// remote read result, or a wholesale error) from a desk-server daemon. The
/// manager implements this to feed the chunk into its remote-tool pending store,
/// keyed by `request_id` and validated against the connection the matching
/// `RemoteToolRequest` was written to; the signal server leaves it unset, so the
/// frames are ignored there. `source` is the reporting connection (a
/// token-authenticated desk server).
pub trait RemoteToolObserver: Send + Sync {
    fn on_remote_tool_response<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== FetchConnectionsResolver trait ======

/// Resolves a `FetchConnections` request into the connection list to return.
///
/// The signal server leaves this unset and the handler falls back to the local
/// connection map (single instance, correct). The manager implements it to
/// return a cluster-wide, presence-backed and scope-authorized list whose items
/// carry `device_id` / `owner_node_id` — so a control end behind a load balancer
/// sees every device regardless of which instance holds the desk-server socket.
///
/// `requester` is the asking connection (carries the trusted `AuthContext` used
/// for scoped authorization); `scope` is the parsed request payload.
pub trait FetchConnectionsResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        requester: &'a ConnectionState,
        scope: FetchConnectionsScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<ConnectionModel>, DeskSignalFacadeError>>
                + Send
                + 'a,
        >,
    >;
}

// ====== PeerFrameRelay trait ======

/// Result of a cross-instance peer-frame relay attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayOutcome {
    /// The frame was delivered to the peer connection on its owning instance.
    Delivered,
    /// No instance currently holds the target connection (genuinely offline).
    NotFound,
}

/// Relays a signaling frame whose `to_connection_id` this instance does not hold
/// locally, by forwarding it to the instance that does.
///
/// The signal server leaves this unset — it is single-instance, so a local miss is
/// a genuine "connection not found". The manager implements it: it resolves the
/// target connection to its owning instance via the connection-location registry
/// and forwards the frame over an authenticated internal hop, where the owning
/// instance delivers it to the local peer ([`deliver_to_local_peer`]). Both relay
/// directions flow through this — browser→host (OFFER / ICE) and host→browser
/// (ANSWER / ICE) — because each frame is routed independently by its
/// `to_connection_id`, and a browser connection (absent from device presence) is
/// locatable here just like a host.
///
/// The returned future is intentionally **not** `Send`: the whole signaling stack
/// runs on the single-threaded actix runtime (`rt::spawn`) on both the manager and
/// the OSS signal server, and the manager's implementation drives the internal hop
/// with `awc` (which is `!Send`). The trait object itself is `Send + Sync` so it can
/// live in the shared handler.
pub trait PeerFrameRelay: Send + Sync {
    fn relay<'a>(
        &'a self,
        to_connection_id: &'a str,
        from_connection_id: &'a str,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<RelayOutcome, DeskSignalFacadeError>> + 'a>,
    >;
}

/// Place a signaling frame onto the WebSocket of the peer `to_connection_id` if
/// it is held in `connection_map`, rewriting the frame's from/to fields.
///
/// Returns `Ok(true)` if delivered, `Ok(false)` if the target is not in this
/// instance's map. The target [`ConnectionState`] is cloned out and the map lock
/// dropped before the `send_to_peer` await, so the map is never held across I/O.
///
/// Shared by [`SignalingHandler::forward_to_peer`] (its local branch) and the
/// manager's internal relay landing, so cross-instance delivery reproduces
/// single-instance semantics exactly. The `request_callback_map` shortcut stays in
/// `forward_to_peer` (it concerns the *origin* connection awaiting a reply, which
/// is always co-located with that connection); the relay landing only places the
/// frame onto the target peer's socket.
pub async fn deliver_to_local_peer(
    connection_map: &SharedConnectionMap,
    from_connection_id: &str,
    to_connection_id: &str,
    model: &SignalingModel,
) -> Result<bool, DeskSignalFacadeError> {
    let target = {
        let map = connection_map.read().await;
        map.get(to_connection_id).cloned()
    };
    let Some(target) = target else {
        return Ok(false);
    };
    target.send_to_peer(from_connection_id, model).await?;
    Ok(true)
}

/// Decide the outcome for a frame whose target connection is not held locally: try
/// the cross-instance [`PeerFrameRelay`] (if configured), otherwise honor
/// `ignore_connection_not_found` or surface `SESSION_NOT_FOUND`.
///
/// Split out of [`SignalingHandler::forward_to_peer`] so the relay decision is
/// unit-testable without a full handler (which needs a live WS `Session`). With
/// no relay (the signal server) this is exactly the original single-instance
/// behavior: a local miss is a genuine "connection not found".
async fn relay_or_not_found(
    peer_relay: &Option<Arc<dyn PeerFrameRelay>>,
    to_connection_id: &str,
    from_connection_id: &str,
    model: &SignalingModel,
    ignore_connection_not_found: bool,
) -> Result<(), DeskSignalFacadeError> {
    if let Some(relay) = peer_relay {
        match relay
            .relay(to_connection_id, from_connection_id, model)
            .await?
        {
            RelayOutcome::Delivered => return Ok(()),
            // Held by no instance — fall through to the not-found handling.
            RelayOutcome::NotFound => {}
        }
    }

    if ignore_connection_not_found {
        log::warn!(
            "Connection {} is not found to forward signaling, ignore it: {:?}",
            to_connection_id,
            model
        );
        return Ok(());
    }
    DeskSignalFacadeError::custom_error(
        DeskErrorCode::SESSION_NOT_FOUND,
        &format!(
            "Connection {} is not found to forward signaling: {:?}",
            to_connection_id, model
        ),
    )
}

// ====== 通用工具函数 ======

pub fn parse_ip_from_peer_addr(addr: &str) -> Option<IpAddr> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Some(sock.ip());
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Some(ip);
    }
    None
}

pub fn rewrite_mdns_candidate_with_ip(
    signaling_model: &SignalingModel,
    fallback_ip: IpAddr,
) -> Option<SignalingModel> {
    let data = match signaling_model.get_raw_data() {
        Some(d) => d.clone(),
        None => return None,
    };
    let mut obj = match data.as_object() {
        Some(o) => o.clone(),
        None => return None,
    };

    let candidate_value = obj.get("candidate")?;
    let candidate_str = candidate_value.as_str()?;

    if !candidate_str.contains(".local") {
        return None;
    }

    let mut parts = candidate_str
        .split_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if parts.len() < 6 {
        return None;
    }

    let host = parts[4].clone();
    if !host.ends_with(".local") {
        return None;
    }

    parts[4] = fallback_ip.to_string();
    let new_candidate = parts.join(" ");
    obj.insert(
        "candidate".to_string(),
        serde_json::Value::String(new_candidate.clone()),
    );

    log::info!(
        "Rewrote mDNS ICE candidate using signaling peer IP {}: {} -> {}",
        fallback_ip,
        host,
        new_candidate
    );

    Some(SignalingModel::new(
        &signaling_model.request_id,
        signaling_model.signaling_type,
        signaling_model.from_connection_id.clone(),
        signaling_model.to_connection_id.clone(),
        Some(serde_json::Value::Object(obj)),
        signaling_model.response_state.clone(),
    ))
}

/// Run a request/response signaling call against a connection held in the local
/// map, **without holding the map lock across the await**.
///
/// The connection state is cloned under a brief read lock, the guard is dropped,
/// and only then is `request_peer_with_callback` awaited — so a slow desk server
/// can never serialize the whole map or risk a lock held across a 30s round trip.
/// Shared by the file / settings / sysinfo / terminal proxy controllers (and the
/// manager's owner-local handling), so all of them get the same no-lock behavior.
///
/// Returns the peer's response model. A connection absent from the local map
/// yields `REMOTE_DESK_OFFLINE` (`not_found_msg`); a peer error_code in the
/// response is surfaced as that error.
pub async fn request_on_local_connection<T>(
    connection_map: &SharedConnectionMap,
    connection_id: &str,
    signaling_type: SignalingType,
    data: Option<&T>,
    not_found_msg: &str,
) -> Result<SignalingModel, DeskSignalFacadeError>
where
    T: ?Sized + serde::Serialize + Sync,
{
    let state = {
        let guard = connection_map.read().await;
        guard.get(connection_id).cloned()
    };
    let Some(state) = state else {
        return DeskSignalFacadeError::custom_error(
            DeskErrorCode::REMOTE_DESK_OFFLINE,
            not_found_msg,
        );
    };
    let response = state
        .request_peer_with_callback(signaling_type, data, None)
        .await?;
    if let Some(ref response_state) = response.response_state
        && response_state.error_code != 0
    {
        return DeskSignalFacadeError::custom_error(
            DeskErrorCode::new(response_state.error_code),
            &response_state.message.clone().unwrap_or_default(),
        );
    }
    Ok(response)
}

// ====== SignalingHandler ======

/// Generic signaling handler. Usable by both signal server and manager.
pub struct SignalingHandler<U: SignalingUser> {
    pub connection_state: ConnectionState,
    pub connection_map: web::Data<SharedConnectionMap>,
    pub user: U,
    pub turn: Option<Arc<dyn TurnProvider>>,
    /// Fleet policy decision point for the control-end AI frames. `Some` only in
    /// the manager (which wraps the frames with an authorization decision);
    /// `None` in the signal server, where the frames relay unwrapped.
    pub control_authorizer: Option<Arc<dyn ControlFrameAuthorizer>>,
    /// Audit persistence observer for inbound `AiAuditEvent` frames. `Some` only
    /// in the manager (which persists them); `None` elsewhere, where they are
    /// ignored.
    pub audit_observer: Option<Arc<dyn AuditObserver>>,
    /// Remote-collect response consumer for inbound `CollectResponse` frames.
    /// `Some` only in the manager (which feeds them into its orchestrator's
    /// pending store); `None` elsewhere, where they are ignored.
    pub collect_observer: Option<Arc<dyn CollectObserver>>,
    /// Fleet-execution result consumer for inbound `EdgeExecResult` frames.
    /// `Some` only in the manager (which feeds them into its execution pending
    /// store); `None` elsewhere, where they are ignored.
    pub edge_exec_observer: Option<Arc<dyn EdgeExecObserver>>,
    /// Remote read-tool response consumer for inbound `RemoteToolResponse` frames.
    /// `Some` only in the manager (which feeds them into its remote-tool pending
    /// store); `None` elsewhere, where they are ignored.
    pub remote_tool_observer: Option<Arc<dyn RemoteToolObserver>>,
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
            control_authorizer: None,
            audit_observer: None,
            collect_observer: None,
            edge_exec_observer: None,
            remote_tool_observer: None,
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

    /// Attach a remote-tool response consumer (the manager's remote-tool pending
    /// store). The signal server never calls this, so inbound `RemoteToolResponse`
    /// frames are ignored there.
    pub fn with_remote_tool_observer(mut self, observer: Arc<dyn RemoteToolObserver>) -> Self {
        self.remote_tool_observer = Some(observer);
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
        }

        if let Some(tx) = self
            .connection_state
            .request_callback_map
            .write()
            .await
            .remove(&signaling_model.request_id)
        {
            tx.send(signaling_model.clone()).map_err(|_| {
                DeskSignalFacadeError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "Failed to send response to peer",
                )
            })?;
            return Ok(());
        }
        let to_connection_id = signaling_model.check_and_get_to_connection_id()?;
        let from_connection_id = self.connection_state.model.connection_id.clone();

        // Local fast path: the target connection is held by this instance.
        if deliver_to_local_peer(
            &self.connection_map,
            &from_connection_id,
            to_connection_id,
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
            to_connection_id,
            &from_connection_id,
            signaling_model,
            ignore_connection_not_found,
        )
        .await
    }

    /// Handle incoming signaling message
    pub async fn handle_message(&mut self, text: ByteString) -> Result<(), DeskSignalFacadeError> {
        log::debug!("Received text message: {}", text);
        let signaling_model = serde_json::from_str::<SignalingModel>(&text)?;
        match signaling_model.signaling_type {
            SignalingType::Heartbeat => {
                // Respond to heartbeat immediately to keep connection alive
                let response = SignalingModel::success_response::<()>(
                    &signaling_model.request_id,
                    SignalingType::Heartbeat,
                    None,
                    None,
                    None,
                )?;
                self.connection_state.send_response(None, &response).await?;
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
                let connection_list = ConnectionList {
                    current_connection_id: self.connection_state.model.connection_id.clone(),
                    connection_map,
                };

                log::info!("Sending connection list to client: {:?}", connection_list);
                let response = SignalingModel::success_response(
                    &signaling_model.request_id,
                    SignalingType::ConnectionList,
                    None,
                    None,
                    Some(&connection_list),
                )?;
                self.connection_state.send_response(None, &response).await?;
            }
            SignalingType::ConnectionList => {
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
            SignalingType::SendDataToTerminal => {
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

            SignalingType::ReplyFromTerminal | SignalingType::TerminalClosed => {
                self.forward_to_peer(&signaling_model, true).await?;
            }

            SignalingType::Canid => {
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
                    return Ok(());
                }
                self.forward_to_peer(&signaling_model, false).await?;
            }

            SignalingType::RequestRemote => {
                let mut data = signaling_model.get_data_with_default::<RequestRemoteModel>()?;
                // Inject a TURN REST ICE server for the RECIPIENT (the desk
                // server / host this REQUEST_REMOTE is forwarded to) so it can
                // gather srflx/relay candidates for NAT traversal. Keyed on
                // `to_connection_id`, not the sender: a browser requester has no
                // usable TURN identity, which would otherwise leave the host
                // with no ICE servers (`iceServers=0`).
                if let Some(ice_server) = build_request_remote_ice(
                    &signaling_model,
                    self.turn.as_ref(),
                    REQUEST_REMOTE_TURN_TTL_SECS,
                )
                .await
                {
                    data.ice_servers.push(ice_server);
                }
                let data = Some(serde_json::to_value(data)?);
                let new_signaling_model = SignalingModel::new(
                    signaling_model.request_id.as_str(),
                    signaling_model.signaling_type,
                    signaling_model.from_connection_id,
                    signaling_model.to_connection_id,
                    data,
                    signaling_model.response_state,
                );
                self.forward_to_peer(&new_signaling_model, false).await?;
            }

            SignalingType::Init => {
                let mut data = signaling_model.get_data::<InitSignalingData>()?;
                // TODO need to support static auth secret
                // ice servers
                // username is connection_id
                // password is client id
                let client_id_opt = self.connection_state.model.version_info.client_id.clone();
                if let Some(client_id) = client_id_opt {
                    if let Some(turn) = &self.turn {
                        let ice_server = turn
                            .get_ice_servers(
                                &self.connection_state.model.connection_id,
                                &client_id,
                            )
                            .await;
                        if !ice_server.urls.is_empty() {
                            data.ice_servers.push(ice_server);
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
                let data = Some(serde_json::to_value(data)?);
                let new_signaling_model = SignalingModel::new(
                    signaling_model.request_id.as_str(),
                    signaling_model.signaling_type,
                    signaling_model.from_connection_id,
                    signaling_model.to_connection_id,
                    data,
                    signaling_model.response_state,
                );
                self.forward_to_peer(&new_signaling_model, false).await?;
            }
            // Forwarding types
            SignalingType::Offer
            | SignalingType::Answer
            | SignalingType::RequireControl
            | SignalingType::AcceptControl
            | SignalingType::DenyControl
            | SignalingType::CloseControl
            | SignalingType::ChangeDisplaySettings
            | SignalingType::UpdateDeskSettings
            | SignalingType::ManagerFileList
            | SignalingType::ManagerFileDelete
            | SignalingType::ManagerSystemInfo
            | SignalingType::ManagerSystemStatue
            | SignalingType::ManagerQuerySettings
            | SignalingType::ManagerUpdateSettings
            | SignalingType::ListTerminal
            | SignalingType::EnablePrivateScreen
            | SignalingType::PrivateScreenStateChanged
            | SignalingType::TerminalStarted
            | SignalingType::AudioPlaybackError
            | SignalingType::DesktopSwitching
            | SignalingType::DesktopReady
            // AI host → control-end responses are plain relayed types (no
            // authorization injection on the reply path).
            | SignalingType::AgentResponse
            | SignalingType::DiagnoseEvent
            | SignalingType::TerminalCopilotEvent
            | SignalingType::TerminalCompleteResult
            | SignalingType::ExecPreview
            | SignalingType::ExecResult => {
                // Generic forwarding
                self.forward_to_peer(&signaling_model, false).await?;
            }

            // AI audit event (host → manager only). Consumed by the manager's
            // audit observer for persistence; never relayed to a peer (it must
            // not re-enter the control-end broadcast lane). Ignored where no
            // observer is attached (the signal server).
            SignalingType::AiAuditEvent => {
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
            // handoff/abort must cancel the manager's pending collection and
            // record the cancellation rather than be relayed to a host that has
            // no diagnose task. With no authorizer (signal server) it relays to
            // the host that is running the diagnosis, exactly like before.
            // `ResolveExec` is included so the manager can consume an agentic exec
            // approval centrally (it owns the durable work item). A host exec's
            // ResolveExec is relayed unwrapped by the authorizer (`Forward`); with no
            // authorizer (signal server) it relays plainly, exactly like before.
            SignalingType::AgentRequest
            | SignalingType::Diagnose
            | SignalingType::DiagnoseCancel
            | SignalingType::TerminalCopilotAsk
            | SignalingType::TerminalCopilotCancel
            | SignalingType::TerminalCompleteAsk
            | SignalingType::ConfirmExec
            | SignalingType::ResolveExec => {
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
                        ControlFrameOutcome::Handled => return Ok(()),
                    }
                } else {
                    signaling_model
                };
                self.forward_to_peer(&to_forward, false).await?;
            }

            SignalingType::CommandTemplateSync => {
                // Manager → daemon only, originated server-side and written
                // directly to the desk-server's session. A client sending it
                // inbound to the signaling server is a protocol error; swallow
                // it so a control end cannot forge a template sync.
                log::warn!(
                    "Received command-template sync from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::CommandBlocklistSync => {
                // Manager → daemon only, originated server-side and written
                // directly to the desk-server's session. A client sending it
                // inbound to the signaling server is a protocol error; swallow
                // it so a control end cannot forge a blocklist sync.
                log::warn!(
                    "Received command-blocklist sync from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::CollectRequest => {
                // Manager → daemon only, originated server-side and written
                // directly to the desk-server's session. A client sending it
                // inbound to the signaling server is a protocol error; swallow it
                // so a control end cannot forge an evidence-collection request.
                log::warn!(
                    "Received remote-collect request from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::CollectResponse => {
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
            SignalingType::EdgeExecRequest => {
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
            SignalingType::RemoteToolRequest => {
                // Manager owner instance → daemon only, written directly to the
                // desk-server's session. A client sending it inbound to the
                // signaling server is a protocol error; swallow it so a control end
                // cannot forge a remote tool invocation.
                log::warn!(
                    "Received remote-tool request from client {}, ignoring",
                    self.connection_state.model.connection_id
                );
            }
            SignalingType::RemoteToolResponse => {
                // Desk-server daemon → manager only. Consumed by the manager
                // remote-tool pending store; never relayed to a peer. Ignored where
                // no remote-tool consumer is attached (the signal server).
                if let Some(observer) = self.remote_tool_observer.clone() {
                    observer
                        .on_remote_tool_response(&self.connection_state, &signaling_model)
                        .await;
                }
            }
            SignalingType::EdgeExecResult => {
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
        }
        Ok(())
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
                    if let Err(e) = self.handle_message(text).await {
                        log::error!("Error handling signaling message: {}", e);
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
mod tests {
    use super::*;

    /// `ConnectionRemoved` is the wire-level marker the daemon's
    /// signaling router keys off to release per-`connection_id`
    /// resources. The integer discriminant must stay stable across
    /// releases — bumping it would silently desync browsers /
    /// daemons running mismatched builds, and the active cleanup
    /// path the daemon depends on would just drop on the floor at
    /// `SignalingType::Unknown`. Pin both the discriminant and the
    /// JSON wire form.
    #[test]
    fn signaling_type_connection_removed_wire_format_is_stable() {
        // Discriminant: integer 23 is what the JSON deserializer reads.
        // Hard-coded both sides so a `repr(i32)` reorder breaks the test
        // instead of silently shifting the enum's wire value.
        assert_eq!(SignalingType::ConnectionRemoved as i32, 23);

        let json = serde_json::to_string(&SignalingType::ConnectionRemoved)
            .expect("serialize ConnectionRemoved");
        assert_eq!(json, "23");

        let parsed: SignalingType =
            serde_json::from_str("23").expect("deserialize 23 -> ConnectionRemoved");
        assert!(matches!(parsed, SignalingType::ConnectionRemoved));
    }

    /// The terminal command-completion discriminants must stay stable: a browser
    /// and a manager / host on mismatched builds would otherwise desync, with the
    /// frame silently collapsing to `SignalingType::Unknown`. The ask (620) routes
    /// through the AI authorizer branch and the result (621) through the plain
    /// host → control relay branch, exactly like the copilot ask/event pair.
    #[test]
    fn terminal_complete_signaling_discriminants_are_stable() {
        assert_eq!(SignalingType::TerminalCompleteAsk as i32, 620);
        assert_eq!(SignalingType::TerminalCompleteResult as i32, 621);
        assert_eq!(
            serde_json::to_string(&SignalingType::TerminalCompleteAsk).unwrap(),
            "620"
        );
        let parsed: SignalingType = serde_json::from_str("621").unwrap();
        assert!(matches!(parsed, SignalingType::TerminalCompleteResult));
    }

    /// Empty map (no `Server`-type peers around) must skip the
    /// broadcast cleanly. This covers the early-exit path that keeps
    /// the helper safe to call from a `Drop` background task — even
    /// when the connection map has already been drained.
    #[tokio::test]
    async fn broadcast_connection_removed_to_servers_no_op_on_empty_map() {
        let empty = SharedConnectionMap::new();
        // Should return without blocking on anything; the assertion is
        // simply that the future completes promptly under the test
        // runtime's default no-IO budget.
        broadcast_connection_removed_to_servers("conn-bye", &empty).await;
        assert_eq!(empty.read().await.len(), 0);
    }

    /// A target connection absent from this instance's map yields `Ok(false)`
    /// (a local miss), the signal `forward_to_peer` uses to fall back to the
    /// cross-instance relay.
    #[tokio::test]
    async fn deliver_to_local_peer_reports_miss_when_absent() {
        let empty = SharedConnectionMap::new();
        let model = SignalingModel::new(
            "req-1",
            SignalingType::Offer,
            Some("from".to_string()),
            Some("missing".to_string()),
            None,
            None,
        );
        let delivered = deliver_to_local_peer(&empty, "from", "missing", &model)
            .await
            .expect("miss path is not an error");
        assert!(!delivered);
    }

    /// Records the outcome a mock relay should return, so the seam decision can be
    /// exercised without a live WS `Session`.
    struct MockRelay {
        outcome: RelayOutcome,
        called: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl PeerFrameRelay for MockRelay {
        fn relay<'a>(
            &'a self,
            _to: &'a str,
            _from: &'a str,
            _model: &'a SignalingModel,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<RelayOutcome, DeskSignalFacadeError>> + 'a>,
        > {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            let outcome = self.outcome;
            Box::pin(async move { Ok(outcome) })
        }
    }

    fn dummy_model() -> SignalingModel {
        SignalingModel::new(
            "req-1",
            SignalingType::Answer,
            Some("from".to_string()),
            Some("to".to_string()),
            None,
            None,
        )
    }

    fn is_session_not_found(err: &DeskSignalFacadeError) -> bool {
        matches!(
            err,
            DeskSignalFacadeError::CustomError(e) if e.error_code == DeskErrorCode::SESSION_NOT_FOUND
        )
    }

    /// No relay (the signal server) + a local miss is a genuine SESSION_NOT_FOUND.
    #[tokio::test]
    async fn relay_or_not_found_without_relay_is_session_not_found() {
        let model = dummy_model();
        let err = relay_or_not_found(&None, "to", "from", &model, false)
            .await
            .expect_err("a local miss with no relay must error");
        assert!(is_session_not_found(&err));
    }

    /// No relay + `ignore_connection_not_found` swallows the miss (best-effort
    /// frames like terminal replies).
    #[tokio::test]
    async fn relay_or_not_found_without_relay_honors_ignore_flag() {
        let model = dummy_model();
        relay_or_not_found(&None, "to", "from", &model, true)
            .await
            .expect("ignore flag turns a miss into Ok");
    }

    /// A relay that delivers the frame resolves the forward successfully.
    #[tokio::test]
    async fn relay_or_not_found_delivered_is_ok() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let relay: Option<Arc<dyn PeerFrameRelay>> = Some(Arc::new(MockRelay {
            outcome: RelayOutcome::Delivered,
            called: called.clone(),
        }));
        let model = dummy_model();
        relay_or_not_found(&relay, "to", "from", &model, false)
            .await
            .expect("a delivered relay resolves Ok");
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// A relay that reports the target held by no instance still surfaces
    /// SESSION_NOT_FOUND (when not ignoring), and honors the ignore flag otherwise.
    #[tokio::test]
    async fn relay_or_not_found_not_found_falls_through() {
        let called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let relay: Option<Arc<dyn PeerFrameRelay>> = Some(Arc::new(MockRelay {
            outcome: RelayOutcome::NotFound,
            called: called.clone(),
        }));
        let model = dummy_model();
        let err = relay_or_not_found(&relay, "to", "from", &model, false)
            .await
            .expect_err("relay NotFound with no ignore must error");
        assert!(is_session_not_found(&err));
        assert!(called.load(std::sync::atomic::Ordering::SeqCst));

        relay_or_not_found(&relay, "to", "from", &model, true)
            .await
            .expect("relay NotFound with ignore is Ok");
    }
}
