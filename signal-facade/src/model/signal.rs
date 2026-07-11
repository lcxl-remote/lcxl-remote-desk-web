use std::{collections::BTreeMap, time::Duration};

use desk_utils::error::{CustomDeskError, DeskErrorCode};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use strum_macros::{Display, EnumIter, FromRepr};
use utoipa::{IntoParams, ToSchema};
use utoipa_repr::ToSchema_repr;
use uuid::Uuid;
use webrtc::{
    ice_transport::{ice_connection_state::RTCIceConnectionState, ice_server::RTCIceServer},
    peer_connection::{
        peer_connection_state::RTCPeerConnectionState,
        sdp::session_description::RTCSessionDescription,
    },
};

use crate::{
    error::DeskSignalFacadeError,
    model::{
        audio_capture::AudioDevice,
        desk_settings::DeskSettings,
        image_capture::DisplayInfo,
        os::OperationSystemEnum,
        security_settings::SecuritySettings,
        virtual_display::{DEFAULT_ADAPTIVE_DEBOUNCE_MS, DEFAULT_ADAPTIVE_MIN_DELTA_PX},
    },
};

#[derive(
    Copy,
    Clone,
    Debug,
    Display,
    PartialEq,
    Eq,
    FromRepr,
    EnumIter,
    ToSchema_repr,
    Serialize_repr,
    Deserialize_repr,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
/// Signaling Type
#[repr(i32)]
// wincode's default `u32` positional tag would silently disagree with the
// `Serialize_repr` JSON form (which emits the explicit `i32` discriminant).
// Lock the wincode tag to the same `i32` encoding so the daemon ↔ worker
// wire bytes and the browser-facing JSON identify the same variant by the
// same number.
#[wincode(tag_encoding = "i32")]
pub enum SignalingType {
    /// Heartbeat for keeping WebSocket alive through reverse proxies
    #[wincode(tag = 1)]
    Heartbeat = 1,

    // /// API version, should not be used
    // Version = 11,
    /// Request list connections
    #[wincode(tag = 21)]
    FetchConnections = 21,

    /// Response session list
    #[wincode(tag = 22)]
    ConnectionList = 22,

    /// Signaling server → Server peers: a connection has just left the
    /// server's connection map. The signaling server fans this out
    /// (currently only when a `Browser` peer exits) so the daemon-side
    /// PC manager can release per-`connection_id` resources (DXGI
    /// duplication, encoder, IPC senders, …) immediately, without
    /// waiting for the multi-second ICE `disconnected → failed`
    /// fallback. Carries the departed peer's `connection_id` in
    /// `from_connection_id`; data payload is intentionally empty.
    #[wincode(tag = 23)]
    ConnectionRemoved = 23,

    /// WebRTC request remote access
    #[wincode(tag = 100)]
    RequestRemote = 100,
    /// WebRTC init signaling type
    #[wincode(tag = 101)]
    Init = 101,
    /// WebRTC offer signaling type
    #[wincode(tag = 102)]
    Offer = 102,
    /// WebRTC answer signaling type
    #[wincode(tag = 103)]
    Answer = 103,
    /// WebRTC CANID signaling type
    #[wincode(tag = 104)]
    Canid = 104,

    #[wincode(tag = 201)]
    RequireControl = 201,
    #[wincode(tag = 202)]
    AcceptControl = 202,
    #[wincode(tag = 203)]
    DenyControl = 203,
    #[wincode(tag = 204)]
    CloseControl = 204,
    #[wincode(tag = 205)]
    ChangeDisplaySettings = 205,

    /// Enable or disable private screen mode
    #[wincode(tag = 206)]
    EnablePrivateScreen = 206,
    /// Private screen state changed notification
    #[wincode(tag = 207)]
    PrivateScreenStateChanged = 207,
    /// Audio playback error notification
    #[wincode(tag = 208)]
    AudioPlaybackError = 208,

    /// Manager → host (desk server) over its dedicated Support upstream: the
    /// temporary support code the manager issued for this connection, with its
    /// expiry (see [`SupportCodeIssuedData`]). The host displays it to the local
    /// user, who passes it out-of-band to a supporter. Server-originated only — a
    /// client that sends this inbound is misbehaving and the signal drops it.
    #[wincode(tag = 210)]
    SupportCodeIssued = 210,

    #[wincode(tag = 301)]
    UpdateDeskSettings = 301,

    #[wincode(tag = 10003)]
    ManagerSystemInfo = 10003,
    #[wincode(tag = 10004)]
    ManagerSystemStatue = 10004,

    #[wincode(tag = 10005)]
    ManagerFileList = 10005,
    #[wincode(tag = 10006)]
    ManagerFileDelete = 10006,
    /// Start terminal
    #[wincode(tag = 10007)]
    StartTerminal = 10007,
    /// Send data to terminal
    #[wincode(tag = 10008)]
    SendDataToTerminal = 10008,
    /// Resize terminal
    #[wincode(tag = 10009)]
    ResizeTerminal = 10009,
    /// Close terminal
    #[wincode(tag = 10010)]
    CloseTerminal = 10010,
    /// Reply from terminal
    #[wincode(tag = 10011)]
    ReplyFromTerminal = 10011,
    /// List terminal
    #[wincode(tag = 10012)]
    ListTerminal = 10012,
    /// Terminal started
    #[wincode(tag = 10013)]
    TerminalStarted = 10013,
    /// Terminal closed
    #[wincode(tag = 10014)]
    TerminalClosed = 10014,
    /// Query remote system settings via signaling
    #[wincode(tag = 10015)]
    ManagerQuerySettings = 10015,
    /// Update remote system settings via signaling
    #[wincode(tag = 10016)]
    ManagerUpdateSettings = 10016,

    /// ServiceDaemon → Browser: desktop is switching, WebRTC will drop shortly
    #[wincode(tag = 500)]
    DesktopSwitching = 500,
    /// ServiceDaemon → Browser: new Worker is ready, reconnect now
    #[wincode(tag = 501)]
    DesktopReady = 501,

    /// AI agent capability request (control end / orchestrator → host).
    /// Carries `desk_agent_protocol::AgentRequestData` as signaling_data;
    /// the daemon stamps the trusted fields and forwards a typed
    /// `ServiceToWorker::AgentRequest` to the worker.
    #[wincode(tag = 600)]
    AgentRequest = 600,
    /// AI agent capability response (host → control end). Carries
    /// `desk_agent_protocol::AgentOutcome` as signaling_data.
    #[wincode(tag = 601)]
    AgentResponse = 601,

    /// AI Diagnose request (control end → host). Carries
    /// `desk_agent_protocol::diagnose::DiagnoseRequestData` as signaling_data;
    /// the daemon runs the diagnose orchestrator (Default / DeskServer) or
    /// replies `FEATURE_UNAVAILABLE` (ServiceDaemon).
    #[wincode(tag = 602)]
    Diagnose = 602,
    /// AI Diagnose streamed event (host → control end). Carries
    /// `desk_agent_protocol::diagnose::DiagnoseEvent` as signaling_data.
    /// Notification-style (`response_state = None`) so multiple frames reach
    /// the control end instead of being consumed by the one-shot callback map.
    #[wincode(tag = 603)]
    DiagnoseEvent = 603,
    /// AI Diagnose cancellation (control end → host). Sent when the operator
    /// hands a diagnosis off to a human ("转人工"). Carries no payload; the
    /// message `request_id` correlates the cancelled diagnosis. Handoff has no
    /// orchestrator state-machine branch — the daemon only records an
    /// `ai.task.cancelled` audit so the handoff is auditable.
    #[wincode(tag = 604)]
    DiagnoseCancel = 604,

    /// AI confirmed-execution: classify/preview request (control end → host).
    /// Carries `desk_agent_protocol::exec::ConfirmExecData` as signaling_data;
    /// the daemon classifies the command and replies with `ExecPreview`.
    #[wincode(tag = 605)]
    ConfirmExec = 605,
    /// AI confirmed-execution: preview result (host → control end). Carries
    /// `desk_agent_protocol::exec::ExecPreview`. Notification-style
    /// (`response_state = None`); daemon-owned, never accepted inbound.
    #[wincode(tag = 606)]
    ExecPreview = 606,
    /// AI confirmed-execution: approve / reject a previewed execution
    /// (control end → host). Carries `desk_agent_protocol::exec::ResolveExecData`.
    #[wincode(tag = 607)]
    ResolveExec = 607,
    /// AI confirmed-execution: execution result (host → control end). Carries
    /// `desk_agent_protocol::exec::ExecResultPayload` (tagged with
    /// `exec_request_id` for row backfill). Notification-style
    /// (`response_state = None`); daemon-owned, never accepted inbound.
    #[wincode(tag = 609)]
    ExecResult = 609,
    /// AI audit event (host → manager only). Carries
    /// `desk_agent_protocol::audit::AiAuditEventPayload`. Reported by a desk
    /// server to its manager for persistence into `ai_audit_event`; consumed by
    /// the manager's audit observer, never relayed to a browser (it must not
    /// re-enter the control-end broadcast lane). Notification-style
    /// (`response_state = None`).
    #[wincode(tag = 608)]
    AiAuditEvent = 608,
    /// Command-template sync (manager → desk-server daemon only). Carries
    /// `desk_agent_protocol::command_template::CommandTemplateSyncPayload`: the
    /// full enabled operator template set. The manager pushes it on link
    /// establishment and on any template change; the daemon replaces its cache
    /// and unions the templates with its built-in baseline at classify time.
    /// Accepted only from the trusted manager link (the inbound source gate
    /// drops it from any other source). Notification-style
    /// (`response_state = None`).
    #[wincode(tag = 610)]
    CommandTemplateSync = 610,
    /// Remote-collect request (manager → desk-server daemon only). Carries
    /// `desk_agent_protocol::diagnose::CollectRequest`. In the thin-edge model
    /// the diagnose orchestrator runs centrally; the manager pushes this over the
    /// established desk-server link to ask the edge to run its read-only
    /// collectors. Accepted only from the trusted manager link (the inbound
    /// source gate drops it from any other source). Notification-style
    /// (`response_state = None`).
    #[wincode(tag = 611)]
    CollectRequest = 611,
    /// Remote-collect response (desk-server daemon → manager only). Carries
    /// `desk_agent_protocol::diagnose::CollectResponse` (a chunk of the evidence
    /// snapshot or a wholesale error). Consumed by the manager's orchestrator
    /// pending store, never relayed to a browser or another peer. Notification-
    /// style (`response_state = None`).
    #[wincode(tag = 612)]
    CollectResponse = 612,

    /// Fleet batch-execution request (manager → desk-server daemon only).
    /// Carries `AuthorizedControlPayload<desk_agent_protocol::exec::ExecPlan>` —
    /// a manager-sealed, approved execution plan plus the
    /// `desk_agent_protocol::authz::AuthorizationBlock` that scopes it (max_risk,
    /// actor/device, per-attempt request_id binding, audience, and expiry). The
    /// daemon re-validates (PEP) before handing the argv to the worker. Accepted
    /// only from the trusted manager link (the inbound source gate drops it from
    /// any other source); a client sending it inbound to the signaling server is
    /// a protocol error and is swallowed. Notification-style
    /// (`response_state = None`).
    #[wincode(tag = 613)]
    EdgeExecRequest = 613,
    /// Fleet batch-execution result (desk-server daemon → manager only). Carries
    /// `desk_agent_protocol::edge_exec::EdgeExecResultPayload` (the per-attempt
    /// `request_id` + a structured `EdgeExecDisposition`). Consumed by the
    /// manager's execution pending store, never relayed to a browser or another
    /// peer. Notification-style (`response_state = None`).
    #[wincode(tag = 614)]
    EdgeExecResult = 614,

    /// Remote read-tool request (manager owner instance → desk-server daemon
    /// only). Carries `desk_agent_protocol::remote_tool::RemoteToolRequest`: one
    /// server-stamped capability call the agentic loop (running centrally on the
    /// manager) wants the edge to run. The owning instance writes it directly to
    /// the edge's session; a client sending it inbound to the signaling server is a
    /// protocol error and is swallowed. Notification-style (`response_state = None`).
    #[wincode(tag = 615)]
    RemoteToolRequest = 615,
    /// Remote read-tool response (desk-server daemon → manager owner instance
    /// only). Carries `desk_agent_protocol::remote_tool::RemoteToolResponse` (a
    /// chunk of the already-redacted result or a wholesale error). Consumed by the
    /// manager's remote-tool pending store, never relayed to a browser or another
    /// peer. Notification-style (`response_state = None`).
    #[wincode(tag = 616)]
    RemoteToolResponse = 616,

    /// In-terminal AI copilot request (control end → host / manager). Carries
    /// `desk_agent_protocol::terminal_copilot::TerminalCopilotAsk` as
    /// signaling_data. Like `Diagnose`, it is a manager-owned AI control frame:
    /// in the manager the control authorizer runs it centrally; in the signal
    /// server (no authorizer) it relays to the host that runs the copilot. The
    /// target device rides the outer `to_connection_id`, not the payload.
    #[wincode(tag = 617)]
    TerminalCopilotAsk = 617,
    /// In-terminal AI copilot streamed event (host / manager → control end).
    /// Carries `desk_agent_protocol::terminal_copilot::TerminalCopilotEvent`.
    /// Notification-style (`response_state = None`) so multiple frames reach the
    /// control end instead of being consumed by the one-shot callback map.
    #[wincode(tag = 618)]
    TerminalCopilotEvent = 618,
    /// In-terminal AI copilot cancellation (control end → host / manager). Sent
    /// when the operator dismisses an in-flight copilot turn; the message
    /// `request_id` correlates the cancelled turn. Routed like `DiagnoseCancel`.
    #[wincode(tag = 619)]
    TerminalCopilotCancel = 619,

    /// In-terminal AI command completion request (control end → host / manager).
    /// Carries `desk_agent_protocol::terminal_complete::TerminalCompleteAsk` as
    /// signaling_data. Like `TerminalCopilotAsk`, it is a manager-owned AI control
    /// frame: in the manager the control authorizer runs it centrally; in the
    /// signal server (no authorizer) it relays to the host that completes it. The
    /// target device rides the outer `to_connection_id`, not the payload.
    #[wincode(tag = 620)]
    TerminalCompleteAsk = 620,
    /// In-terminal AI command completion response (host / manager → control end).
    /// Carries `desk_agent_protocol::terminal_complete::TerminalCompleteResult`.
    /// Non-streaming (one response per ask); the `response_state = None`
    /// notification lane keeps it off the one-shot callback map, and the control
    /// end discards a result whose `request_id` is no longer the active one.
    #[wincode(tag = 621)]
    TerminalCompleteResult = 621,

    /// Command-blocklist sync (manager → desk-server daemon only). Carries
    /// `desk_agent_protocol::command_blocklist::CommandBlocklistSyncPayload`: the
    /// full effective blocklist set (the built-in floor minus admin-disabled rules,
    /// plus enabled custom rules). The manager pushes it on link establishment and
    /// on any blocklist change; the daemon replaces its cache (gated on a monotonic
    /// revision) and matches against it before tokenization at classify time.
    /// Accepted only from the trusted manager link (the inbound source gate drops
    /// it from any other source). Notification-style (`response_state = None`).
    #[wincode(tag = 622)]
    CommandBlocklistSync = 622,

    /// Error
    #[wincode(tag = -1)]
    Error = -1,
    /// Unrecognized signaling type will map to this on the JSON path
    /// (via `#[serde(other)]`). The wincode wire never emits this
    /// variant — daemon and worker are version-locked so an "unknown"
    /// discriminant cannot reach the IPC boundary. We still assign it
    /// a wincode tag so the type implements `SchemaWrite` / `SchemaRead`.
    #[serde(other)]
    #[wincode(tag = -100)]
    Unknown = -100,
}

#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingResponseState {
    /// error code
    ///
    /// see alse: desk_utils::DeskErrorCode
    pub error_code: i32,
    /// error message
    pub message: Option<String>,
}

impl SignalingResponseState {
    pub fn success() -> Self {
        Self {
            error_code: DeskErrorCode::SUCCESS.code(),
            message: None,
        }
    }

    pub fn is_success(&self) -> bool {
        self.error_code == DeskErrorCode::SUCCESS.code()
    }
}
/// Signaling model
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct SignalingModel {
    /// Request id
    pub request_id: String,
    /// Signaling type
    pub signaling_type: SignalingType,
    /// From connection id, if None, means from signal server
    pub from_connection_id: Option<String>,
    /// To connection id, if None, means to signal server
    pub to_connection_id: Option<String>,
    /// Signaling data
    signaling_data: Option<serde_json::Value>,
    /// Signaling response state. Some means this is a response message.
    pub response_state: Option<SignalingResponseState>,
}

impl SignalingModel {
    pub fn new(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        signaling_data: Option<serde_json::Value>,
        response_state: Option<SignalingResponseState>,
    ) -> Self {
        Self {
            request_id: request_id.to_string(),
            signaling_type,
            from_connection_id,
            to_connection_id,
            signaling_data,
            response_state,
        }
    }

    /// New request signaling model
    pub fn new_request<T>(
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        signaling_data: Option<&T>,
    ) -> Result<Self, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize,
    {
        Ok(Self::new(
            &Uuid::new_v4().to_string(),
            signaling_type,
            None,
            to_connection_id,
            signaling_data.map(serde_json::to_value).transpose()?,
            None,
        ))
    }

    /// New response signaling model
    pub fn new_response<T>(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        signaling_data: Option<&T>,
        response_state: SignalingResponseState,
    ) -> Result<Self, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize,
    {
        Ok(Self::new(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            signaling_data.map(serde_json::to_value).transpose()?,
            Some(response_state),
        ))
    }

    /// New success response signaling model
    pub fn success_response<T>(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        signaling_data: Option<&T>,
    ) -> Result<Self, DeskSignalFacadeError>
    where
        T: ?Sized + Serialize,
    {
        Self::new_response(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            signaling_data,
            SignalingResponseState::success(),
        )
    }

    /// New response signaling model with none data
    pub fn error(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        error_code: DeskErrorCode,
        message: &str,
    ) -> Result<Self, DeskSignalFacadeError> {
        let error_data = SignalingResponseState {
            error_code: error_code.code(),
            message: Some(message.to_string()),
        };
        Self::new_response::<()>(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            None,
            error_data,
        )
    }

    pub fn custom_desk_error(
        request_id: &str,
        signaling_type: SignalingType,
        from_connection_id: Option<String>,
        to_connection_id: Option<String>,
        error: CustomDeskError,
    ) -> Result<Self, DeskSignalFacadeError> {
        Self::error(
            request_id,
            signaling_type,
            from_connection_id,
            to_connection_id,
            error.error_code,
            &error.message,
        )
    }

    /// Get data with type
    pub fn get_data_with_type<T>(&self) -> Result<Option<T>, DeskSignalFacadeError>
    where
        T: for<'a> Deserialize<'a>,
    {
        let value = if let Some(data) = &self.signaling_data {
            Some(serde_json::from_value(data.clone())?)
        } else {
            None
        };
        Ok(value)
    }

    /// Get data with type
    pub fn get_data_with_default<T>(&self) -> Result<T, DeskSignalFacadeError>
    where
        T: for<'a> Deserialize<'a> + Default,
    {
        let value = if let Some(data) = &self.signaling_data {
            serde_json::from_value(data.clone())?
        } else {
            T::default()
        };
        Ok(value)
    }

    /// Get data with type, if data is none, will throw error
    pub fn get_data<T>(&self) -> Result<T, DeskSignalFacadeError>
    where
        T: for<'a> Deserialize<'a>,
    {
        let data_opt = self.get_data_with_type::<T>()?;
        if let Some(data) = data_opt {
            Ok(data)
        } else {
            DeskSignalFacadeError::custom_error(
                DeskErrorCode::BLANK_SIGNALING_DATA,
                &format!("Data can't be none, signal type: {}", self.signaling_type),
            )
        }
    }

    pub fn get_raw_data(&self) -> &Option<serde_json::Value> {
        &self.signaling_data
    }

    pub fn check_and_get_from_connection_id(&self) -> Result<&str, DeskSignalFacadeError> {
        if let Some(from_connection_id) = &self.from_connection_id {
            Ok(from_connection_id.as_str())
        } else {
            DeskSignalFacadeError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "From connection id can't be none, signal type: {}",
                    self.signaling_type
                ),
            )
        }
    }

    pub fn check_and_get_to_connection_id(&self) -> Result<&str, DeskSignalFacadeError> {
        if let Some(to_connection_id) = &self.to_connection_id {
            Ok(to_connection_id.as_str())
        } else {
            DeskSignalFacadeError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "To connection id can't be none, signal type: {}",
                    self.signaling_type
                ),
            )
        }
    }

    pub fn is_request(&self) -> bool {
        self.response_state.is_none()
    }

    pub fn is_response(&self) -> bool {
        self.response_state.is_some()
    }
}

/// Peer signaling sender trait
pub trait PeerSignalingSender {
    /// Send signaling message
    fn send_response<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        signaling_data: &T,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send
    where
        T: ?Sized + Serialize + Sync;

    fn send_error(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: Option<String>,
        error_code: DeskErrorCode,
        error_message: &str,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send;

    /// Send to peer session
    fn send_to_peer<T>(
        &mut self,
        request_id: &str,
        signaling_type: SignalingType,
        to_connection_id: &str,
        data: T,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send
    where
        T: Serialize + Sync + Send;
}

pub trait ForwardSignalingSender {
    /// Send response signaling message
    fn send_response(
        &self,
        from_connection_id: Option<String>,
        signaling_model: &SignalingModel,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send;

    /// Forward to peer session
    fn send_to_peer(
        &self,
        from_connection_id: &str,
        signaling_model: &SignalingModel,
    ) -> impl std::future::Future<Output = Result<(), DeskSignalFacadeError>> + Send;

    /// Send request signaling message with callback
    /// There is no from_connection_id in this function, because it is used by http api
    fn request_peer_with_callback<T>(
        &self,
        signaling_type: SignalingType,
        data: Option<&T>,
        timeout: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<SignalingModel, DeskSignalFacadeError>> + Send
    where
        T: ?Sized + Serialize + Sync;
}

/// Turn transport type
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Hash, ToSchema)]
pub enum TurnTransport {
    /// Stun transport
    Stun,
    /// Turn transport
    Turn,
}

/// RTC IceServer
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize, Hash, ToSchema)]
pub struct LcxlRTCIceServer {
    /// List of URLs associated with the ICE server, e.g. ["stun:stun.l.google.com:19302"]
    pub urls: Vec<String>,
    /// Username for the ICE server, if any.
    pub username: String,
    /// Credential for the ICE server, if any.
    pub credential: String,
}

impl LcxlRTCIceServer {
    /// Get transport type from url
    pub fn transport(&self) -> Option<TurnTransport> {
        if self.urls.is_empty() {
            return None;
        }
        let url = self.urls[0].clone();
        if url.starts_with("stun:") {
            Some(TurnTransport::Stun)
        } else if url.starts_with("turn:") {
            Some(TurnTransport::Turn)
        } else {
            None
        }
    }
}

impl From<RTCIceServer> for LcxlRTCIceServer {
    fn from(value: RTCIceServer) -> Self {
        LcxlRTCIceServer {
            urls: value.urls,
            username: value.username,
            credential: value.credential,
        }
    }
}

impl From<&LcxlRTCIceServer> for RTCIceServer {
    fn from(val: &LcxlRTCIceServer) -> Self {
        RTCIceServer {
            urls: val.urls.clone(),
            username: val.username.clone(),
            credential: val.credential.clone(),
        }
    }
}
/// RequestRemoteModel is used to request remote access.
/// web browser -> signaling server -> desk server
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct RequestRemoteModel {
    /// ICE servers, the value comes from signaling server
    #[serde(default)]
    pub ice_servers: Vec<LcxlRTCIceServer>,
    /// Browser-writable, **untrusted** selector naming which grant session this
    /// request redeems (set after redeeming a device / support code). It only
    /// *selects* a grant; the authorization fact — whether it is honored and what
    /// capability ceiling it carries — is decided server-side by looking the grant
    /// up and checking the caller's server-resolved principal, and is stamped into
    /// the trusted [`super::request_remote_authz::RequestRemoteAuthz`]. A browser
    /// presenting someone else's `grant_session_id` is rejected at that principal
    /// check. `None` on a normal owner/org request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_session_id: Option<String>,
}

/// Browser-facing knobs that drive the adaptive-resolution hook. Server
/// sources these from `Settings.virtual_display.adaptive_*` and ships
/// them through `InitSignalingData` so each browser session uses the
/// host operator's preference without round-tripping a separate REST
/// query.
///
/// `adaptive_throttle_ms` is intentionally NOT included — it is the
/// daemon's defensive rate limit and the browser does not need to know
/// it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(default)]
pub struct AdaptiveResolutionParams {
    /// Trailing-edge debounce window (ms) the browser waits after a
    /// `resize` settles before issuing an auto ChangeDisplaySettings.
    pub debounce_ms: u64,
    /// Minimum pixel delta on either axis the browser treats as
    /// significant. Below this threshold the change is ignored.
    pub min_delta_px: u32,
}

impl Default for AdaptiveResolutionParams {
    fn default() -> Self {
        Self {
            debounce_ms: DEFAULT_ADAPTIVE_DEBOUNCE_MS,
            min_delta_px: DEFAULT_ADAPTIVE_MIN_DELTA_PX,
        }
    }
}

/// InitSignalingData is used to initialize signaling data.
/// desk server -> signaling server -> web browser
/// see https://github.com/webrtc-rs/webrtc/blob/254bdd5d970933e847dc000de9545040ce16f19f/webrtc/src/peer_connection/configuration.rs
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InitSignalingData {
    // ICE servers, the value comes from signaling server
    pub ice_servers: Vec<LcxlRTCIceServer>,
    /// User name for signaling.
    pub user_name: String,
    /// Audio device list
    pub audio_device_list: BTreeMap<String, Vec<AudioDevice>>,
    /// Audio encoder list
    pub audio_encoder_list: Vec<String>,
    /// Video device list
    pub video_device_list: BTreeMap<String, Vec<DisplayInfo>>,
    /// Video encoder list
    pub video_encoder_list: Vec<String>,
    /// Current desk settings
    pub desk_settings: DeskSettings,
    /// Whether the remote end has Tauri UI support (required for whiteboard overlay)
    #[serde(default)]
    pub has_tauri: bool,
    /// Whether the server is running with administrative privileges
    pub is_admin: bool,
    /// Whether the daemon currently has the IDD virtual display attached
    /// (service-daemon mode + `virtual_display.enabled=true` + attach
    /// resolved). The browser uses this to gate the adaptive-resolution
    /// hook — there is no point firing ChangeDisplaySettings against a
    /// host that does not own the IDD.
    #[serde(default)]
    pub virtual_display_active: bool,
    /// Most-recently-applied IDD refresh rate the daemon has seen via the
    /// worker's VirtualDisplayMode echo. `0` means the daemon has no
    /// observation yet (cold start) — the browser may use it for
    /// display purposes only; the auto path always sends `refresh_hz=0`
    /// and lets the daemon do the authoritative fallback.
    #[serde(default)]
    pub virtual_display_current_refresh_hz: u32,
    /// GDI device name (e.g. `\\.\DISPLAY8`) of the IDD virtual display
    /// when the daemon currently has it attached. `None` when no virtual
    /// display is attached (default mode / IDD detached / Disabled
    /// supervisor). The browser uses this both to label the matching
    /// entry in the display picker AND to gate the adaptive-resolution
    /// hook — auto requests only fire when the captured display equals
    /// this name, otherwise resizing the browser silently changes the
    /// IDD resolution while the worker is capturing a physical monitor.
    #[serde(default)]
    pub virtual_display_device_name: Option<String>,
    /// Browser-side adaptive resolution knobs sourced from
    /// `VirtualDisplaySettings`. Missing in legacy responses ⇒
    /// `AdaptiveResolutionParams::Default` (5000 ms / 16 px).
    #[serde(default)]
    pub adaptive_resolution: AdaptiveResolutionParams,
    /// Operating system of the remote host. Lets the browser tailor
    /// host-targeted UI (e.g. the keyboard-shortcut menu) to the host's
    /// platform instead of assuming Windows. Missing in legacy responses ⇒
    /// `OperationSystemEnum::Other` (unknown host) — NOT the deserializing
    /// machine's own OS, which is what `OperationSystemEnum::default()` yields.
    #[serde(default = "unknown_host_os")]
    pub operation_system: OperationSystemEnum,
}

/// Serde fallback for a host that does not advertise its OS. Unlike
/// `OperationSystemEnum::default()` (which resolves to the *local* compile-time
/// OS, the right answer when a host reports its own OS) a decoded-but-absent
/// field means the host OS is simply unknown.
fn unknown_host_os() -> OperationSystemEnum {
    OperationSystemEnum::Other
}

/// WebRTC Connection State
// `UpdateSettings` carries a full `DeskSettings` payload which dwarfs the other
// variants. Boxing it would ripple through every `match` site without a real
// runtime gain on this rarely-cloned enum, so we accept the size delta.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum WebRTConnectionState {
    Init,
    Connected,
    UpdateSettings(DeskSettings),
    Closed,
}

impl std::fmt::Display for WebRTConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<&RTCIceConnectionState> for WebRTConnectionState {
    fn from(value: &RTCIceConnectionState) -> Self {
        match value {
            RTCIceConnectionState::Unspecified
            | RTCIceConnectionState::New
            | RTCIceConnectionState::Checking => WebRTConnectionState::Init,
            RTCIceConnectionState::Connected => WebRTConnectionState::Connected,
            _ => WebRTConnectionState::Closed,
        }
    }
}

impl From<&RTCPeerConnectionState> for WebRTConnectionState {
    fn from(value: &RTCPeerConnectionState) -> Self {
        match value {
            RTCPeerConnectionState::Unspecified
            | RTCPeerConnectionState::New
            | RTCPeerConnectionState::Connecting => WebRTConnectionState::Init,
            RTCPeerConnectionState::Connected => WebRTConnectionState::Connected,
            _ => WebRTConnectionState::Closed,
        }
    }
}

/// Signaling State
#[derive(Debug, Clone, Default)]
pub struct SignalingState {
    /// accept control from remote peer
    pub accept_control: bool,
    /// accept clipboard sync from remote peer
    pub accept_clipboard_sync: bool,
    /// The validated capability ceiling for this connection, unwrapped from the
    /// `RequestRemoteAuthz` stamp by the host gate. `None` for a central-verified
    /// owner/full session (no ceiling) or a plain unrestricted connection;
    /// `Some(_)` for a redeemed-grant session whose effective capabilities are
    /// `meet(ceiling, global)` at each worker-side permission gate. Host-local
    /// runtime state, never carried on the wire; a plain signal leaves it `None`.
    pub access_ceiling: Option<SecuritySettings>,
    /// The grant logical-session id this connection belongs to, copied from the
    /// stamp so the daemon can index connections by grant (directed teardown /
    /// revocation) instead of by the coarse restricted-set. `None` for owner /
    /// unrestricted / legacy-support connections. Host-local runtime state.
    pub grant_session_id: Option<String>,
    /// current display info
    pub display_info: DisplayInfo,
    /// wayland control mode: portal/uinput/auto/none
    pub wayland_control_mode: Option<String>,
}

/// Offer Model
/// web browser -> signaling server -> desk server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferModel {
    /// offer session description
    pub offer: RTCSessionDescription,
    /// desk settings
    pub desk_settings: DeskSettings,
}

/// Remote Desk Type Enum
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[schema(rename_all = "kebab-case")]
pub enum RemoteDeskTypeEnum {
    /// Browser type
    Browser,
    /// Lcxl remote desktop server type
    Server,
    /// Lcxl remote desktop signal type
    Signal,
    /// Lcxl remote desktop manager type,
    /// used for manage multiple remote desktops,
    /// this enum used by another project, not this project
    /// so keep this enum but do not use it
    Manager,
    /// Temporary-support type: a desk server's dedicated, short-lived upstream
    /// connection opened solely to obtain and serve a temporary support session
    /// (a supporter the owner does not otherwise share the device with). Distinct
    /// from its main `Server` connection so it registers no device / presence and
    /// the host can hold it as a restricted, fail-closed session. Only a central
    /// brain (the manager) attaches temp-code semantics to this role; a plain
    /// signal treats it as routing-only.
    Support,
}

/// Request remote access model.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestRemote {
    pub connection_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RequestRemoteData {}

/// Minimal user trait for signaling permission checks.
/// Both web/server-user::CurrentUser and manager::CurrentUser should implement this.
pub trait SignalingUser: Send + Sync {
    fn get_access(&self) -> Option<&str>;
    fn get_target_connection_id(&self) -> Option<&str>;
}

/// Async so an implementation can read live cluster state (e.g. the manager's
/// Redis TURN-node registry) when issuing ICE servers. Every call site is in an
/// async signaling loop, and the cost is paid once per connection/session
/// establishment (not per packet).
#[async_trait::async_trait]
pub trait TurnProvider: Send + Sync {
    async fn get_ice_servers(&self, username: &str, credential: &str) -> LcxlRTCIceServer;

    /// Build an ICE server with a self-signed TURN REST credential for `name`,
    /// valid for `ttl_secs`. Returns `None` when the provider cannot issue one
    /// (no static auth secret / no interface), so callers never inject an
    /// unusable entry. Default `None` keeps non-TURN providers compiling.
    async fn get_rest_ice_servers(&self, _name: &str, _ttl_secs: u64) -> Option<LcxlRTCIceServer> {
        None
    }
}

#[cfg(test)]
mod wincode_tests {
    //! Wincode `SignalingType` coverage. The enum has 38 variants with
    //! explicit `#[repr(i32)]` discriminants, and the wincode tag is
    //! locked to `i32` via `#[wincode(tag_encoding = "i32")]` so the
    //! daemon ↔ worker wire bytes use the same number the JSON wire
    //! emits (via `Serialize_repr`).
    //!
    //! Two tests cover this from different angles:
    //!
    //!   * `signaling_type_round_trips_wincode` — encode + decode each
    //!     variant and assert the decoded value matches the input. This
    //!     catches "did we forget to add `#[derive(...)]` or
    //!     `#[wincode(tag_encoding = ...)]`?" kinds of bugs.
    //!
    //!   * `signaling_type_wire_tag_matches_discriminant_for_all_variants`
    //!     — encode each variant and assert the *first four bytes* of
    //!     the encoded payload equal `(variant as i32).to_le_bytes()`.
    //!     This is the byte-level check the migration plan and code
    //!     review both call out: a round-trip test pairs encode and
    //!     decode, so a `#[wincode(tag = N)]` that silently disagrees
    //!     with the `repr(i32)` discriminant for a single variant
    //!     (e.g. typo `tag = 101` on a `= 102` variant) would still
    //!     pass round-trip — encode + decode would both use the same
    //!     wrong tag. Only by asserting against the *expected*
    //!     discriminant separately do we catch tag drift.
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    /// Table of every `SignalingType` variant paired with the explicit
    /// `i32` discriminant it carries. When a new variant is added to
    /// the enum, this table must be extended — leaving it incomplete
    /// is precisely the regression `signaling_type_wire_tag_matches_…`
    /// is built to catch.
    fn all_variants_with_tag() -> [(SignalingType, i32); 59] {
        [
            (SignalingType::Heartbeat, 1),
            (SignalingType::FetchConnections, 21),
            (SignalingType::ConnectionList, 22),
            (SignalingType::ConnectionRemoved, 23),
            (SignalingType::RequestRemote, 100),
            (SignalingType::Init, 101),
            (SignalingType::Offer, 102),
            (SignalingType::Answer, 103),
            (SignalingType::Canid, 104),
            (SignalingType::RequireControl, 201),
            (SignalingType::AcceptControl, 202),
            (SignalingType::DenyControl, 203),
            (SignalingType::CloseControl, 204),
            (SignalingType::ChangeDisplaySettings, 205),
            (SignalingType::EnablePrivateScreen, 206),
            (SignalingType::PrivateScreenStateChanged, 207),
            (SignalingType::AudioPlaybackError, 208),
            (SignalingType::UpdateDeskSettings, 301),
            (SignalingType::DesktopSwitching, 500),
            (SignalingType::DesktopReady, 501),
            (SignalingType::AgentRequest, 600),
            (SignalingType::AgentResponse, 601),
            (SignalingType::Diagnose, 602),
            (SignalingType::DiagnoseEvent, 603),
            (SignalingType::DiagnoseCancel, 604),
            (SignalingType::ConfirmExec, 605),
            (SignalingType::ExecPreview, 606),
            (SignalingType::ResolveExec, 607),
            (SignalingType::ExecResult, 609),
            (SignalingType::AiAuditEvent, 608),
            (SignalingType::CommandTemplateSync, 610),
            (SignalingType::CollectRequest, 611),
            (SignalingType::CollectResponse, 612),
            (SignalingType::EdgeExecRequest, 613),
            (SignalingType::EdgeExecResult, 614),
            (SignalingType::RemoteToolRequest, 615),
            (SignalingType::RemoteToolResponse, 616),
            (SignalingType::TerminalCopilotAsk, 617),
            (SignalingType::TerminalCopilotEvent, 618),
            (SignalingType::TerminalCopilotCancel, 619),
            (SignalingType::TerminalCompleteAsk, 620),
            (SignalingType::TerminalCompleteResult, 621),
            (SignalingType::CommandBlocklistSync, 622),
            (SignalingType::ManagerSystemInfo, 10003),
            (SignalingType::ManagerSystemStatue, 10004),
            (SignalingType::ManagerFileList, 10005),
            (SignalingType::ManagerFileDelete, 10006),
            (SignalingType::StartTerminal, 10007),
            (SignalingType::SendDataToTerminal, 10008),
            (SignalingType::ResizeTerminal, 10009),
            (SignalingType::CloseTerminal, 10010),
            (SignalingType::ReplyFromTerminal, 10011),
            (SignalingType::ListTerminal, 10012),
            (SignalingType::TerminalStarted, 10013),
            (SignalingType::TerminalClosed, 10014),
            (SignalingType::ManagerQuerySettings, 10015),
            (SignalingType::ManagerUpdateSettings, 10016),
            (SignalingType::Error, -1),
            (SignalingType::Unknown, -100),
        ]
    }

    #[test]
    fn signaling_type_round_trips_wincode() {
        let config = unbounded_config();
        for (variant, _expected) in all_variants_with_tag() {
            let bytes = wincode::config::serialize(&variant, config)
                .unwrap_or_else(|err| panic!("encode {variant:?}: {err}"));
            let back: SignalingType = wincode::config::deserialize(&bytes, config)
                .unwrap_or_else(|err| panic!("decode {variant:?}: {err}"));
            assert_eq!(
                back as i32, variant as i32,
                "round-trip mismatch for {variant:?}",
            );
        }
    }

    #[test]
    fn signaling_type_wire_tag_matches_discriminant_for_all_variants() {
        let config = unbounded_config();
        for (variant, expected_tag) in all_variants_with_tag() {
            let bytes = wincode::config::serialize(&variant, config)
                .unwrap_or_else(|err| panic!("encode {variant:?}: {err}"));
            assert!(bytes.len() >= 4, "{variant:?} produced fewer than 4 bytes",);
            let tag = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            assert_eq!(
                tag, expected_tag,
                "wincode wire tag for {variant:?} does not match its repr(i32) discriminant",
            );
        }
    }
}

#[cfg(test)]
mod init_signaling_data_tests {
    use super::*;

    /// Pre-adaptive-resolution peers ship `InitSignalingData` JSON without
    /// the three new fields. `#[serde(default)]` must populate them with
    /// sensible defaults so the daemon stays compatible with anyone still
    /// running the previous release of the signaling facade.
    #[test]
    fn init_signaling_data_legacy_json_defaults_new_fields() {
        let raw = r#"{
            "ice_servers": [],
            "user_name": "tester",
            "audio_device_list": {},
            "audio_encoder_list": [],
            "video_device_list": {},
            "video_encoder_list": [],
            "desk_settings": {},
            "is_admin": false
        }"#;
        let data: InitSignalingData = serde_json::from_str(raw).expect("decode");
        assert!(!data.virtual_display_active);
        assert_eq!(data.virtual_display_current_refresh_hz, 0);
        assert!(
            data.virtual_display_device_name.is_none(),
            "legacy peers without virtual_display_device_name must decode to None",
        );
        assert_eq!(
            data.adaptive_resolution.debounce_ms,
            DEFAULT_ADAPTIVE_DEBOUNCE_MS
        );
        assert_eq!(
            data.adaptive_resolution.min_delta_px,
            DEFAULT_ADAPTIVE_MIN_DELTA_PX
        );
        // Legacy peers predate the host-OS field; it must default to Other so
        // the browser falls back to a generic (Windows) shortcut menu rather
        // than mislabelling the host.
        assert_eq!(data.operation_system, OperationSystemEnum::Other);
    }

    /// A host that advertises its OS must round-trip so the browser can tailor
    /// host-targeted UI (e.g. macOS shortcuts) instead of assuming Windows.
    #[test]
    fn init_signaling_data_round_trips_host_os() {
        let raw = r#"{
            "ice_servers": [],
            "user_name": "tester",
            "audio_device_list": {},
            "audio_encoder_list": [],
            "video_device_list": {},
            "video_encoder_list": [],
            "desk_settings": {},
            "is_admin": false,
            "operation_system": "Mac"
        }"#;
        let data: InitSignalingData = serde_json::from_str(raw).expect("decode");
        assert_eq!(data.operation_system, OperationSystemEnum::Mac);

        let encoded = serde_json::to_string(&data).expect("encode");
        let decoded: InitSignalingData = serde_json::from_str(&encoded).expect("re-decode");
        assert_eq!(decoded.operation_system, OperationSystemEnum::Mac);
    }

    /// Empty `AdaptiveResolutionParams` JSON must fall back to the shared
    /// constants. Pin this so a future Default-by-field-init that drifts
    /// from `DEFAULT_ADAPTIVE_*` constants fails the test.
    #[test]
    fn adaptive_resolution_params_legacy_json_defaults_to_5000_16() {
        let p: AdaptiveResolutionParams = serde_json::from_str("{}").expect("decode");
        assert_eq!(p.debounce_ms, 5_000);
        assert_eq!(p.min_delta_px, 16);
        assert_eq!(p, AdaptiveResolutionParams::default());
    }
}
