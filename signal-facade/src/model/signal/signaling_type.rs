//! Stable signaling discriminants shared by JSON and wincode wires.

use serde_repr::{Deserialize_repr, Serialize_repr};
use strum_macros::{Display, EnumIter, FromRepr};
use utoipa_repr::ToSchema_repr;
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

    /// Host (desk server) → central brain (manager) over its regular `Server`
    /// upstream: a request to mint a temporary support code for this connection's
    /// device. The manager resolves the connection's owner-bound device, mints a
    /// short-lived code and pushes it back as [`Self::SupportCodeIssued`]. A plain signal
    /// (no central brain) never mints, so it ignores this. It replaces the former
    /// dedicated `Support` upstream whose registration itself triggered the mint.
    #[wincode(tag = 209)]
    RequestSupportCode = 209,

    /// Manager → host (desk server) over its regular `Server` upstream: the
    /// temporary support code the manager issued for this connection, with its
    /// expiry (see [`crate::model::support::SupportCodeIssuedData`]). The host displays it to the local
    /// user, who passes it out-of-band to a supporter. Server-originated only — a
    /// client that sends this inbound is misbehaving and the signal drops it.
    #[wincode(tag = 210)]
    SupportCodeIssued = 210,

    /// Host (desk server) → central brain (manager): revoke the temporary support
    /// code this host currently holds (the local user ended support), so it can no
    /// longer be redeemed. Carries the code to revoke (see [`crate::model::support::RevokeSupportCodeData`]);
    /// the manager verifies the requesting connection owns the code's device before
    /// revoking. A plain signal never mints, so it ignores this.
    #[wincode(tag = 211)]
    RevokeSupportCode = 211,

    /// Central brain (manager) → host (desk server): direct-close every in-flight
    /// grant session for a device that was minted at a superseded generation, after
    /// the device's dial code is regenerated. Carries the target device and the
    /// revoked generation (see [`crate::model::access_grant::RevokeAccessGrantData`]); the host closes every
    /// grant it holds whose recorded generation is `≤ revoked_generation`. A plain
    /// signal server issues no such teardown, so it ignores this.
    #[wincode(tag = 212)]
    RevokeAccessGrant = 212,

    /// Host → central durable mirror update for emergency remote-access lock.
    #[wincode(tag = 213)]
    HostRemoteAccessLockRequest = 213,

    /// Central → host acknowledgement of a committed lock-mirror update.
    #[wincode(tag = 214)]
    HostRemoteAccessLockAck = 214,

    /// Host → central request to close one browser peer after local teardown.
    #[wincode(tag = 215)]
    TerminateRemotePeerRequest = 215,

    /// Central → host delivery outcome for a peer termination request.
    #[wincode(tag = 216)]
    TerminateRemotePeerAck = 216,

    /// Host → controller notification for a streaming, blocked, or failed
    /// capture/encoder pipeline.
    #[wincode(tag = 217)]
    MediaPipelineStateChanged = 217,

    /// Controller → host request to retry a blocked or failed media pipeline
    /// using the already-negotiated codec and concrete encoder.
    #[wincode(tag = 218)]
    RetryMediaPipeline = 218,

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
    /// AI Diagnose cancellation (control end → host / manager). Sent when the
    /// operator starts over while a diagnosis is still running. Carries no
    /// payload; the message `request_id` correlates the cancelled diagnosis so
    /// pending collection, approval, and model work can be stopped and audited.
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

    /// Upstream → host: act on an execution already in flight. Carries
    /// `desk_agent_protocol::exec_lifecycle::ExecControlPayload` — either a
    /// cancel or a state query, both fenced by `execution_generation`. The host
    /// answers both with [`SignalingType::ExecStateReply`].
    #[wincode(tag = 623)]
    ExecControl = 623,

    /// Host → upstream: what the host's durable ledger says about one execution
    /// generation. Carries `desk_agent_protocol::exec_lifecycle::
    /// ExecStateReplyPayload`. The reply to both a cancel and a state query, so
    /// an upstream has one rule to implement: keep asking until the state is
    /// settled. Notification-style (`response_state = None`).
    #[wincode(tag = 624)]
    ExecStateReply = 624,

    /// Host → upstream: an execution progressed without finishing. Carries
    /// `desk_agent_protocol::exec_lifecycle::ExecLifecyclePayload` (accepted, or
    /// still running). Removes the guessing an upstream had to do from a clock
    /// while it heard nothing. Notification-style (`response_state = None`).
    #[wincode(tag = 625)]
    ExecLifecycle = 625,

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
