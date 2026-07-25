//! Extension contracts used by the signaling handler.

use super::*;

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

// ====== AccessGrantAuthorizer (RequestRemote capability-ceiling stamp) ======

/// Outcome of authorizing a `RequestRemote` before it is relayed to the host.
pub enum RequestRemoteOutcome {
    /// Relay this model to the peer. The manager / signal server returns the frame
    /// wrapped in an `AuthorizedRequestRemote` (a trusted capability-ceiling
    /// stamp); with no authorizer the plain frame is relayed unchanged.
    Forward(SignalingModel),
    /// Reject the request (default-deny: neither an owner/org authorization nor a
    /// valid grant for the target). An error response is returned to the sender.
    Reject {
        code: DeskErrorCode,
        message: String,
    },
}

/// Stamps a trusted capability ceiling onto every `RequestRemote` before it is
/// relayed to the host — the enforcement seam that lets the host drop any bare
/// `RequestRemote` on its trusted-central upstream (defense against a grant
/// session stripping its stamp to masquerade as an owner). The manager and the
/// signal server each implement it (rule 22, same shape): an owner / org
/// authorization stamps `access_ceiling: None` (full), a valid redeemed grant
/// stamps `Some(ceiling)`, and neither is a default-deny reject. The signal
/// server injects its own single-account/grant implementation; a handler with no
/// authorizer relays plainly (used where the host applies no central trust).
pub trait RequestRemoteAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RequestRemoteOutcome> + Send + 'a>>;
}

// ====== TerminalStartAuthorizer (StartTerminal capability-ceiling stamp) ======

/// Stamps a trusted capability ceiling onto a `StartTerminal` frame before it is
/// relayed to the host — the terminal analogue of [`RequestRemoteAuthorizer`]. The
/// remote terminal opens on a distinct WS connection that never does a
/// `RequestRemote`, so without this stamp the host has no admission / ceiling for it
/// (and its first door would either reject it or fall back to the host global). The
/// manager and the signal server each implement it (rule 22, same shape): an owner
/// session stamps `access_ceiling: None` (full control), a valid redeemed grant
/// stamps `Some(ceiling)`, and anything else is a default-deny reject. Reuses
/// [`RequestRemoteOutcome`]: `Forward` carries the frame wrapped in an
/// `AuthorizedTerminalStart`, `Reject` fails the terminal open. Unlike
/// `RequestRemoteAuthorizer` this is invoked directly by the terminal WS controller
/// (which builds the `StartTerminal` frame itself) rather than through the signaling
/// handler's per-type dispatch.
pub trait TerminalStartAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RequestRemoteOutcome> + Send + 'a>>;
}

// ====== OwnerPlaneAuthorizer (owner-plane management-frame gate) ======

/// Outcome of authorizing an owner-plane management frame before it is relayed to
/// the host.
pub enum OwnerPlaneOutcome {
    /// The actor is the device owner (or org-authorized); relay the frame.
    Allow,
    /// The actor is not the owner (e.g. a capped grant-holder); default-deny. An
    /// error response is returned to the sender.
    Reject {
        code: DeskErrorCode,
        message: String,
    },
}

/// Default-denies owner-plane device-management frames
/// (`ManagerUpdateSettings` / `ManagerQuerySettings` / `ManagerSystemInfo` /
/// `ManagerSystemStatue` / `ChangeDisplaySettings`) for any sender that is not the
/// target device's owner (or an org-authorized operator). These frames carry no
/// capability ceiling and are meaningful only to the owner, so a capped
/// grant-holder session must never reach them.
///
/// `Some` only in the manager, where the owner/grant distinction is computed
/// per-target (a `CookieAuth` browser is not intrinsically owner or grant — it
/// depends on the target device). The signal server leaves it unset: there the
/// same frames are already denied for code sessions inside `forward_to_peer`
/// (`is_owner_plane_management_frame`), and an owner drives its own device by
/// cookie session, so no central owner-plane gate is needed (rule 22 keeps both
/// control-end flows identical).
pub trait OwnerPlaneAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = OwnerPlaneOutcome> + Send + 'a>>;
}

// ====== AuditObserver trait ======

/// Consumes inbound `AiAuditEvent` frames for persistence. The manager
/// implements this to write the audit row (after re-deriving the trusted
/// subject from the reporting connection's `AuthContext` and applying the
/// persist-level filter); the signal server leaves it unset, so audit frames
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
/// `EdgeExecDisposition` for one central-agent or fleet execution attempt) from
/// a desk-server daemon. Manager and OSS Signal route the result into their
/// respective durable task stores, keyed by the per-attempt `request_id` and
/// validated against the connection that received the matching
/// `EdgeExecRequest`. `source` is the reporting connection (a
/// token-authenticated desk server).
pub trait EdgeExecObserver: Send + Sync {
    fn on_fleet_exec_result<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

// ====== ExecStateReplyObserver trait ======

/// Consumes inbound `ExecStateReply` frames that answer a query the central brain
/// itself issued (a reconcile of an execution whose live result it lost).
/// Manager and OSS Signal feed the reply into a state-query pending store keyed
/// by execution generation and validated against the reporting connection. A
/// reply carrying a `to_connection_id` is a browser-initiated query's answer and
/// is relayed to that peer instead.
pub trait ExecStateReplyObserver: Send + Sync {
    fn on_exec_state_reply<'a>(
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

// ====== SupportCodeMinter trait ======

/// Central-brain lifecycle for temporary support codes: mint on an inbound
/// `RequestSupportCode`, revoke on an inbound `RevokeSupportCode`.
///
/// Only a central brain (the manager) implements this. On mint it resolves
/// `source`'s owner-bound device, stores a short-lived code in shared state and
/// writes the issued code onto `source`'s own session as `SupportCodeIssued`. On
/// revoke it verifies `source` owns the code's device, then drops the code so it
/// can no longer be redeemed. A plain signal server leaves this unset, so both
/// frames are ignored there (support codes are a manager feature). `source` is the
/// requesting connection — a token-authenticated desk server whose regular
/// `Server` upstream carries the frame (there is no dedicated support upstream).
pub trait SupportCodeMinter: Send + Sync {
    fn on_request_support_code<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    fn on_revoke_support_code<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// Frames that may create, extend, or exercise remote access. The central lock
/// gate resolves the target host and rejects these while its durable mirror is
/// locked. Cleanup, heartbeat, and host→central lock maintenance remain allowed.
pub(crate) fn remote_access_frame_requires_unlocked(t: SignalingType) -> bool {
    !matches!(
        t,
        SignalingType::Heartbeat
            | SignalingType::FetchConnections
            | SignalingType::ConnectionList
            | SignalingType::ConnectionRemoved
            | SignalingType::CloseControl
            | SignalingType::CloseTerminal
            | SignalingType::RequestSupportCode
            | SignalingType::SupportCodeIssued
            | SignalingType::RevokeSupportCode
            | SignalingType::RevokeAccessGrant
            | SignalingType::HostRemoteAccessLockRequest
            | SignalingType::HostRemoteAccessLockAck
            | SignalingType::TerminateRemotePeerRequest
            | SignalingType::TerminateRemotePeerAck
            | SignalingType::AiAuditEvent
            | SignalingType::CollectResponse
            | SignalingType::EdgeExecResult
            | SignalingType::ExecStateReply
            | SignalingType::RemoteToolResponse
            | SignalingType::CommandTemplateSync
            | SignalingType::CommandBlocklistSync
            | SignalingType::Error
            | SignalingType::Unknown
    )
}

// ====== Host remote-access lock traits ======

pub enum RemoteAccessAdmissionOutcome {
    Allow,
    Reject {
        code: DeskErrorCode,
        message: String,
    },
}

/// Central durable-lock gate evaluated before any control-end request can reach
/// a host or central orchestration path. Both OSS signal and manager install it.
pub trait RemoteAccessAdmissionAuthorizer: Send + Sync {
    fn authorize<'a>(
        &'a self,
        source: &'a ConnectionState,
        connections: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = RemoteAccessAdmissionOutcome> + Send + 'a>,
    >;
}

/// Consumes authenticated host requests that update the durable central lock
/// mirror or terminate one remote peer, then writes the matching ack to `source`.
pub trait HostRemoteAccessController: Send + Sync {
    fn on_lock_request<'a>(
        &'a self,
        source: &'a ConnectionState,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;

    fn on_terminate_peer_request<'a>(
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
