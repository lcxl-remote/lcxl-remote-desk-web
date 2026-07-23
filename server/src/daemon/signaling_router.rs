//! # Daemon-side signaling router
//!
//! Successor to `service::signaling::DeskSession::handle_message`. The
//! worker process used to own `DeskSession` and route every
//! `SignalingType` from there; the routing is now split two ways
//! around the daemon-held PeerConnection:
//!
//! - **Daemon-owned**: types that touch the [`RTCPeerConnection`] /
//!   SDP / ICE / `SignalingState`, and "swallow" types — daemon-emitted
//!   notifications or dead-enum variants the browser should never echo
//!   back. Handled inline (or trace-logged + dropped) on the daemon
//!   side, against [`super::pc_manager`]'s registry.
//! - **Worker-bound**: types that need the user-session WinSta0
//!   (file system, terminal, Tauri shell, screen / audio capture
//!   parameters, ...). Each one rides a typed `ServiceToWorker::*`
//!   IPC variant — there is no opaque-envelope bridge anymore.
//!
//! Batches 0–4 of the typed-IPC migration removed the transitional
//! `ServiceToWorker::SignalingMessage` / `WorkerToService::SignalingMessage`
//! variants and the `RouteOutcome::ForwardToWorker` fallback that fed
//! them. `route` now never falls back: every inbound `SignalingType`
//! is either handled inline (daemon-owned) or shipped to the worker
//! through a dedicated typed IPC variant.

use std::sync::Arc;
use std::time::Duration;

use actix_web::web;
use desk_agent_protocol::audit::{AuditEvent, AuditSink};
use desk_agent_protocol::diagnose::{
    COLLECT_CHUNK_PAYLOAD_LIMIT, CollectRequest, CollectResponse, CollectResponseError,
    DiagnoseEvent,
};
use desk_agent_protocol::edge_exec::{
    EdgeExecDisposition, EdgeExecRequestPayload, EdgeExecResultPayload,
};
use desk_agent_protocol::exec::{
    ConfirmExecData, ExecDecision, ExecEffect, ExecPlan, ExecPreview, ExecResultPayload,
    ResolveExecData,
};
use desk_agent_protocol::exec_policy::{DEFAULT_OUTPUT_BYTES, build_exact_argv_draft};

use crate::diagnose::terminal_copilot::copilot_signaling_sink;
use desk_agent_protocol::exec_lifecycle::{ExecControlAction, ExecControlPayload};
use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentError, AgentErrorKind, AgentOperation, AgentOutcome,
    AgentRequestData, AgentScope, CallerRef, CallerType, Capability, ExecutionMode, OperationInput,
    ProtocolVersion, RequestId, TargetRef,
};
use desk_ipc_protocol::message::{
    AgentRequestPayload, CloseTerminalPayload, EnablePrivateScreenPayload, ExecCancelPayload,
    ExecPlanPayload, ListTerminalRequestPayload, ManagerFileDeleteRequestPayload,
    ManagerFileListRequestPayload, ManagerRequestRefPayload, ManagerUpdateSettingsRequestPayload,
    ResizeTerminalPayload, SendDataToTerminalPayload, ServiceToWorker,
    SetVirtualDisplayModePayload, StartTerminalRequestPayload, UpdateDeskSettingsPayload,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams};
use desk_signal_facade::model::private_screen::EnablePrivateScreenData;
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{
    OfferModel, RemoteSessionPurpose, RequestRemoteModel, SignalingModel, SignalingType,
};
use desk_signal_facade::model::system_settings::RemoteSystemSettings;
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalResizeData,
};
use desk_signal_facade::model::virtual_display::ChangeDisplaySettingsPayload;
use desk_utils::error::DeskErrorCode;
use desk_virtual_display::{VirtualDisplayMode, validate_mode};
use tokio::sync::broadcast;

use crate::daemon::pc_manager::{self, PcRegistry};
use crate::daemon::virtual_display::{EnsureAttachedOutcome, VirtualDisplaySupervisor};
use crate::daemon::worker_manager::WorkerManager;
use crate::diagnose::DiagnoseOrchestrator;

/// Bound on how long the `RequestRemote` branch waits for the IDD to
/// finish bring-up before falling through to a capabilities-without-IDD
/// Init reply. `resolve_attach_with_backoff` schedules retries at
/// `[250, 500, 1000, 2000, 4000, 8000]` ms; with the driver already
/// loaded the first one or two attempts usually succeed (< 1 s) and
/// the post-attach `RefreshCapabilities` round-trip lands within
/// another second. 5 s covers the typical cold-bring-up while still
/// bounding browser-perceived dialog latency if the driver hangs.
const VIRTUAL_DISPLAY_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
use crate::error::DeskError;
use crate::host_control::HostControlHub;
use crate::model::settings::SharedSettings;

/// Whether a given `SignalingType` is owned by the daemon (handled
/// inline against the PC registry) or by the worker (forwarded over
/// IPC). Pure function — easy to unit-test exhaustively.
pub fn classify(signaling_type: SignalingType) -> RouteOwnership {
    match signaling_type {
        // ---- Daemon-owned: PC / SDP / ICE / SignalingState ----
        SignalingType::RequestRemote
        | SignalingType::Init
        | SignalingType::Offer
        | SignalingType::Answer
        | SignalingType::Canid
        | SignalingType::CloseControl
        | SignalingType::ConnectionRemoved => RouteOwnership::Daemon,

        // The daemon owns SignalingState, so the per-connection
        // accept-control flow runs daemon-side (browser → daemon →
        // host_control_hub → user → daemon updates SignalingState +
        // emits AcceptControl/DenyControl back). Worker no longer
        // sees RequireControl in daemon-worker mode.
        SignalingType::RequireControl => RouteOwnership::Daemon,

        // Daemon-emitted reply variants for the RequireControl flow.
        // The daemon emits AcceptControl / DenyControl outbound to the
        // browser from `pc_manager::handle_require_control`; browsers
        // never echo them back. If a stray inbound copy arrives the
        // daemon swallows it (worker's `DeskSession::handle_message`
        // has no arm for these and would only return
        // `UNKNOWN_SIGNALING_TYPE` — bridging would just bounce
        // confusing errors back to the browser).
        SignalingType::AcceptControl | SignalingType::DenyControl => RouteOwnership::Daemon,

        // Types that only flow *outbound* from the host
        // (worker → daemon → browser) or
        // are dead enums no client/worker handles. An inbound copy
        // is a protocol error from the browser; daemon swallows it
        // here rather than bridging — the worker would either fall
        // through to `UNKNOWN_SIGNALING_TYPE` or have no handler at
        // all.
        //
        // - `PrivateScreenStateChanged`: worker → browser only;
        //   emitted by `WorkerToService::PrivateScreenStateChanged`
        //   typed IPC.
        // - `AudioPlaybackError`: emitted from the PC's `on_track`
        //   callback; in daemon-worker mode the daemon's
        //   pc_manager does not attach an `on_track` handler so the
        //   variant is dead until that work lands. Portable mode
        //   still produces it from `service::signaling`, but that
        //   path bypasses the router entirely.
        // - `ManagerSystemStatue`: a dead-enum variant —
        //   the worker's `handle_message` has no arm and the
        //   front-end never emits it.
        // - `ReplyFromTerminal` / `TerminalStarted` / `TerminalClosed`:
        //   worker → browser only. Worker emits them via
        //   typed `WorkerToService::ReplyFromTerminal` /
        //   `TerminalStarted` / `TerminalClosed`; the browser never
        //   echoes them back. A stray inbound copy is a protocol
        //   error from the browser — daemon swallows it rather than
        //   bridging to the worker (which has no `handle_message`
        //   arm for these and would only return
        //   `UNKNOWN_SIGNALING_TYPE`).
        // - `AgentResponse`: worker → control end only. The worker
        //   emits it via typed `WorkerToService::AgentResponse`; the
        //   control end never echoes it back. A stray inbound copy is a
        //   protocol error — daemon swallows it.
        // - `DiagnoseEvent`: host → control end only (streamed). The
        //   daemon orchestrator emits it; the control end never echoes
        //   it back. A stray inbound copy is a protocol error — swallow.
        // - `ExecPreview` / `ExecResult`: host → control end only (the
        //   confirm-execution preview and result). Daemon-emitted; a stray
        //   inbound copy is a protocol error — swallow.
        SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackError
        | SignalingType::ManagerSystemStatue
        | SignalingType::ReplyFromTerminal
        | SignalingType::TerminalStarted
        | SignalingType::TerminalClosed
        | SignalingType::AgentResponse
        | SignalingType::DiagnoseEvent
        | SignalingType::TerminalCopilotEvent
        | SignalingType::TerminalCompleteResult
        | SignalingType::ExecPreview
        | SignalingType::ExecResult
        | SignalingType::ExecLifecycle
        | SignalingType::ExecStateReply => RouteOwnership::Daemon,

        // AI Diagnose request: control end → daemon. Unlike `AgentRequest`
        // (worker-bound raw capability call), the diagnose orchestrator runs
        // daemon-side (it owns the model call + redaction + streaming), so this
        // is handled inline against the daemon's orchestrator rather than
        // forwarded over IPC.
        // DiagnoseCancel is the handoff notification — handled inline against
        // the daemon's orchestrator (audit only), like `Diagnose`.
        SignalingType::Diagnose | SignalingType::DiagnoseCancel => RouteOwnership::Daemon,

        // In-terminal AI copilot: control end → daemon. Like `Diagnose`, the
        // copilot orchestrator runs daemon-side (model call + redaction +
        // streaming) in Default / DeskServer, so this is handled inline rather
        // than forwarded over IPC. `TerminalCopilotCancel` dismisses an in-flight
        // turn, handled inline like `DiagnoseCancel`.
        SignalingType::TerminalCopilotAsk | SignalingType::TerminalCopilotCancel => {
            RouteOwnership::Daemon
        }

        // In-terminal AI command completion: control end → daemon. Like the
        // copilot, the completion turn runs daemon-side (a single tool-free model
        // call + redaction) in Default / DeskServer, so it is handled inline
        // rather than forwarded over IPC.
        SignalingType::TerminalCompleteAsk => RouteOwnership::Daemon,

        // AI confirmed-execution: control end → daemon. The approval state
        // machine (classify → preview → approve/reject → dispatch) lives
        // daemon-side, so these are handled inline rather than forwarded over
        // IPC, like `Diagnose`. The worker only ever receives the sealed
        // `ServiceToWorker::ExecPlan` (a later step), never these.
        SignalingType::ConfirmExec | SignalingType::ResolveExec => RouteOwnership::Daemon,

        // Acting on a running execution needs the durable ledger and the worker
        // handle, both of which are the daemon's. The worker is told to stop a
        // command, but never asked what it knows — the ledger outlives it.
        SignalingType::ExecControl => RouteOwnership::Daemon,

        // Daemon-emitted notifications. Browsers don't send these
        // back at us, but if they did the daemon should swallow them
        // rather than relay to the worker (which has no PC to act on).
        SignalingType::DesktopSwitching | SignalingType::DesktopReady => RouteOwnership::Daemon,

        // AI audit event is emitted by this daemon toward the manager; it is
        // never received inbound here. Classify as daemon-owned so a stray
        // inbound frame is swallowed rather than forwarded to the worker.
        SignalingType::AiAuditEvent => RouteOwnership::Daemon,

        // Command-template sync is applied to the daemon's own cache (the exec
        // classifier reads it); never forwarded to the worker.
        SignalingType::CommandTemplateSync => RouteOwnership::Daemon,

        // Command-blocklist sync is applied to the daemon's own cache (the exec
        // classifier's Step 0 reads it); never forwarded to the worker.
        SignalingType::CommandBlocklistSync => RouteOwnership::Daemon,

        // Temporary-support code: manager → daemon, pushed over the host's regular
        // `Server` upstream after the manager issues a code for that connection.
        // The daemon consumes it locally (surfaces the code to the local user);
        // never forwarded to the worker. `RequestSupportCode` / `RevokeSupportCode`
        // are the reverse direction (daemon → manager, asking for / revoking a code)
        // and are never received inbound here, so a stray inbound copy is swallowed
        // daemon-side.
        SignalingType::SupportCodeIssued
        | SignalingType::RequestSupportCode
        | SignalingType::RevokeSupportCode
        | SignalingType::HostRemoteAccessLockRequest
        | SignalingType::HostRemoteAccessLockAck
        | SignalingType::TerminateRemotePeerRequest
        | SignalingType::TerminateRemotePeerAck => RouteOwnership::Daemon,

        // Grant-session revocation: manager → daemon, pushed after a dial-code
        // regeneration. The daemon direct-closes the affected grant connections
        // locally; never forwarded to the worker.
        SignalingType::RevokeAccessGrant => RouteOwnership::Daemon,

        // Remote-collect request: manager → daemon. In the thin-edge model the
        // daemon runs its read-only collectors on behalf of the central
        // orchestrator and streams a chunked CollectResponse back; handled inline
        // against the daemon's collector, never forwarded to the worker.
        // CollectResponse is daemon-emitted toward the manager and never received
        // inbound here, so a stray inbound copy is swallowed daemon-side.
        SignalingType::CollectRequest | SignalingType::CollectResponse => RouteOwnership::Daemon,

        // Fleet batch-execution: `EdgeExecRequest` is manager → daemon (the
        // daemon PEP re-validates the manager-sealed `ExecPlan` and dispatches it
        // to the worker); handled inline against the daemon's worker, never
        // forwarded as-is. `EdgeExecResult` is daemon-emitted toward the manager
        // and never received inbound here, so a stray inbound copy is swallowed
        // daemon-side.
        SignalingType::EdgeExecRequest | SignalingType::EdgeExecResult => RouteOwnership::Daemon,

        // Remote read-tool RPC (§8.3): `RemoteToolRequest` is manager → daemon
        // (the daemon runs the one server-stamped read locally); handled inline,
        // never forwarded. `RemoteToolResponse` is daemon-emitted toward the
        // manager and never received inbound here, so a stray inbound copy is
        // swallowed daemon-side.
        SignalingType::RemoteToolRequest | SignalingType::RemoteToolResponse => {
            RouteOwnership::Daemon
        }

        // Connection-list bookkeeping is daemon state too — the
        // daemon knows about every active PC, the worker only knows
        // its own per-connection encoder set.
        SignalingType::FetchConnections | SignalingType::ConnectionList => RouteOwnership::Daemon,

        // Heartbeat is a WS keepalive — not for the worker.
        SignalingType::Heartbeat => RouteOwnership::Daemon,

        // ---- Worker-bound: user-session resources ----
        // Each of these rides a dedicated typed `ServiceToWorker::*`
        // IPC variant — see `route` below for the per-type dispatch
        // helpers (batches 1–3 of the typed-IPC migration covered
        // them all; the legacy `SignalingMessage` bridge no longer
        // exists).
        //
        // `ChangeDisplaySettings` joins this list with the virtual
        // display integration: the daemon validates the request,
        // surfaces feature / state / param errors back to the
        // browser as `SignalingModel::error`, and only forwards a
        // typed `SetVirtualDisplayMode` IPC when the supervisor is
        // active in service mode.
        SignalingType::EnablePrivateScreen
        | SignalingType::UpdateDeskSettings
        | SignalingType::ManagerSystemInfo
        | SignalingType::ManagerFileList
        | SignalingType::ManagerFileDelete
        | SignalingType::StartTerminal
        | SignalingType::SendDataToTerminal
        | SignalingType::ResizeTerminal
        | SignalingType::CloseTerminal
        | SignalingType::ListTerminal
        | SignalingType::ManagerQuerySettings
        | SignalingType::ManagerUpdateSettings
        | SignalingType::ChangeDisplaySettings
        // AI agent capability request: control end → daemon → worker.
        // The daemon two-phase-parses + stamps trusted fields, then
        // ships a typed `ServiceToWorker::AgentRequest`.
        | SignalingType::AgentRequest => RouteOwnership::Worker,

        // ---- Error / Unknown ----
        // These are daemon-owned. `Error` is something the daemon emits
        // at the wire level (an earlier design bounced it through the
        // bridge after `handle_message` failed; now worker errors take
        // the typed `WorkerToService::SignalingError` path so the daemon
        // can ferry them back). `Unknown` is the serde-default catch-all
        // for unrecognised wire enum values
        // — bouncing it to the worker would only round-trip an error
        // back. Both swallow at the daemon with a trace log.
        SignalingType::Error | SignalingType::Unknown => RouteOwnership::Daemon,
    }
}

/// Whether a `SignalingType` is owned by the daemon or the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteOwnership {
    Daemon,
    Worker,
}

/// Errors the router can surface while routing.
#[derive(Debug)]
pub enum RouterError {
    /// A handler failed (PC creation, SDP exchange, ICE add, ...).
    /// Carries the upstream `DeskError` so the caller can decide
    /// between reporting back to the signaling server vs. logging.
    Handler(DeskError),
}

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouterError::Handler(e) => write!(f, "router handler failed: {e}"),
        }
    }
}

impl std::error::Error for RouterError {}

impl From<DeskError> for RouterError {
    fn from(e: DeskError) -> Self {
        RouterError::Handler(e)
    }
}

/// Context the router needs from the calling daemon. Constructed once
/// by `signaling_proxy` per WS connection lifetime and shared with
/// every [`route`] invocation on that connection.
#[derive(Clone)]
pub struct RouterContext {
    pub pc_registry: PcRegistry,
    pub outbound_tx: broadcast::Sender<String>,
    pub settings: web::Data<SharedSettings>,
    pub host_control_hub: Arc<HostControlHub>,
    /// handle_request_remote reads `worker_capabilities` from
    /// here to populate the Init reply, and handle_offer issues
    /// `ServiceToWorker::StartMedia` through it once the SDP exchange
    /// completes (so the worker knows to spin up the per-connection
    /// encoder).
    pub worker_mgr: WorkerManager,
    /// `Some(...)` only in service-daemon mode. Default / signaling
    /// / desk-server-only modes leave this `None`, so the
    /// `ChangeDisplaySettings` route always replies with
    /// `FEATURE_UNAVAILABLE` outside service mode.
    pub virtual_display: Option<Arc<VirtualDisplaySupervisor>>,
    /// `Some(...)` in modes with an in-process worker (Default / DeskServer),
    /// where the host can collect read-only evidence locally. AI diagnosis is
    /// orchestrated by the central signaling brain, so this serves the
    /// remote-collect edge path (`collect_for_remote`): the central server pushes a
    /// `CollectRequest` and this host streams the redacted evidence back. `None` in
    /// ServiceDaemon mode, where a `CollectRequest` replies with a wholesale error.
    pub diagnose_orchestrator: Option<Arc<DiagnoseOrchestrator>>,
    /// Serves a manager remote read tool call (§8.3) against the in-process device
    /// agent. Present in the same modes as `diagnose_orchestrator` (an in-process
    /// worker can read locally); `None` in ServiceDaemon, where a `RemoteToolRequest`
    /// replies with a wholesale error.
    pub remote_read: Option<Arc<crate::diagnose::remote_read::EdgeReadInvoker>>,
    /// Whether confirmed execution is available in this startup mode. `true`
    /// only where an in-process worker can execute (Default / DeskServer);
    /// `false` in ServiceDaemon mode, where `ConfirmExec` / `ResolveExec` reply
    /// with `UnsupportedCapability` (cross-process exec is a later step). Gated
    /// like `diagnose_orchestrator`.
    pub exec_supported: bool,
    /// Short-lived store of previewed-but-not-yet-resolved executions, keyed by
    /// `exec_request_id`. Always present (in-memory state); the startup-mode and
    /// execution-mode gates decide whether it is ever populated.
    pub exec_approvals: Arc<crate::daemon::exec_approval::PendingApprovalStore>,
    /// Await-based coordination for the agentic (model-initiated) exec path: bridges
    /// the inbound `ResolveExec` decision and the worker's `ExecResult` to the loop's
    /// awaiting seam (distinct from the browser-initiated `exec_approvals` flow).
    /// Always present; populated only while an agentic exec is in flight.
    pub agentic_exec: Arc<crate::daemon::agentic_exec::AgenticExecCoordinator>,
    /// Session-scoped approvals for `ExecutionMode::SessionApproved`, keyed by
    /// the control-end connection. Once a template is confirmed in this mode it
    /// is granted for the rest of that connection's session; releasing control
    /// or the connection ending revokes it. Always present (in-memory state);
    /// only populated when the active mode is `SessionApproved`.
    pub session_approvals: Arc<crate::daemon::session_approval::SessionApprovalStore>,
    /// Operator command templates synced from the manager (fleet only). The
    /// exec classifier unions these with the built-in baseline; empty on
    /// single-machine / remote-signaling links. Replaced wholesale on each
    /// `CommandTemplateSync` from the manager.
    pub command_templates: Arc<crate::daemon::command_templates::CommandTemplateCache>,
    /// Effective command blocklist synced from the manager. The exec classifier's
    /// Step 0 matches against this instead of the compiled-in floor, so an
    /// admin-disabled built-in rule is genuinely absent and custom rules apply.
    /// Seeded with the built-in floor, so it is never empty (fail-open) even
    /// before the first manager sync. Replaced (revision-gated) on each
    /// `CommandBlocklistSync`.
    pub command_blocklist: Arc<crate::daemon::command_blocklist::CommandBlocklistCache>,
    /// Audit sink for the confirmed-execution lifecycle. Single-machine uses a
    /// structured log sink; a DB-backed sink can be substituted without touching
    /// the emission sites.
    pub audit: Arc<dyn AuditSink>,
    /// In-flight diagnose orchestrator tasks keyed by `request_id`. A
    /// `DiagnoseCancel` (the control end starting over or handing off) aborts the
    /// matching run so a slow model call stops instead of streaming into a closed
    /// connection. Entries remove themselves on natural completion.
    pub diagnose_tasks: Arc<
        std::sync::Mutex<std::collections::HashMap<String, actix_web::rt::task::JoinHandle<()>>>,
    >,
    /// Manager-injected authorization for the current inbound AI frame, set per
    /// call by the proxy when a validated `AuthorizedControlPayload` arrives on
    /// the Manager link. `None` on the local / remote-signaling links, where the
    /// AI handlers fall back to local-config gating (no fleet PDP). Threaded
    /// through the context (rather than the handler signatures) so the existing
    /// `route()` / handler call sites stay untouched.
    pub inbound_authz: Option<desk_agent_protocol::authz::AuthorizationBlock>,
    /// The validated capability-ceiling stamp for the current inbound
    /// `RequestRemote`, set per call by the proxy after `gate_request_remote_frame`
    /// accepts a wrapped frame on the trusted-central link. `None` for a bare
    /// (non-central) request or a non-`RequestRemote` frame. A `Some` whose
    /// `access_ceiling` is `Some(_)` marks a redeemed-grant session (restricted);
    /// an `access_ceiling` of `None` is a central-verified owner/full session.
    /// Threaded through the context (like `inbound_authz`) so the handler
    /// signatures stay untouched.
    pub inbound_request_remote_authz:
        Option<desk_signal_facade::model::request_remote_authz::RequestRemoteAuthz>,
    /// The validated capability-ceiling stamp for the current inbound
    /// `StartTerminal`, set per call by the proxy after `gate_start_terminal_frame`
    /// accepts a wrapped frame on the trusted-central link. `None` for a bare
    /// (non-central) `StartTerminal` (owner-only relay path) or any other frame.
    /// `handle_start_terminal_inbound` consumes it to register the terminal
    /// connection's worker ceiling, record its admission, and index it under its
    /// grant — the terminal analogue of `inbound_request_remote_authz` (the terminal
    /// WS is a distinct connection that never does a `RequestRemote`).
    pub inbound_start_terminal_authz:
        Option<desk_signal_facade::model::request_remote_authz::RequestRemoteAuthz>,
    /// Per-attempt `request_id`s of fleet executions currently dispatched to the
    /// worker. When the worker replies with `WorkerToService::ExecResult` whose
    /// `request_id` is in this set, the proxy emits a `EdgeExecResult(614)`
    /// toward the manager instead of an `ExecResult(609)` toward a browser.
    /// Always present (in-memory state); only populated on the fleet exec path.
    pub edge_exec_pending: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// On-demand temporary-support lifecycle. `handle_support_code_issued_inbound`
    /// records the manager-issued code here for the local UI and arms a teardown
    /// at its expiry; the signaling proxy's support loop drives the upstream from
    /// the same handle. Node-local runtime state (one desk-server process).
    pub support_link_state: Arc<crate::daemon::support_link_state::SupportLinkState>,
    /// This host's durable record of the executions it has accepted. Every exec
    /// dispatch reserves here before the plan reaches the worker, so a redelivered
    /// frame cannot spawn a second process. Not optional: a host without a ledger
    /// would silently double-execute, so there is no "skip it if absent" path.
    pub exec_ledger: Arc<crate::daemon::exec_ledger::ExecLedger>,
    /// How many commands this host currently has running. Enforced locally on
    /// every path, because a central quota only binds work the manager dispatched
    /// and a control end can reach this host through an open-source signal server
    /// without the manager being involved at all.
    pub exec_capacity: Arc<crate::daemon::exec_capacity::ExecCapacity>,
}

/// What the ledger says should happen to an exec dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecAdmission {
    /// First sighting of this dispatch: it is reserved and must be spawned.
    Spawn,
    /// Already run to completion; answer with the stored result instead of
    /// running it again.
    Replay(String),
    /// Already accepted, but this host cannot say how it ended — still in flight,
    /// interrupted mid-spawn, or the result has aged out. Crucially **not** a
    /// "did not run": reporting it as such would license a retry of a change that
    /// may already have happened.
    AcceptedOutcomeUnknown(String),
    /// Refused without spawning, and waiting will not change that.
    Refused(String),
    /// Refused without spawning because the host is at its ceiling. Nothing ran and
    /// the condition is transient, so the caller may retry — kept apart from
    /// `Refused` because treating a busy host as a policy denial would settle a
    /// target that was never even attempted.
    AtCapacity(String),
}

/// Ask the ledger whether this plan may be spawned, reserving it if so.
///
/// Called immediately before the plan is handed to the worker on every dispatch
/// path. The reservation has to be durable *first*: recording it afterwards would
/// leave the exact window this exists to close, where a crash loses the fact that
/// a process was started and the retry starts a second one.
pub async fn admit_exec(ctx: &RouterContext, plan: &ExecPlan) -> ExecAdmission {
    use crate::daemon::exec_ledger::{Reservation, State};

    // Capacity is checked before the ledger so a refused command leaves no trace:
    // reserving first would burn the generation permanently on a dispatch that was
    // never accepted, and the caller's legitimate retry would then be read as a
    // redelivery of something that had already run.
    let limit = ctx
        .settings
        .read()
        .await
        .ai_policy
        .max_concurrent_executions as usize;
    let timeout = std::time::Duration::from_millis(u64::from(plan.timeout_ms));
    if let Err(full) = ctx
        .exec_capacity
        .try_admit(&plan.execution_generation, limit, timeout)
    {
        return ExecAdmission::AtCapacity(full.to_string());
    }

    let reservation = ctx
        .exec_ledger
        .reserve(
            &plan.exec_request_id.0,
            &plan.execution_generation,
            &plan.fingerprint,
            None,
        )
        .await;
    // Anything other than a fresh reservation means nothing new will run, so the
    // slot goes straight back rather than waiting for a report that never comes.
    if !matches!(reservation, Ok(Reservation::Granted)) {
        ctx.exec_capacity.release(&plan.execution_generation);
    }

    match reservation {
        Ok(Reservation::Granted) => ExecAdmission::Spawn,
        Ok(Reservation::FingerprintMismatch) => ExecAdmission::Refused(
            "this dispatch id was already used for a different command".to_string(),
        ),
        Ok(Reservation::Duplicate(row)) => {
            if row.state == State::SpawnFailed.as_str() {
                // The earlier attempt provably never started, so replaying its
                // recorded failure is honest and does not risk a double run.
                return ExecAdmission::Refused(
                    "this dispatch already failed to start on this host".to_string(),
                );
            }
            match row.result_json {
                Some(result) if row.state == State::Terminal.as_str() => {
                    ExecAdmission::Replay(result)
                }
                _ => ExecAdmission::AcceptedOutcomeUnknown(format!(
                    "this host already accepted this dispatch and its outcome is {}",
                    if row.state == State::Terminal.as_str() {
                        "no longer retained"
                    } else {
                        "not yet known"
                    }
                )),
            }
        }
        Err(e) => {
            // A ledger that cannot be written cannot promise "at most once", so the
            // dispatch is refused rather than run unrecorded.
            log::error!("[exec-ledger] refusing dispatch, ledger write failed: {e}");
            ExecAdmission::Refused("the host could not record this execution".to_string())
        }
    }
}

/// Fresh audit event id.
fn new_audit_event_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Baseline session-establishment / control-plane frames that every session —
/// even a fully capped grant / support session — may use. Deliberately minimal:
/// session establishment (`RequestRemote` / `Offer` / `Answer` / `Canid`), the
/// control plane (`RequireControl` / `CloseControl`), teardown
/// (`ConnectionRemoved`), `Heartbeat`, and the manager's `SupportCodeIssued`
/// host-inbound notification (display + arm TTL; triggers no privileged action).
fn is_baseline_signaling_type(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::RequestRemote
            | SignalingType::Offer
            | SignalingType::Answer
            | SignalingType::Canid
            | SignalingType::RequireControl
            | SignalingType::CloseControl
            | SignalingType::ConnectionRemoved
            | SignalingType::Heartbeat
            | SignalingType::SupportCodeIssued
    )
}

/// Connection-scoped capability frames whose enforcement is a per-dimension
/// `access_ceiling` gate: the terminal I/O family, the file-browse family, and
/// private-screen enable. These are only ever legitimate **after** the connection's
/// admission has been recorded (owner → `OwnerFull`, redeemed grant → `Capped`) and
/// its worker-side ceiling provisioned. An un-admitted connection sending one is
/// anomalous: no ceiling has reached the worker yet, so the worker-side `meet` gate
/// would fall back to the host global (fail-open). door1 therefore denies these for
/// an un-admitted connection.
///
/// `StartTerminal` is deliberately **excluded**: like `RequestRemote`, it is the
/// admission-*establishing* frame for the terminal WS (a distinct connection that
/// never does a `RequestRemote`). Its own source-gate (`gate_start_terminal_frame`)
/// requires and validates a capability stamp on the trusted-central link, and
/// `handle_start_terminal_inbound` records the admission + ceiling from that stamp —
/// so it must be allowed to reach the handler on an un-admitted connection, exactly
/// as `RequestRemote` is. The remaining terminal I/O frames stay gated here and pass
/// once `StartTerminal` has established the admission.
fn is_connection_scoped_capability_frame(t: SignalingType) -> bool {
    use SignalingType::*;
    matches!(
        t,
        SendDataToTerminal
            | ResizeTerminal
            | CloseTerminal
            | ListTerminal
            | ManagerFileList
            | ManagerFileDelete
            | EnablePrivateScreen
    )
}

/// The first fail-closed door for a capability-capped session (a redeemed grant
/// or a legacy support session, both carrying an `access_ceiling`). Permits the
/// baseline frames unconditionally, plus the connection-scoped capability
/// families whose ceiling dimension is not an explicit `Some(false)` — so the
/// frame can reach its worker-side `meet(ceiling, global)` gate. Everything else
/// is denied: owner-plane frames (`Manager*` settings / system-info, display,
/// AI / exec / remote-tool) have **no** worker-side meet gate, so door1 is their
/// only enforcement point against a capped session; and any unknown / future
/// signaling type falls through the `_ => false` arm (deliberate fail-closed —
/// this is not the `handle_message` exhaustiveness rule).
fn capped_session_permits(t: SignalingType, ceiling: &SecuritySettings) -> bool {
    use SignalingType::*;
    if is_baseline_signaling_type(t) {
        return true;
    }
    match t {
        // Terminal family — the whole terminal UI including enumeration.
        StartTerminal | SendDataToTerminal | ResizeTerminal | CloseTerminal | ListTerminal => {
            ceiling.allow_terminal != Some(false)
        }
        ManagerFileList => ceiling.allow_file_browse != Some(false),
        ManagerFileDelete => {
            ceiling.allow_file_browse != Some(false) && ceiling.allow_file_delete != Some(false)
        }
        EnablePrivateScreen => ceiling.allow_private_screen != Some(false),
        _ => false,
    }
}

/// Classification of a `from_connection_id` for the door1 gate, derived from its
/// **admission record** (kept for the whole signaling connection, independent of
/// the PC lifecycle) rather than the PC's live state — so a capped connection
/// that dropped its PC via `CloseControl` is still classified as capped.
enum ConnectionGate {
    /// Admitted as a full owner session — no capability ceiling.
    KnownOwnerFull,
    /// Admitted as a redeemed-grant / legacy-support session, capped by a ceiling.
    KnownCapped(SecuritySettings),
    /// A server-internal frame has no originating WebSocket connection id.
    /// Its producing service already performed the applicable authorization.
    /// File-manager operations are never server-internal because their REST
    /// entry points were removed; they require an admitted controller connection.
    /// Client frames always receive a server-stamped id, so a client cannot
    /// manufacture this classification.
    ServerInternal,
    /// A WS connection carrying a real stamped id but no admission record: it
    /// never did an authorized `RequestRemote` on this instance (a management-only
    /// connection, or a session before its `RequestRemote`), or the id is spoofed.
    /// door1 is fail-closed for connection-scoped capability frames here (see
    /// [`door1_permits`]) — a capped session that has not yet been admitted must
    /// not slip a capability frame through the pre-admission window where the
    /// worker has no ceiling and would fall back to the host global.
    UnadmittedConnection,
}

/// Classify a `from_connection_id` for the door1 gate from the registry's
/// admission map. The server stamps `from_connection_id` authoritatively
/// (`ConnectionState::send_to_peer`), so this cannot be spoofed by the client.
async fn classify_connection(registry: &PcRegistry, connection_id: Option<&str>) -> ConnectionGate {
    let Some(cid) = connection_id else {
        return ConnectionGate::ServerInternal;
    };
    match registry.admission(cid).await {
        Some(pc_manager::Admission::OwnerFull) => ConnectionGate::KnownOwnerFull,
        Some(pc_manager::Admission::Capped(c)) => ConnectionGate::KnownCapped(c),
        None => ConnectionGate::UnadmittedConnection,
    }
}

/// The door1 decision for an inbound frame. A session admitted as owner passes
/// everything (route() drops non-inbound types anyway); a capped session (a
/// redeemed grant carrying an `access_ceiling`, including a temporary-support
/// session) runs the capability matrix (still capped after a `CloseControl` PC
/// teardown, since the admission outlives the PC).
///
/// A server-internal frame passes because its producing service already ran
/// the applicable authorization checks, except file-manager frames, which must
/// carry an admitted controller connection. An un-admitted WebSocket connection
/// is fail-closed for connection-scoped capability frames: those frames are
/// only legitimate after admission provisions the worker ceiling. Otherwise
/// the worker would evaluate the request against the host global setting in
/// the pre-admission window. Owner-plane management frames are authorized by
/// the central service, while AI and exec frames keep their dedicated
/// authorization gates.
fn door1_permits(gate: &ConnectionGate, t: SignalingType) -> bool {
    match gate {
        ConnectionGate::KnownOwnerFull => true,
        ConnectionGate::KnownCapped(ceiling) => capped_session_permits(t, ceiling),
        ConnectionGate::ServerInternal => !matches!(
            t,
            SignalingType::ManagerFileList | SignalingType::ManagerFileDelete
        ),
        ConnectionGate::UnadmittedConnection => !is_connection_scoped_capability_frame(t),
    }
}

/// RFC3339 timestamp for an audit event.
fn audit_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Revoke every session-scoped exec approval held by the connection that sent
/// `model`. Called when the connection releases control (`CloseControl`) or ends
/// (`ConnectionRemoved`); a no-op when the connection had no grants.
fn revoke_session_approvals(ctx: &RouterContext, model: &SignalingModel) {
    if let Some(conn) = model.from_connection_id.as_deref() {
        let revoked = ctx.session_approvals.revoke_connection(conn);
        if revoked > 0 {
            log::debug!("[router] revoked {revoked} session exec approval(s) for {conn}");
        }
    }
}

/// snake_case risk label for the audit `risk` column.
fn risk_str(risk: desk_agent_protocol::RiskLevel) -> &'static str {
    use desk_agent_protocol::RiskLevel::*;
    match risk {
        Low => "low",
        Medium => "medium",
        High => "high",
        Critical => "critical",
        Blocked => "blocked",
    }
}

/// Route a signaling message.
///
/// Each `SignalingType` is exhaustively dispatched: PC / SDP / ICE /
/// SignalingState types run inline against `ctx.pc_registry`;
/// worker-bound types (terminal, manager queries, EnablePrivateScreen,
/// UpdateDeskSettings) are shipped to the worker via dedicated
/// `ServiceToWorker::*` typed IPC variants; daemon-emitted notifications
/// and dead-enum variants (`Answer`, `Init`, `Heartbeat`, `Error`,
/// `Unknown`, ...) are trace-logged + dropped. There is no fallback
/// path — the typed-IPC migration removed the `SignalingMessage` bridge.
async fn promote_desktop_resources(
    model: &SignalingModel,
    ctx: &RouterContext,
    reason: &str,
) -> Result<(), RouterError> {
    let connection_id = model
        .check_and_get_from_connection_id()
        .map_err(DeskError::from)?;
    if let Some(pc) = ctx.pc_registry.get(connection_id).await {
        let pc = pc.read().await;
        let mut state = pc.signaling_state.write().await;
        if state.purpose == RemoteSessionPurpose::FileManager {
            state.purpose = RemoteSessionPurpose::RemoteDesktop;
            log::info!("[router] promoted {connection_id} to remote_desktop for {reason}");
        }
    }

    let virtual_display_enabled = ctx.settings.read().await.virtual_display.enabled;
    if virtual_display_enabled && let Some(supervisor) = ctx.virtual_display.as_ref() {
        match supervisor
            .ensure_attached(VIRTUAL_DISPLAY_ATTACH_TIMEOUT)
            .await
        {
            EnsureAttachedOutcome::Attached => {}
            EnsureAttachedOutcome::TimedOut => {
                log::warn!("[router] virtual display attach timed out during {reason}");
            }
            EnsureAttachedOutcome::Unavailable(e) => {
                log::warn!("[router] virtual display unavailable during {reason}: {e}");
            }
        }
    }
    Ok(())
}

pub async fn route(model: &SignalingModel, ctx: &RouterContext) -> Result<(), RouterError> {
    if let Some(connection_id) = model.from_connection_id.as_deref()
        && ctx.pc_registry.is_tombstoned(connection_id).await
        && !allowed_for_tombstoned_connection(model.signaling_type)
    {
        log::warn!(
            "[router] host-terminated connection {connection_id}: rejecting {:?} frame",
            model.signaling_type
        );
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::INVALID_STATE,
            "Connection was terminated by the host",
        );
        return Ok(());
    }

    if ctx.host_control_hub.remote_access_gate().is_locked()
        && !allowed_while_remote_access_locked(model.signaling_type)
    {
        log::warn!(
            "[router] remote access is locked: rejecting {:?} frame",
            model.signaling_type
        );
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::REMOTE_ACCESS_LOCKED,
            "Remote access is locked on the host",
        );
        return Ok(());
    }

    // First fail-closed door: capability-capped sessions (a redeemed grant or a
    // legacy support session — both carry an `access_ceiling`) may only use the
    // baseline session/control frames plus the connection-scoped capability frames
    // their ceiling permits. The owner-plane frames (`Manager*` settings / system
    // info, display, AI / exec / remote-tool) — which have **no** worker-side meet
    // gate — plus any unknown / future signaling type are denied here; this is the
    // only enforcement point protecting them from a capped session. A full owner
    // session (registered PC, no ceiling) passes unchanged. An unknown / spoofed /
    // not-yet-registered connection id is never treated as owner (fail-closed); a
    // support-upstream frame with no PC yet falls back to the fixed support
    // ceiling. The data-channel path is gated independently by `route_is_permitted`
    // (the second door) and the worker-side `meet` gates.
    let gate = classify_connection(&ctx.pc_registry, model.from_connection_id.as_deref()).await;
    if !door1_permits(&gate, model.signaling_type) {
        log::warn!(
            "[router] capability-restricted session: rejecting {:?} frame",
            model.signaling_type
        );
        return Ok(());
    }
    match model.signaling_type {
        SignalingType::RequestRemote => {
            let request_remote = model
                .get_data::<RequestRemoteModel>()
                .map_err(DeskError::from)?;
            // Hold a pending guard for the lifetime
            // of this handler so cleanup_pc on a concurrently-closing
            // old PC cannot N→0 detach the IDD out from under us.
            let pending_guard = ctx.pc_registry.enter_pending();

            let s = ctx.settings.read().await.clone();
            // Block on virtual display attach BEFORE assembling the
            // Init reply so the daemon's capabilities cache reflects
            // the IDD and the dropdown shows it on the first dialog
            // open. Timeout falls through to a capabilities-without-IDD
            // reply; the next dialog open recovers via the existing
            // RefreshCapabilities round-trip if attach eventually
            // completes in the background.
            if let Some(supervisor) = ctx.virtual_display.as_ref()
                && s.virtual_display.enabled
                && request_remote.purpose == RemoteSessionPurpose::RemoteDesktop
            {
                match supervisor
                    .ensure_attached(VIRTUAL_DISPLAY_ATTACH_TIMEOUT)
                    .await
                {
                    EnsureAttachedOutcome::Attached => {}
                    EnsureAttachedOutcome::TimedOut => {
                        log::warn!(
                            "[router] virtual display attach did not complete within \
                             {VIRTUAL_DISPLAY_ATTACH_TIMEOUT:?}; Init reply will omit \
                             IDD this round"
                        );
                    }
                    EnsureAttachedOutcome::Unavailable(e) => {
                        log::warn!(
                            "[router] virtual display provider unavailable: {e}; \
                             continuing without IDD"
                        );
                    }
                }
            }

            let user_name = "worker_node".to_string();
            let has_tauri = ctx.host_control_hub.has_tauri_ui();
            let capabilities = ctx.worker_mgr.worker_capabilities();
            // Resolve the connection's capability ceiling. A redeemed-grant stamp
            // carries one directly (an owner stamp carries `None` = no ceiling); a
            // bare (non-central) request carries none. A temporary-support session
            // now arrives as an ordinary redeemed grant (its ceiling comes from the
            // device's per-code capabilities), so there is no separate support path.
            let (access_ceiling, grant_session_id, grant_generation) =
                match ctx.inbound_request_remote_authz.as_ref() {
                    Some(a) => (
                        a.access_ceiling.clone(),
                        a.grant_session_id.clone(),
                        a.generation,
                    ),
                    None => (None, None, 0),
                };
            ctx.host_control_hub.host_activity().ensure_session(
                model.check_and_get_from_connection_id().map_err(DeskError::from)?,
                ctx.inbound_request_remote_authz
                    .as_ref()
                    .map(|authz| authz.actor.clone())
                    .unwrap_or_else(
                        desk_signal_facade::model::request_remote_authz::ActorSummary::unknown,
                    ),
            );
            let result = pc_manager::handle_request_remote(
                &ctx.pc_registry,
                &ctx.outbound_tx,
                &s,
                &user_name,
                has_tauri,
                capabilities.as_ref(),
                Some(&ctx.worker_mgr),
                ctx.virtual_display.as_ref(),
                model,
                access_ceiling,
                grant_session_id,
                grant_generation,
            )
            .await;

            // Release the guard before the post-handler cleanup check so
            // pending_requests reflects the actual outstanding work.
            drop(pending_guard);

            // Round 3 #12 / codex post-ensure cleanup: if handle_request_remote
            // failed to register a PC (parse error, registry collision,
            // build_peer_connection failure, ...) the supervisor would
            // remain Attached with no PC ever holding it, and no cleanup
            // path would later trigger detach. Re-check N→0 conditions
            // here and detach if appropriate.
            if let Some(supervisor) = ctx.virtual_display.as_ref()
                && ctx.pc_registry.len().await == 0
                && ctx.pc_registry.pending_requests() == 0
            {
                log::info!(
                    "[router] post-RequestRemote cleanup: registry empty and no pending; \
                     detaching virtual display"
                );
                if let Err(e) = supervisor.apply(false).await {
                    log::warn!("[router] post-RequestRemote cleanup detach failed: {e}");
                }
            }

            result?;
            Ok(())
        }
        SignalingType::Offer => {
            let offer = model.get_data::<OfferModel>().map_err(DeskError::from)?;
            if offer.offer.sdp.contains("m=video") {
                promote_desktop_resources(model, ctx, "video offer").await?;
            }
            pc_manager::handle_offer(&ctx.pc_registry, &ctx.outbound_tx, &ctx.worker_mgr, model)
                .await?;
            Ok(())
        }
        SignalingType::Canid => {
            pc_manager::handle_canid(&ctx.pc_registry, model).await?;
            Ok(())
        }
        SignalingType::CloseControl => {
            // Releasing control revokes any session-scoped exec approvals the
            // connection accrued in SessionApproved mode.
            revoke_session_approvals(ctx, model);
            pc_manager::handle_close_control(
                &ctx.pc_registry,
                &ctx.worker_mgr,
                ctx.virtual_display.as_ref(),
                model,
            )
            .await?;
            Ok(())
        }
        SignalingType::ConnectionRemoved => {
            // The connection ending revokes its session-scoped exec approvals.
            revoke_session_approvals(ctx, model);
            pc_manager::handle_connection_removed(
                &ctx.pc_registry,
                &ctx.worker_mgr,
                ctx.virtual_display.as_ref(),
                model,
            )
            .await?;
            Ok(())
        }
        SignalingType::RequireControl => {
            promote_desktop_resources(model, ctx, "control request").await?;
            let settings: &SharedSettings = &ctx.settings;
            let outcome = pc_manager::handle_require_control(
                &ctx.pc_registry,
                &ctx.outbound_tx,
                settings,
                &ctx.host_control_hub,
                model,
            )
            .await?;
            ctx.host_control_hub
                .host_activity()
                .set_remote_control(&outcome.connection_id, outcome.accept_control);
            update_exclusive_after_control_change(ctx, &outcome).await;
            Ok(())
        }
        // Daemon-emitted or dead inbound; the browser should never
        // send these at us but if it does, swallow rather than
        // routing onward. See classify() doc-comments for per-variant
        // rationale. `Error` and `Unknown` are in this group too (they
        // used to be worker-bound for verbose logging, but since the
        // bridge is gone there is no point round-tripping them).
        SignalingType::Answer
        | SignalingType::Init
        | SignalingType::AcceptControl
        | SignalingType::DenyControl
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::AudioPlaybackError
        | SignalingType::ManagerSystemStatue
        | SignalingType::ReplyFromTerminal
        | SignalingType::TerminalStarted
        | SignalingType::TerminalClosed
        | SignalingType::DesktopSwitching
        | SignalingType::DesktopReady
        | SignalingType::FetchConnections
        | SignalingType::ConnectionList
        | SignalingType::Heartbeat
        // AgentResponse only flows worker → control end; an inbound
        // copy is a protocol error — swallow it.
        | SignalingType::AgentResponse
        // DiagnoseEvent only flows host → control end (streamed); an
        // inbound copy is a protocol error — swallow it.
        | SignalingType::DiagnoseEvent
        // TerminalCopilotEvent only flows host → control end (streamed); an
        // inbound copy is a protocol error — swallow it.
        | SignalingType::TerminalCopilotEvent
        // TerminalCompleteResult only flows host → control end; an inbound copy
        // is a protocol error — swallow it.
        | SignalingType::TerminalCompleteResult
        // ExecPreview / ExecResult and the lifecycle frames only flow host →
        // control end; an inbound copy is a protocol error — swallow it.
        | SignalingType::ExecPreview
        | SignalingType::ExecResult
        | SignalingType::ExecLifecycle
        | SignalingType::ExecStateReply
        | SignalingType::HostRemoteAccessLockRequest
        | SignalingType::HostRemoteAccessLockAck
        | SignalingType::TerminateRemotePeerRequest
        | SignalingType::TerminateRemotePeerAck
        | SignalingType::Error
        | SignalingType::Unknown => {
            log::trace!(
                "[router] daemon-emitted or unknown variant arrived inbound, dropping: {:?}",
                model.signaling_type,
            );
            Ok(())
        }
        SignalingType::EnablePrivateScreen => {
            handle_enable_private_screen_inbound(ctx, model).await
        }
        SignalingType::UpdateDeskSettings => handle_update_desk_settings_inbound(ctx, model).await,
        // Manager-plane typed-IPC dispatch.
        SignalingType::ManagerSystemInfo => handle_manager_system_info_inbound(ctx, model).await,
        SignalingType::ManagerQuerySettings => {
            handle_manager_query_settings_inbound(ctx, model).await
        }
        SignalingType::ManagerFileList => handle_manager_file_list_inbound(ctx, model).await,
        SignalingType::ManagerFileDelete => handle_manager_file_delete_inbound(ctx, model).await,
        SignalingType::ManagerUpdateSettings => {
            handle_manager_update_settings_inbound(ctx, model).await
        }
        // Terminal-plane typed-IPC dispatch.
        SignalingType::StartTerminal => handle_start_terminal_inbound(ctx, model).await,
        SignalingType::SendDataToTerminal => handle_send_data_to_terminal_inbound(ctx, model).await,
        SignalingType::ResizeTerminal => handle_resize_terminal_inbound(ctx, model).await,
        SignalingType::CloseTerminal => handle_close_terminal_inbound(ctx, model).await,
        SignalingType::ListTerminal => handle_list_terminal_inbound(ctx, model).await,
        // Virtual display integration: browser → daemon ChangeDisplaySettings.
        // Daemon validates input, surfaces error responses for the
        // un-routable cases (FEATURE_UNAVAILABLE / INVALID_PARAMS /
        // REMOTE_DESK_OFFLINE / INVALID_STATE), and forwards a typed
        // SetVirtualDisplayMode IPC only when the supervisor is active.
        SignalingType::ChangeDisplaySettings => {
            handle_change_display_settings_inbound(ctx, model).await
        }
        // AI agent capability request: two-phase parse + trusted-field
        // stamp, then ship a typed `ServiceToWorker::AgentRequest`.
        SignalingType::AgentRequest => handle_agent_request_inbound(ctx, model).await,
        // AI Diagnose: run the daemon-side orchestrator (Default / DeskServer)
        // or reply `FEATURE_UNAVAILABLE` (ServiceDaemon, where the orchestrator
        // is not injected). Streams `DiagnoseEvent` frames back to the browser.
        SignalingType::Diagnose => handle_diagnose_inbound(ctx, model).await,
        // AI Diagnose handoff ("转人工"): a UI-side action with no orchestrator
        // state branch. The daemon only records an `ai.task.cancelled` audit so
        // the handoff is auditable; no `DiagnoseEvent` is streamed back.
        SignalingType::DiagnoseCancel => handle_diagnose_cancel_inbound(ctx, model).await,
        // In-terminal AI copilot: run the daemon-side orchestrator (Default /
        // DeskServer) or reply `FEATURE_UNAVAILABLE` (ServiceDaemon, where the
        // orchestrator is not injected). Streams `TerminalCopilotEvent` frames
        // back to the control end.
        SignalingType::TerminalCopilotAsk => handle_terminal_copilot_inbound(ctx, model).await,
        // Copilot dismissal: a UI-side action with no orchestrator state branch
        // yet; recorded as a no-op cancellation, like `DiagnoseCancel`.
        SignalingType::TerminalCopilotCancel => Ok(()),
        // In-terminal AI command completion: run the daemon-side single-shot
        // completion (Default / DeskServer) or reply with an error result
        // (ServiceDaemon, where the runtime is not injected). Answers with one
        // `TerminalCompleteResult` frame back to the control end.
        SignalingType::TerminalCompleteAsk => handle_terminal_complete_inbound(ctx, model).await,
        // AI confirmed-execution: classify the command, store an immutable
        // pending approval, and stream an `ExecPreview` back (Default /
        // DeskServer) or reply `UnsupportedCapability` (ServiceDaemon).
        SignalingType::ConfirmExec => handle_confirm_exec_inbound(ctx, model).await,
        // AI confirmed-execution: consume a pending approval and (on approve)
        // dispatch the sealed plan. The execution itself + outbound
        // `ExecResult` land with the worker executor in a later step.
        SignalingType::ResolveExec => handle_resolve_exec_inbound(ctx, model).await,
        SignalingType::ExecControl => handle_exec_control_inbound(ctx, model).await,
        // AI audit events are emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never persists audit itself).
        SignalingType::AiAuditEvent => Ok(()),
        // Command-template sync from the manager. The source gate
        // (`handle_inbound_signaling_text`) has already dropped any non-Manager
        // origin before reaching here; this only applies the validated set.
        SignalingType::CommandTemplateSync => handle_command_template_sync_inbound(ctx, model),
        // Command-blocklist sync from the manager. The source gate
        // (`handle_inbound_signaling_text`) has already dropped any non-central
        // origin before reaching here; this only applies the validated set.
        SignalingType::CommandBlocklistSync => handle_command_blocklist_sync_inbound(ctx, model),
        // Temporary-support code issued by the manager over this host's dedicated
        // Support upstream. The daemon surfaces it to the local user, who reads it
        // out to a supporter. The source gate (`handle_inbound_signaling_text`) has
        // already dropped any non-central origin before reaching here.
        SignalingType::SupportCodeIssued => handle_support_code_issued_inbound(ctx, model),
        // RequestSupportCode / RevokeSupportCode are emitted by this daemon toward
        // the manager (asking for / revoking a code); a stray inbound copy is
        // swallowed (the daemon never consumes its own request).
        SignalingType::RequestSupportCode | SignalingType::RevokeSupportCode => Ok(()),
        // Grant-session teardown from the manager after a dial-code regeneration.
        // The source gate (`is_trusted_central_only`) has already dropped any
        // non-central origin before reaching here; the daemon direct-closes every
        // grant it holds at a superseded generation.
        SignalingType::RevokeAccessGrant => handle_revoke_access_grant_inbound(ctx, model).await,
        // Remote-collect request from the manager: run the daemon-side read-only
        // collectors and stream a chunked CollectResponse back. The source gate
        // (`handle_inbound_signaling_text`) has already dropped any non-Manager
        // origin before reaching here.
        SignalingType::CollectRequest => handle_collect_request_inbound(ctx, model).await,
        // CollectResponse is emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never consumes its own stream).
        SignalingType::CollectResponse => Ok(()),
        // Fleet batch-execution request from the manager: PEP re-validate the
        // manager-sealed `ExecPlan` and dispatch it to the worker, correlating
        // the worker's result back to the manager as a `EdgeExecResult`. The
        // source gate + dedicated authz gate (`signaling_proxy`) have already
        // dropped non-Manager origins and unwrapped/validated the authorization.
        SignalingType::EdgeExecRequest => handle_edge_exec_request_inbound(ctx, model).await,
        // EdgeExecResult is emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never consumes its own replies).
        SignalingType::EdgeExecResult => Ok(()),
        // Remote read-tool request from the manager (agentic loop running
        // centrally): run the one server-stamped read locally and stream a chunked
        // RemoteToolResponse back. The source gate has already dropped any
        // non-Manager origin before reaching here.
        SignalingType::RemoteToolRequest => handle_remote_tool_request_inbound(ctx, model).await,
        // RemoteToolResponse is emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never consumes its own stream).
        SignalingType::RemoteToolResponse => Ok(()),
    }
}

fn allowed_for_tombstoned_connection(signaling_type: SignalingType) -> bool {
    matches!(
        signaling_type,
        SignalingType::CloseControl
            | SignalingType::ConnectionRemoved
            | SignalingType::CloseTerminal
    )
}

fn allowed_while_remote_access_locked(signaling_type: SignalingType) -> bool {
    matches!(
        signaling_type,
        SignalingType::CloseControl
            | SignalingType::ConnectionRemoved
            | SignalingType::CloseTerminal
            | SignalingType::CommandTemplateSync
            | SignalingType::CommandBlocklistSync
            | SignalingType::SupportCodeIssued
            | SignalingType::RevokeAccessGrant
            | SignalingType::HostRemoteAccessLockAck
            | SignalingType::TerminateRemotePeerAck
    )
}

/// Handle an inbound remote-collect request from the manager. Runs the
/// daemon's read-only collectors over the policy-gated capability set, refits
/// any screenshot into a model-ready data URL, redacts text evidence, and
/// streams the resulting [`EvidenceSnapshot`](desk_agent_protocol::evidence::EvidenceSnapshot)
/// back to the manager as a chunked `CollectResponse`. Always replies (a chunk
/// stream or an error frame) so the manager's pending entry never hangs.
async fn handle_collect_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request: CollectRequest = match model.get_data::<CollectRequest>() {
        Ok(r) => r,
        Err(e) => {
            // No request_id to correlate; log and drop (the manager times out).
            log::warn!("[router] dropping malformed CollectRequest: {e}");
            return Ok(());
        }
    };
    let request_id = request.request_id.clone();

    // The collector is only injected where an in-process worker can collect
    // (Default / DeskServer). Without it the edge cannot serve a remote
    // collection — report a wholesale error.
    let Some(orchestrator) = ctx.diagnose_orchestrator.clone() else {
        send_collect_error(
            &ctx.outbound_tx,
            &request_id,
            AgentErrorKind::SessionUnavailable,
            "evidence collector is not available on this host",
        );
        return Ok(());
    };

    match orchestrator
        .collect_for_remote(&request_id, &request.request)
        .await
    {
        Ok(snapshot) => {
            match desk_diagnose_core::chunk::chunk_snapshot(
                &request_id,
                &snapshot,
                COLLECT_CHUNK_PAYLOAD_LIMIT,
            ) {
                Ok(chunks) => {
                    for chunk in chunks {
                        send_collect_response(&ctx.outbound_tx, &CollectResponse::Chunk(chunk));
                    }
                }
                Err(e) => {
                    send_collect_error(
                        &ctx.outbound_tx,
                        &request_id,
                        AgentErrorKind::Internal,
                        &format!("failed to encode evidence snapshot: {e}"),
                    );
                }
            }
        }
        Err(e) => {
            // Preserve the failure class (notably a fail-closed `RedactionFailed`)
            // so the central orchestrator audits it correctly.
            send_collect_error(&ctx.outbound_tx, &request_id, e.kind, &e.message);
        }
    }
    Ok(())
}

/// Serialize and emit a [`CollectResponse`] frame toward the manager over the
/// outbound lane. Mirrors the audit-event emit path: a server-initiated
/// `new_request` (its signaling `request_id` is unused — correlation rides the
/// payload's `request_id`), consumed only by the manager's collect observer.
fn send_collect_response(outbound_tx: &broadcast::Sender<String>, response: &CollectResponse) {
    match SignalingModel::new_request(SignalingType::CollectResponse, None, Some(response)) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => log::warn!("[collect] failed to serialize CollectResponse: {e}"),
        },
        Err(e) => log::warn!("[collect] failed to build CollectResponse model: {e}"),
    }
}

/// Emit a wholesale [`CollectResponse::Error`] for `request_id`, tagged with the
/// structured failure `kind` so the central orchestrator can audit it.
fn send_collect_error(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    kind: AgentErrorKind,
    reason: &str,
) {
    send_collect_response(
        outbound_tx,
        &CollectResponse::Error(CollectResponseError {
            request_id: request_id.to_string(),
            error_kind: kind,
            reason: reason.to_string(),
        }),
    );
}

/// Handle an inbound remote read-tool request from the manager (§8.3). Runs the
/// one server-stamped capability call against the in-process device agent (which
/// enforces the envelope's gate), redacts the result fail-closed, and streams it
/// back as a chunked `RemoteToolResponse`. Always replies (a chunk stream or an
/// error frame) so the manager's pending entry never hangs.
async fn handle_remote_tool_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::remote_tool::{
        REMOTE_TOOL_CHUNK_PAYLOAD_LIMIT, RemoteToolRequest, RemoteToolResponse,
    };
    let request: RemoteToolRequest = match model.get_data::<RemoteToolRequest>() {
        Ok(r) => r,
        Err(e) => {
            // No request_id to correlate; log and drop (the manager times out).
            log::warn!("[router] dropping malformed RemoteToolRequest: {e}");
            return Ok(());
        }
    };
    let request_id = request.request_id.clone();

    // The read invoker is only injected where an in-process worker can read
    // (Default / DeskServer). Without it the edge cannot serve a remote read.
    let Some(invoker) = ctx.remote_read.clone() else {
        send_remote_tool_error(
            &ctx.outbound_tx,
            &request_id,
            AgentErrorKind::SessionUnavailable,
            "remote read is not available on this host",
        );
        return Ok(());
    };

    match invoker.invoke_redacted(request.envelope).await {
        Ok(outcome) => match serde_json::to_vec(&outcome) {
            Ok(bytes) => {
                for chunk in desk_diagnose_core::chunk::chunk_bytes(
                    &request_id,
                    &bytes,
                    REMOTE_TOOL_CHUNK_PAYLOAD_LIMIT,
                ) {
                    send_remote_tool_response(&ctx.outbound_tx, &RemoteToolResponse::Chunk(chunk));
                }
            }
            Err(e) => {
                send_remote_tool_error(
                    &ctx.outbound_tx,
                    &request_id,
                    AgentErrorKind::Internal,
                    &format!("failed to encode remote tool result: {e}"),
                );
            }
        },
        Err(e) => {
            // Preserve the failure class (notably a fail-closed `RedactionFailed`
            // or a gate `PermissionDenied`) so the central loop reports it safely.
            send_remote_tool_error(&ctx.outbound_tx, &request_id, e.kind, &e.message);
        }
    }
    Ok(())
}

/// Serialize and emit a [`RemoteToolResponse`](desk_agent_protocol::remote_tool::RemoteToolResponse)
/// frame toward the manager over the outbound lane (correlation rides the
/// payload's `request_id`, consumed only by the manager's remote-tool observer).
fn send_remote_tool_response(
    outbound_tx: &broadcast::Sender<String>,
    response: &desk_agent_protocol::remote_tool::RemoteToolResponse,
) {
    match SignalingModel::new_request(SignalingType::RemoteToolResponse, None, Some(response)) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => log::warn!("[rtool] failed to serialize RemoteToolResponse: {e}"),
        },
        Err(e) => log::warn!("[rtool] failed to build RemoteToolResponse model: {e}"),
    }
}

/// Emit a wholesale [`RemoteToolResponse::Error`](desk_agent_protocol::remote_tool::RemoteToolResponse)
/// for `request_id`, tagged with the model-safe failure so the central loop turns
/// it into an error tool-result.
fn send_remote_tool_error(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    kind: AgentErrorKind,
    reason: &str,
) {
    use desk_agent_protocol::remote_tool::{RemoteToolResponse, RemoteToolResponseError};
    send_remote_tool_response(
        outbound_tx,
        &RemoteToolResponse::Error(RemoteToolResponseError {
            request_id: request_id.to_string(),
            error: AgentError {
                kind,
                message: reason.to_string(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            },
        }),
    );
}

/// Apply an inbound `CommandTemplateSync` from the manager: parse the payload,
/// reject an unknown wire version, and replace the operator-template cache
/// (entries are shape-validated, fail-closed, inside `replace`). The exec
/// classifier picks up the new set on the next `ConfirmExec`.
fn handle_command_template_sync_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::command_template::{
        COMMAND_TEMPLATE_SYNC_VERSION, CommandTemplateSyncPayload,
        MIN_COMMAND_TEMPLATE_SYNC_VERSION,
    };
    let payload = match model.get_data::<CommandTemplateSyncPayload>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[router] bad CommandTemplateSync payload: {e}");
            return Ok(());
        }
    };
    // Accept any version in the supported range; a version outside it (e.g. a
    // future version reaching an older daemon) is safely ignored. The set-narrowing
    // wire epoch — not the payload version — is what guards against a stale
    // pre-narrowing sender: `replace` rejects any frame below the current epoch
    // floor, so a payload that predates set narrowing (epoch 0) never widens the
    // cache regardless of its version.
    if !(MIN_COMMAND_TEMPLATE_SYNC_VERSION..=COMMAND_TEMPLATE_SYNC_VERSION)
        .contains(&payload.version)
    {
        log::warn!(
            "[router] ignoring CommandTemplateSync with unsupported version {}",
            payload.version
        );
        return Ok(());
    }
    let revision = payload.command_template_revision;
    let epoch = payload.epoch;
    match ctx
        .command_templates
        .replace(payload.templates, epoch, revision)
    {
        Some(accepted) => log::info!(
            "[router] applied operator command-template sync: {accepted} template(s) (epoch {epoch}, revision {revision:?})"
        ),
        None => log::info!(
            "[router] ignored stale operator command-template sync (epoch {epoch}, revision {revision:?})"
        ),
    }
    Ok(())
}

/// Apply an inbound `CommandBlocklistSync` from the manager: parse the payload,
/// reject an unknown wire version, and replace the effective-blocklist cache
/// (revision-gated, fail-closed inside `replace`). A frame with no revision is
/// dropped — the manager always stamps one, and for the blocklist a revision is
/// required to enforce monotonic ordering. The exec classifier's Step 0 picks up
/// the new set on the next classification.
/// Surface a manager-issued temporary support code to the local user and arm the
/// session's expiry teardown.
///
/// The code arrives over the host's dedicated Support upstream (the source gate
/// has already dropped any non-central origin). The daemon records it in the
/// support link state for the local UI and spawns a timer that ends the session
/// at the code's expiry — guarded by the session epoch so a stale timer from an
/// earlier session cannot tear down a newer one. The signaling proxy's support
/// loop performs the actual upstream / PC teardown when the state flips inactive.
fn handle_support_code_issued_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_signal_facade::model::support::SupportCodeIssuedData;
    let payload = match model.get_data::<SupportCodeIssuedData>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[support] bad SupportCodeIssued payload: {e}");
            return Ok(());
        }
    };
    log::info!(
        "[support] manager issued temporary support code (expires_at={})",
        payload.expires_at
    );
    let state = ctx.support_link_state.clone();
    let expires_at = payload.expires_at;
    let code = payload.code;
    let armed_epoch = state.epoch();
    actix_web::rt::spawn(async move {
        state.set_snapshot(code, expires_at).await;
        let now = chrono::Utc::now().timestamp();
        let remaining = expires_at.saturating_sub(now).max(0) as u64;
        tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
        // Only tear down if this is still the same session — a manual stop or a
        // fresh start (which bumps the epoch) supersedes this timer.
        if state.epoch() == armed_epoch && state.is_active() {
            log::info!("[support] temporary support code expired; ending session");
            state.request_stop();
        }
    });
    Ok(())
}

/// Apply an inbound `RevokeAccessGrant` from the manager (the source gate has
/// already dropped any non-central origin). Direct-closes every grant session this
/// host holds whose recorded generation is `≤ revoked_generation`, cutting an
/// already-established peer connection immediately after a dial-code regeneration —
/// the in-flight teardown that the `authorize` generation check alone can only
/// enforce on the session's *next* `RequestRemote`.
async fn handle_revoke_access_grant_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_signal_facade::model::access_grant::RevokeAccessGrantData;
    let payload = match model.get_data::<RevokeAccessGrantData>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[grant] bad RevokeAccessGrant payload: {e}");
            return Ok(());
        }
    };
    // Session-scoped teardown (the owner ended one temporary-support session):
    // close exactly that grant session, leaving its generation-mates up. A
    // generation-scoped frame (no session id) closes the whole superseded range.
    if let Some(grant_session_id) = payload.grant_session_id.as_deref() {
        log::info!(
            "[grant] manager revoked grant session {} for device {} (reason: {})",
            grant_session_id,
            payload.target_device,
            payload.reason
        );
        pc_manager::close_grant_session(
            &ctx.pc_registry,
            &ctx.worker_mgr,
            ctx.virtual_display.as_ref(),
            grant_session_id,
            &payload.reason,
        )
        .await;
        return Ok(());
    }
    log::info!(
        "[grant] manager revoked grants for device {} at generation <= {} (reason: {})",
        payload.target_device,
        payload.revoked_generation,
        payload.reason
    );
    pc_manager::close_grants_up_to_generation(
        &ctx.pc_registry,
        &ctx.worker_mgr,
        ctx.virtual_display.as_ref(),
        payload.revoked_generation,
        &payload.reason,
    )
    .await;
    Ok(())
}

fn handle_command_blocklist_sync_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::command_blocklist::{
        COMMAND_BLOCKLIST_SYNC_VERSION, CommandBlocklistSyncPayload,
        MIN_COMMAND_BLOCKLIST_SYNC_VERSION,
    };
    let payload = match model.get_data::<CommandBlocklistSyncPayload>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[router] bad CommandBlocklistSync payload: {e}");
            return Ok(());
        }
    };
    if !(MIN_COMMAND_BLOCKLIST_SYNC_VERSION..=COMMAND_BLOCKLIST_SYNC_VERSION)
        .contains(&payload.version)
    {
        log::warn!(
            "[router] ignoring CommandBlocklistSync with unsupported version {}",
            payload.version
        );
        return Ok(());
    }
    // The blocklist requires a revision to enforce monotonic ordering (a stale
    // frame must never roll back a newer set, re-opening a denied command). A
    // frame without one is malformed for this type — drop it and keep the
    // current cache (which is at worst the built-in floor).
    let Some(revision) = payload.command_blocklist_revision else {
        log::warn!("[router] dropping CommandBlocklistSync without a revision");
        return Ok(());
    };
    match ctx.command_blocklist.replace(payload.rules, revision) {
        Some(count) => log::info!(
            "[router] applied command-blocklist sync: {count} effective rule(s) (revision {revision})"
        ),
        None => {
            log::warn!("[router] command-blocklist sync at revision {revision} rejected as stale")
        }
    }
    Ok(())
}

/// Computes whether the daemon currently wants the worker in
/// exclusive mode, plus the pre-detach prompt duration to use when
/// entering. Both router (on control change) and supervisor (on
/// attach edge) reach the answer through this helper so there is a
/// single source of truth.
///
/// `active` is the supervisor's `is_active()` snapshot the caller has
/// already taken (the helper does **not** call back into the
/// supervisor — that would risk a lock cycle and re-introduce the
/// self-reference path codex round 7 #1 closed).
pub async fn compute_desired_with_active(
    settings: &crate::model::settings::SharedSettings,
    pc_registry: &PcRegistry,
    active: bool,
) -> (bool, u32) {
    let s = settings.read().await;
    let on = s.virtual_display.enabled && s.virtual_display.exclusive;
    let prompt_ms = s.virtual_display.prompt_ms;
    drop(s);
    if !on || !active {
        return (false, prompt_ms);
    }
    let any = pc_registry.any_with_accept_control().await;
    (any, prompt_ms)
}

/// Called by the `RequireControl` route after `handle_require_control`
/// settles the per-PC `accept_control` flag. Pokes the supervisor's
/// `set_desired_exclusive` so its internal driver loop can recompute
/// the IPC to send (if any).
///
/// `outcome.changed = false` short-circuits — a re-grant of an
/// already-accepted permission never moves the desired flag.
pub async fn update_exclusive_after_control_change(
    ctx: &RouterContext,
    outcome: &crate::daemon::pc_manager::ControlOutcome,
) {
    if !outcome.changed {
        return;
    }
    let Some(supervisor) = ctx.virtual_display.as_ref() else {
        return;
    };
    let active = supervisor.is_active().await;
    let (desired, prompt_ms) =
        compute_desired_with_active(&ctx.settings, &ctx.pc_registry, active).await;
    supervisor.set_desired_exclusive(desired, prompt_ms);
}

/// Emit an error response back to the browser via `outbound_tx`. The
/// browser's pending request matches on `request_id` + `signaling_type`.
/// Build / serialise failures are non-fatal — log and drop.
fn emit_error_response(
    ctx: &RouterContext,
    model: &SignalingModel,
    code: DeskErrorCode,
    message: &str,
) {
    match SignalingModel::error(
        &model.request_id,
        model.signaling_type,
        None,
        model.from_connection_id.clone(),
        code,
        message,
    ) {
        Ok(error_model) => match serde_json::to_string(&error_model) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(e) => log::warn!(
                "[router] failed to serialise {:?} error response: {e} (request_id={})",
                model.signaling_type,
                model.request_id,
            ),
        },
        Err(e) => log::warn!(
            "[router] failed to build {:?} error response: {e} (request_id={})",
            model.signaling_type,
            model.request_id,
        ),
    }
}

/// Synthesise an `Applied(width, height, refresh_hz)` success response
/// for a `ChangeDisplaySettings` request whose target already matches
/// the supervisor's cached mode. Used by the idempotent short-circuit:
/// when the browser asks for the resolution the IDD is already at, the
/// router replies inline without round-tripping to the worker. The
/// payload shape mirrors `signaling_proxy::build_virtual_display_response`'s
/// `Applied` branch (a `ChangeDisplaySettingsPayload` with `auto=false`)
/// so the browser cannot distinguish a real `Applied` from this synth.
fn emit_applied_response(
    ctx: &RouterContext,
    model: &SignalingModel,
    width: u32,
    height: u32,
    refresh_hz: u32,
) {
    let payload = ChangeDisplaySettingsPayload {
        width,
        height,
        refresh_hz,
        auto: false,
    };
    match SignalingModel::success_response(
        &model.request_id,
        model.signaling_type,
        None,
        model.from_connection_id.clone(),
        Some(&payload),
    ) {
        Ok(reply) => match serde_json::to_string(&reply) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(e) => log::warn!(
                "[router] failed to serialise idempotent ChangeDisplaySettings reply: {e} \
                 (request_id={})",
                model.request_id,
            ),
        },
        Err(e) => log::warn!(
            "[router] failed to build idempotent ChangeDisplaySettings reply: {e} \
             (request_id={})",
            model.request_id,
        ),
    }
}

/// Virtual display: validate + forward a browser-issued
/// `ChangeDisplaySettings`. Inbound model carries
/// `ChangeDisplaySettingsPayload`; daemon checks (in order):
///
/// 1. Service-mode only — `ctx.virtual_display.is_none()` ⇒
///    `FEATURE_UNAVAILABLE` ("only supported in service mode").
/// 2. Toggle on — `settings.virtual_display.enabled == false` ⇒
///    `FEATURE_UNAVAILABLE` ("not enabled").
/// 3. Supervisor live — `is_active() == false` ⇒
///    `FEATURE_UNAVAILABLE` ("unavailable").
/// 4. Payload parses — `INVALID_PARAMS`.
/// 5. Auto + single-client — `payload.auto && pc_registry.len() != 1`
///    ⇒ `INVALID_STATE` ("auto requires single client connection").
///    Manual requests bypass this guard. Server-wide
///    `desk_settings.adaptive_web_page_resolution` is *not* consulted
///    here — see the inline comment in the function body for why.
/// 6. Auto refresh-hz fallback — `payload.auto && refresh_hz == 0`
///    substitutes `supervisor.last_refresh_hz()` (or 60 on cold start)
///    so the daemon owns the authoritative refresh value.
/// 7. Mode within bounds — `validate_mode` ⇒ `INVALID_PARAMS`.
/// 8. Auto throttle — applied *after* `validate_mode` so an invalid
///    payload never burns the next legitimate slot. Interval comes
///    from `settings.virtual_display.adaptive_throttle_ms`; 0 disables
///    the throttle. Manual requests bypass.
/// 9. Worker reachable — `send_to_worker` ⇒ `REMOTE_DESK_OFFLINE`.
///
/// On success the typed `SetVirtualDisplayMode` IPC carries
/// `request_id` + `connection_id` so the worker's reply (via
/// `WorkerToService::VirtualDisplayMode`) can be ferried back to the
/// matching browser PC. `route` itself always returns `Ok(())` — the
/// browser-visible failure is the error response we already emitted.
async fn handle_change_display_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let supervisor = match ctx.virtual_display.as_ref() {
        Some(s) => s,
        None => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::FEATURE_UNAVAILABLE,
                "virtual display only supported in service mode",
            );
            return Ok(());
        }
    };
    let settings_snapshot = ctx.settings.read().await.clone();
    if !settings_snapshot.virtual_display.enabled {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "virtual display not enabled",
        );
        return Ok(());
    }
    if !supervisor.is_active().await {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "virtual display unavailable",
        );
        return Ok(());
    }
    let payload = match model.get_data::<ChangeDisplaySettingsPayload>() {
        Ok(p) => p,
        Err(e) => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                &format!("bad ChangeDisplaySettings payload: {e}"),
            );
            return Ok(());
        }
    };

    // Auto-only gate: single-client. Refuses so a second browser
    // cannot fight the first one over the IDD resolution; manual
    // requests bypass this (operators can still drive resolution from
    // any tab through the regular UI). Placed before `validate_mode`
    // so a multi-client tab gets the more informative INVALID_STATE
    // error rather than a generic INVALID_PARAMS on malformed inputs.
    //
    // No `desk_settings.adaptive_web_page_resolution` check here:
    // that field is per-connection (the browser dialog collects it and
    // ships it via UpdateDeskSettings, which the daemon forwards to
    // the worker without writing back to `ctx.settings.desk`). Reading
    // the server-wide default would always see `false`, blocking the
    // browser's request no matter how the user toggled the checkbox.
    // The browser hook already gates on the same flag locally, so the
    // request only reaches here when the user has opted in; defence in
    // depth is still provided by `virtual_display.enabled`,
    // `supervisor.is_active`, the single-client guard above, and the
    // throttle below.
    if payload.auto && ctx.pc_registry.len().await != 1 {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::INVALID_STATE,
            "auto requires single client connection",
        );
        return Ok(());
    }

    // Auto refresh-hz fallback: the browser hook ships `refresh_hz=0`
    // to let the daemon supply the authoritative value (most recent
    // IDD echo, or 60 as a cold-start default). This stays inside the
    // `payload.auto` branch — a manual `refresh_hz=0` must keep its
    // original semantics (rejected by `validate_mode` as a zero
    // dimension), which the regression test
    // `manual_zero_refresh_still_invalid` pins.
    let effective_refresh_hz = if payload.auto && payload.refresh_hz == 0 {
        let cached = supervisor.last_refresh_hz();
        if cached == 0 { 60 } else { cached }
    } else {
        payload.refresh_hz
    };

    let mode = VirtualDisplayMode {
        width: payload.width,
        height: payload.height,
        refresh_hz: effective_refresh_hz,
    };
    if let Err(e) = validate_mode(mode) {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::INVALID_PARAMS,
            &format!("invalid mode: {e}"),
        );
        return Ok(());
    }

    // Idempotent short-circuit: if the request's (width, height,
    // effective_refresh_hz) exactly matches the supervisor's cached
    // mode (last seen via the worker's `VirtualDisplayMode::Applied`
    // echo), skip the worker IPC entirely and synthesise an Applied
    // response inline. Rationale: the worker's `set_mode` path always
    // triggers an IDD Departure+Arrival driver cycle plus a WGC capture
    // restart, even when the resolution is unchanged. The browser's
    // adaptive-resolution hook frequently re-fires on devicePixelRatio
    // jitter at the same wrapper size, so dropping these no-op
    // round-trips removes a large source of visible WGC restart
    // hitches.
    //
    // Placed *after* `validate_mode` (so an invalid payload still
    // returns INVALID_PARAMS rather than masking the validation bug as
    // a fake Applied) and *before* `try_consume_auto_slot` (an
    // idempotent hit has zero IDD cost, so it should not consume a
    // throttle slot the operator has reserved for real changes).
    // `last_known_mode()` returns `None` until the worker has reported
    // a fully-formed Applied (all three components non-zero) AND the
    // current attach generation has not been torn down — dimensions
    // are cleared on every attach lifecycle transition, see
    // `VirtualDisplaySupervisor::reset_known_dimensions` doc.
    if let Some((cached_w, cached_h, cached_hz)) = supervisor.last_known_mode()
        && payload.width == cached_w
        && payload.height == cached_h
        && effective_refresh_hz == cached_hz
    {
        log::debug!(
            "[router] ChangeDisplaySettings idempotent hit {cached_w}x{cached_h}@{cached_hz}; \
             skipping worker IPC (request_id={})",
            model.request_id,
        );
        emit_applied_response(ctx, model, cached_w, cached_h, cached_hz);
        return Ok(());
    }

    // Throttle is the last gate before IPC. Placed *after*
    // `validate_mode` so an invalid auto payload never burns the
    // operator's next legitimate slot.
    if payload.auto {
        let min_interval =
            Duration::from_millis(settings_snapshot.virtual_display.adaptive_throttle_ms);
        if !supervisor.try_consume_auto_slot(tokio::time::Instant::now(), min_interval) {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_STATE,
                "auto change throttled",
            );
            return Ok(());
        }
    }

    let connection_id = model.from_connection_id.clone().unwrap_or_default();
    let ipc_payload = SetVirtualDisplayModePayload {
        request_id: model.request_id.clone(),
        connection_id,
        width: payload.width,
        height: payload.height,
        refresh_hz: effective_refresh_hz,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::SetVirtualDisplayMode(ipc_payload))
        .await
    {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::REMOTE_DESK_OFFLINE,
            &format!("worker unavailable: {e}"),
        );
    }
    Ok(())
}

/// Parse the inbound `EnablePrivateScreen` payload and ship
/// it to the worker as typed [`ServiceToWorker::EnablePrivateScreen`].
/// Replaces the legacy `SignalingMessage` opaque envelope.
///
/// Parse / send failures are non-fatal for the WS connection — they
/// only prevent the toggle from reaching the worker, which is logged.
async fn handle_enable_private_screen_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let from_connection_id = match model.from_connection_id.as_deref() {
        Some(id) => id.to_string(),
        None => {
            log::warn!(
                "[router] EnablePrivateScreen missing from_connection_id; ignoring \
                 (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let data = match model.get_data::<EnablePrivateScreenData>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!(
                "[router] EnablePrivateScreen payload parse failed for {from_connection_id}: \
                 {e}; ignoring"
            );
            return Ok(());
        }
    };
    let payload = EnablePrivateScreenPayload {
        connection_id: from_connection_id.clone(),
        enable: data.enable,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::EnablePrivateScreen(payload))
        .await
    {
        log::warn!(
            "[router] failed to send typed EnablePrivateScreen for {from_connection_id}: {e}",
        );
    }
    Ok(())
}

/// Parses the inbound `UpdateDeskSettings` payload, fans out the
/// media-relevant knobs as `UpdateMediaSettings` IPC (so the
/// per-connection encoder pipeline retunes live), applies the
/// connection-scoped adaptive-bitrate toggle, and ships the full
/// settings to the worker as typed
/// [`ServiceToWorker::UpdateDeskSettings`] (the worker keeps that
/// dispatch path as a hook; it currently applies nothing from it).
///
/// `adaptive_bitrate` deliberately does **not** ride the global
/// fan-out: it is a per-browser session preference (persisted in the
/// browser, not server-side), so it only updates the state of the
/// connection that sent the message. fps / quality / dirty_rect keep
/// their existing fan-out-to-all semantics.
async fn handle_update_desk_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let settings = match model.get_data::<DeskSettings>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] UpdateDeskSettings payload parse failed: {e}; dropping \
                 (no media retune, no worker forward)"
            );
            return Ok(());
        }
    };

    ctx.pc_registry
        .broadcast_media_settings_update(
            &ctx.worker_mgr,
            Some(settings.video_fps),
            None,
            Some(settings.video_quality),
            Some(settings.enable_dirty_rect),
        )
        .await;

    // Connection-scoped adaptive-bitrate toggle: lock → flip → ship
    // the Clear (if any) → commit, all under the state lock so a
    // stale SetCap from the RTCP task can never land after the Clear
    // (see `daemon::bitrate_controller` for the ordering contract).
    if let Some(conn_id) = model.from_connection_id.as_deref()
        && let Some(pc_ctx) = ctx.pc_registry.get(conn_id).await
    {
        let adaptive = { Arc::clone(&pc_ctx.read().await.adaptive_bitrate) };
        let mut state = adaptive.state.lock().await;
        if let Some(directive) = state.set_enabled_and_decide_clear(settings.adaptive_bitrate) {
            crate::daemon::pc_manager::send_cap_directive(
                &ctx.worker_mgr,
                conn_id,
                directive,
                &mut state,
            )
            .await;
        }
    }

    let from_connection_id = model
        .from_connection_id
        .clone()
        .unwrap_or_else(|| "<unscoped>".to_string());
    let payload = UpdateDeskSettingsPayload {
        connection_id: from_connection_id.clone(),
        settings,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::UpdateDeskSettings(payload))
        .await
    {
        log::warn!(
            "[router] failed to send typed UpdateDeskSettings for {from_connection_id}: {e}",
        );
    }
    Ok(())
}

// ---- Manager-plane typed-IPC dispatch helpers ----
//
// All five share the same skeleton — pull `from_connection_id` (the
// browser's PC ID), build the typed `ServiceToWorker::Manager*Request`
// payload, ship it via `WorkerManager::send_to_worker`. Differences are
// only in payload type and whether the inbound model carries a body.
// The `request_id` is echoed verbatim so the worker's
// `ManagerResponseRefPayload` / typed-response payload can correlate.
// Errors are non-fatal for the WS connection: parse / send failures
// log + drop, same fail-soft semantics the SignalingMessage bridge
// had.

/// Helper: extract `from_connection_id` from an inbound model when
/// the routing path requires one (e.g. terminal session traffic that
/// keys per-PTY on the originating browser/terminal connection).
/// Missing => log and return None so the caller drops the message.
fn require_from_connection_id<'a>(
    model: &'a SignalingModel,
    signaling_type_name: &'static str,
) -> Option<&'a str> {
    match model.from_connection_id.as_deref() {
        Some(id) => Some(id),
        None => {
            log::warn!(
                "[router] {signaling_type_name} missing from_connection_id; ignoring \
                 (request_id={})",
                model.request_id,
            );
            None
        }
    }
}

/// Clone `from_connection_id` for non-interactive manager requests that still
/// support request-id-only REST correlation.
fn optional_from_connection_id(model: &SignalingModel) -> Option<String> {
    model.from_connection_id.clone()
}

async fn handle_manager_system_info_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ManagerRequestRefPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerSystemInfoRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerSystemInfoRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_query_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ManagerRequestRefPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerQuerySettingsRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerQuerySettingsRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_file_list_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "ManagerFileList") else {
        return Ok(());
    };
    let params = match model.get_data::<FileListParams>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "[router] ManagerFileList payload parse failed for {connection_id:?}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerFileListRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: connection_id.to_string(),
        params,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerFileListRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerFileListRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_file_delete_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "ManagerFileDelete") else {
        return Ok(());
    };
    let request = match model.get_data::<DeleteFileRequest>() {
        Ok(r) => r,
        Err(e) => {
            log::warn!(
                "[router] ManagerFileDelete payload parse failed for {connection_id:?}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerFileDeleteRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: connection_id.to_string(),
        request,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerFileDeleteRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerFileDeleteRequest: {e}");
    }
    Ok(())
}

async fn handle_manager_update_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let connection_id = optional_from_connection_id(model);
    let settings = match model.get_data::<RemoteSystemSettings>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] ManagerUpdateSettings payload parse failed for {connection_id:?}: \
                 {e}; dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerUpdateSettingsRequestPayload {
        request_id: model.request_id.clone(),
        connection_id,
        settings,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerUpdateSettingsRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerUpdateSettingsRequest: {e}");
    }
    Ok(())
}

// ---- Terminal-plane typed-IPC dispatch helpers ----
//
// The 5 inbound terminal request types share the same skeleton as the
// manager-plane helpers — pull `from_connection_id`, build the typed
// `ServiceToWorker::*Request` payload, ship it via
// `WorkerManager::send_to_worker`. Differences are only in payload
// type and whether the inbound model carries a body / a request_id.
// Errors are non-fatal for the WS connection: parse / send failures
// log + drop.

async fn handle_start_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "StartTerminal") else {
        return Ok(());
    };
    let session = match model.get_data::<StartTerminalSession>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] StartTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    // The terminal WS is a distinct connection that never does a `RequestRemote`, so
    // this is its admission-establishing frame: register the connection's capability
    // ceiling + admission (+ grant index) from the validated stamp before shipping
    // the request to the worker, so the worker-side `meet(ceiling, global)` gate
    // enforces it from the very first terminal request. Fail-closed: a capped ceiling
    // that cannot reach the worker refuses the terminal (never starts it ceiling-less).
    if !register_terminal_admission(ctx, connection_id).await {
        return Ok(());
    }
    ctx.host_control_hub.host_activity().ensure_session(
        connection_id,
        ctx.inbound_start_terminal_authz
            .as_ref()
            .map(|authz| authz.actor.clone())
            .unwrap_or_else(desk_signal_facade::model::request_remote_authz::ActorSummary::unknown),
    );

    let payload = StartTerminalRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: connection_id.to_string(),
        session,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::StartTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed StartTerminalRequest: {e}");
    }
    Ok(())
}

/// Register a terminal connection's admission, worker ceiling, and grant index from
/// the validated `StartTerminal` stamp (the terminal analogue of what
/// `handle_request_remote` does for a control connection). Returns `false` — refuse
/// the terminal — only when a capped ceiling fails to reach the worker (fail-closed:
/// a terminal must never run with no worker-side cap, which would fall back to the
/// host global). A central stamp with `access_ceiling: None` is an owner session; a
/// bare frame (owner-only relay / local path, no stamp) is likewise admitted as
/// owner with no ceiling.
async fn register_terminal_admission(ctx: &RouterContext, connection_id: &str) -> bool {
    match ctx.inbound_start_terminal_authz.as_ref() {
        Some(authz) => {
            if let Some(ceiling) = authz.access_ceiling.as_ref() {
                if let Err(e) = ctx
                    .worker_mgr
                    .send_to_worker(ServiceToWorker::SetConnectionCeiling(
                        desk_ipc_protocol::message::SetConnectionCeilingPayload {
                            connection_id: connection_id.to_string(),
                            ceiling: Some(ceiling.clone()),
                        },
                    ))
                    .await
                {
                    log::warn!(
                        "[router] StartTerminal ceiling registration failed for {connection_id}: \
                         {e}; refusing terminal"
                    );
                    return false;
                }
                ctx.pc_registry
                    .record_admission(
                        connection_id,
                        pc_manager::Admission::Capped(ceiling.clone()),
                    )
                    .await;
            } else {
                ctx.pc_registry
                    .record_admission(connection_id, pc_manager::Admission::OwnerFull)
                    .await;
            }
            // Index a capped terminal under its grant so a directed revocation /
            // dial-code regeneration tears it down with the rest of the session.
            if let Some(gsid) = authz.grant_session_id.as_deref() {
                ctx.pc_registry
                    .index_grant_connection(gsid, authz.generation, connection_id)
                    .await;
            }
        }
        None => {
            ctx.pc_registry
                .record_admission(connection_id, pc_manager::Admission::OwnerFull)
                .await;
        }
    }
    ctx.pc_registry
        .mark_terminal_connection(connection_id)
        .await;
    true
}

async fn handle_send_data_to_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "SendDataToTerminal") else {
        return Ok(());
    };
    let data = match model.get_data_with_type::<TerminalInputData>() {
        Ok(Some(d)) => d,
        Ok(None) => {
            // Empty payload — match the legacy handler's silent ignore.
            return Ok(());
        }
        Err(e) => {
            log::warn!(
                "[router] SendDataToTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = SendDataToTerminalPayload {
        connection_id: connection_id.to_string(),
        data,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::SendDataToTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed SendDataToTerminalRequest: {e}");
    }
    Ok(())
}

async fn handle_resize_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "ResizeTerminal") else {
        return Ok(());
    };
    let data = match model.get_data_with_type::<TerminalResizeData>() {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(()),
        Err(e) => {
            log::warn!(
                "[router] ResizeTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ResizeTerminalPayload {
        connection_id: connection_id.to_string(),
        data,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ResizeTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ResizeTerminalRequest: {e}");
    }
    Ok(())
}

async fn handle_close_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "CloseTerminal") else {
        return Ok(());
    };
    let payload = CloseTerminalPayload {
        connection_id: connection_id.to_string(),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::CloseTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed CloseTerminalRequest: {e}");
    }

    // Clear the terminal connection's whole capability footprint so nothing survives
    // its close: worker ceiling, admission, grant index, terminal mark. Gated on the
    // terminal marker so a stray `CloseTerminal` from a non-terminal connection can
    // never clear that connection's admission. The connection id is a fresh UUID
    // (never reused), but clearing promptly also bounds the maps' growth.
    if ctx.pc_registry.is_terminal_connection(connection_id).await {
        if let Err(e) = ctx
            .worker_mgr
            .send_to_worker(ServiceToWorker::SetConnectionCeiling(
                desk_ipc_protocol::message::SetConnectionCeilingPayload {
                    connection_id: connection_id.to_string(),
                    ceiling: None,
                },
            ))
            .await
        {
            log::debug!(
                "[router] terminal ceiling clear for {connection_id} did not reach worker: {e}"
            );
        }
        ctx.pc_registry.clear_admission(connection_id).await;
        ctx.pc_registry
            .unindex_grant_connection(connection_id)
            .await;
        ctx.pc_registry
            .unmark_terminal_connection(connection_id)
            .await;
    }
    Ok(())
}

async fn handle_list_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ListTerminalRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ListTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ListTerminalRequest: {e}");
    }
    Ok(())
}

// ---- AI agent plane typed-IPC dispatch ----
//
// Inbound `AgentRequest` from a control end carries the
// non-authoritative `desk_agent_protocol::AgentRequestData` (operation +
// reason). The daemon two-phase-parses the operation against its
// supported-kind set (so an unknown *newer* kind degrades to
// `UnsupportedCapability` instead of failing serde), derives the
// capability from the input, authorizes it against a server-computed
// scope, stamps every trusted field, and ships a typed
// `ServiceToWorker::AgentRequest` to the worker. Any rejection short-
// circuits with an outbound `AgentResponse(AgentOutcome::Err)`; the
// route itself always returns `Ok(())` (the control-end-visible failure
// is the outcome we already emitted).

/// Outer `OperationInput` tags this build understands. A control end on
/// a newer protocol may send a kind outside this set; the two-phase
/// parse turns that into `UnsupportedCapability`.
const SUPPORTED_OPERATION_KINDS: &[&str] = &["read_context", "exec"];

/// Inner `ContextKind` tags (the actual P0 read capabilities) this build
/// can collect. The unknown-kind check descends to this level because
/// the permission point is nested — `operation.input.kind` is only the
/// `read_context` / `exec` dispatch layer; the real capability is
/// `operation.input.params.kind.kind`.
const SUPPORTED_READ_KINDS: &[&str] = &[
    "system_info",
    "process_list",
    "network_ports",
    "service_status",
    "log_recent",
    "container_list",
    "container_inspect",
    "container_logs",
    "screen_capture_current",
];

fn agent_error(
    kind: AgentErrorKind,
    message: &str,
    retryable: bool,
    safe_for_model: bool,
) -> AgentError {
    AgentError {
        kind,
        message: message.to_string(),
        retryable,
        safe_for_model,
        error_code: None,
    }
}

/// Two-phase unknown-kind validation over the raw `AgentRequestData`
/// JSON. Runs **before** the typed `from_value` so an unknown kind
/// surfaces as a structured `UnsupportedCapability` rather than a serde
/// parse error (which would arrive too late to build a graceful
/// outcome). Descends both the outer `operation.input.kind` and — for
/// `read_context` — the inner `operation.input.params.kind.kind`.
fn validate_agent_request_kinds(raw: &serde_json::Value) -> Result<(), AgentError> {
    let outer = raw
        .get("operation")
        .and_then(|o| o.get("input"))
        .and_then(|i| i.get("kind"))
        .and_then(|k| k.as_str());
    let Some(outer) = outer else {
        return Err(agent_error(
            AgentErrorKind::InvalidInput,
            "missing operation.input.kind",
            false,
            true,
        ));
    };
    if !SUPPORTED_OPERATION_KINDS.contains(&outer) {
        return Err(agent_error(
            AgentErrorKind::UnsupportedCapability,
            &format!("unsupported operation kind '{outer}'"),
            false,
            true,
        ));
    }
    if outer == "read_context" {
        let inner = raw
            .get("operation")
            .and_then(|o| o.get("input"))
            .and_then(|i| i.get("params"))
            .and_then(|p| p.get("kind"))
            .and_then(|k| k.get("kind"))
            .and_then(|k| k.as_str());
        let Some(inner) = inner else {
            return Err(agent_error(
                AgentErrorKind::InvalidInput,
                "missing operation.input.params.kind.kind",
                false,
                true,
            ));
        };
        if !SUPPORTED_READ_KINDS.contains(&inner) {
            return Err(agent_error(
                AgentErrorKind::UnsupportedCapability,
                &format!("unsupported read kind '{inner}'"),
                false,
                true,
            ));
        }
    }
    Ok(())
}

/// Server-computed grant for the single-machine read path. There is no
/// policy engine yet (that lands in M4), so the daemon grants the full
/// P0 read set in `ReadOnly` mode. The authorization *mechanism*
/// ([`authorize`]) is exercised regardless, so a future policy engine
/// only has to narrow `granted`.
fn default_read_scope() -> AgentScope {
    AgentScope {
        granted: vec![
            Capability::SystemInfo,
            Capability::ProcessList,
            Capability::NetworkPorts,
            Capability::ServiceStatus,
            Capability::LogRecent,
            Capability::ContainerList,
            Capability::ContainerInspect,
            Capability::ContainerLogs,
            Capability::ScreenCaptureCurrent,
        ],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    }
}

/// Whether `capability` is covered by the granted set. Pure so the
/// `PermissionDenied` path is unit-testable without a live router.
fn authorize(capability: Capability, granted: &[Capability]) -> bool {
    granted.contains(&capability)
}

/// Server-injected actor. Never sourced from the control end (which
/// structurally cannot express it — `AgentRequestData` carries no actor
/// field). The single-machine path has no session identity plumbed into the
/// router, so the local operator is represented as a `System` actor;
/// fleet / authenticated paths will inject the real principal here.
fn server_actor() -> ActorRef {
    ActorRef {
        actor_type: ActorType::System,
        actor_id: "local-operator".to_string(),
    }
}

/// Emit an `AgentResponse(AgentOutcome::Err)` back to the control end.
/// Business / capability-level failures ride the `signaling_data`
/// `AgentOutcome`, not `SignalingResponseState`, so the
/// control-end UI receives the full structured error. Build / serialise
/// failures are non-fatal — log + drop.
fn emit_agent_error(ctx: &RouterContext, model: &SignalingModel, error: AgentError) {
    let outcome = AgentOutcome::Err(error);
    match SignalingModel::success_response(
        &model.request_id,
        SignalingType::AgentResponse,
        None,
        model.from_connection_id.clone(),
        Some(&outcome),
    ) {
        Ok(reply) => match serde_json::to_string(&reply) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(e) => log::warn!(
                "[router] failed to serialise AgentResponse error: {e} (request_id={})",
                model.request_id,
            ),
        },
        Err(e) => log::warn!(
            "[router] failed to build AgentResponse error: {e} (request_id={})",
            model.request_id,
        ),
    }
}

/// Send one `DiagnoseEvent` to the control end as a **notification-style**
/// signaling frame. `response_state = None` is essential: a `Some(_)` value
/// marks the frame as the one-shot response to the originating `Diagnose`
/// request, which the signaling callback map consumes and removes — collapsing
/// the stream after the first frame. With `None`, every frame is delivered as an
/// event. Build / serialise failures are non-fatal — log + drop.
fn send_diagnose_frame(
    outbound_tx: &broadcast::Sender<String>,
    to_connection_id: Option<String>,
    event: DiagnoseEvent,
) {
    let request_id = event.request_id.clone();
    let data = match serde_json::to_value(&event) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[router] failed to serialise DiagnoseEvent: {e} (request_id={request_id})");
            return;
        }
    };
    let frame = SignalingModel::new(
        &request_id,
        SignalingType::DiagnoseEvent,
        None,
        to_connection_id,
        Some(data),
        // Notification, not a one-shot response — see the doc comment.
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = outbound_tx.send(text);
        }
        Err(e) => log::warn!(
            "[router] failed to serialise DiagnoseEvent frame: {e} (request_id={request_id})"
        ),
    }
}

/// Emit a single (typically terminal) `DiagnoseEvent` for a request, before the
/// orchestrator runs (disabled gate / unsupported mode / bad payload).
fn emit_diagnose_event(ctx: &RouterContext, model: &SignalingModel, event: DiagnoseEvent) {
    send_diagnose_frame(&ctx.outbound_tx, model.from_connection_id.clone(), event);
}

/// Route a control-end `TerminalCopilotAsk`. The terminal copilot is orchestrated
/// by the central signaling brain (signal / manager): the control end sends the
/// ask — carrying the terminal context inline — to the central server, which dials
/// the model and streams `TerminalCopilotEvent` frames back. This host runs no
/// local copilot. If an ask still reaches the edge router (a link without a central
/// brain), answer with one terminal `TerminalCopilotEvent::error` so the control
/// end stops waiting on the stream.
async fn handle_terminal_copilot_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let mut sink = copilot_signaling_sink(
        ctx.outbound_tx.clone(),
        model.from_connection_id.clone(),
        model.request_id.clone(),
    );
    sink.emit_error(agent_error(
        AgentErrorKind::UnsupportedCapability,
        "the terminal copilot is handled by the central signaling server",
        false,
        true,
    ));
    Ok(())
}

/// Route a control-end `TerminalCompleteAsk`. Inline command completion is
/// orchestrated centrally too: the central server dials the model over the inline
/// terminal context the control end supplies; the edge runs none locally. If an
/// ask still reaches the edge router, answer with one error `TerminalCompleteResult`
/// so the control end always gets a response.
async fn handle_terminal_complete_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::terminal_complete::TerminalCompleteResult;
    crate::diagnose::terminal_complete::send_completion_result(
        &ctx.outbound_tx,
        model.from_connection_id.clone(),
        &TerminalCompleteResult::failed(
            &model.request_id,
            agent_error(
                AgentErrorKind::UnsupportedCapability,
                "terminal command completion is handled by the central signaling server",
                false,
                true,
            ),
        ),
    );
    Ok(())
}

/// Route a control-end `Diagnose`. AI diagnosis is orchestrated by the central
/// signaling brain (signal / manager): the control end sends `Diagnose` to the
/// central server, which drives the model and pulls read-only evidence from this
/// host through a `CollectRequest` (served by `handle_collect_request_inbound`).
/// This host therefore never runs a browser-facing diagnosis locally. If a
/// `Diagnose` frame still reaches the edge router (a link without a central
/// brain), reply with one terminal `DiagnoseEvent::error` (notification-style,
/// never a one-shot response) so the control end stops treating frames as an
/// in-progress stream.
async fn handle_diagnose_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    emit_diagnose_event(
        ctx,
        model,
        DiagnoseEvent::error(
            &model.request_id,
            0,
            agent_error(
                AgentErrorKind::UnsupportedCapability,
                "AI diagnosis is handled by the central signaling server; this host only \
                 serves evidence collection",
                false,
                true,
            ),
        ),
    );
    Ok(())
}

/// Route a control-end `DiagnoseCancel` (handoff to a human). The message
/// `request_id` is the cancelled diagnosis's id. AI diagnosis is orchestrated by
/// the central signaling brain, which owns the run lifecycle and audit trail, so
/// on the edge this only aborts any locally tracked task handle (defensive) and
/// is otherwise a no-op. No `DiagnoseEvent` is streamed back — the control end
/// already closed the panel and retains the evidence locally.
async fn handle_diagnose_cancel_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    // Abort the in-flight run if it is still tracked, so a slow model call stops
    // instead of streaming into a closed / superseded connection.
    if let Some(handle) = ctx
        .diagnose_tasks
        .lock()
        .expect("diagnose tasks lock")
        .remove(&model.request_id)
    {
        handle.abort();
    }
    Ok(())
}

/// Send an `ExecPreview(606)` to the control end as a notification-style frame
/// (`response_state = None`), mirroring `send_diagnose_frame`. Build / serialise
/// failures are non-fatal — log + drop.
pub(crate) fn send_exec_preview(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    to_connection_id: Option<String>,
    preview: ExecPreview,
) {
    send_notification(
        outbound_tx,
        request_id,
        SignalingType::ExecPreview,
        to_connection_id,
        &preview,
        "ExecPreview",
    );
}

/// Send an `ExecResult(609)` to the control end as a notification-style frame.
fn send_exec_result(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    to_connection_id: Option<String>,
    payload: ExecResultPayload,
) {
    send_notification(
        outbound_tx,
        request_id,
        SignalingType::ExecResult,
        to_connection_id,
        &payload,
        "ExecResult",
    );
}

/// Shared notification-frame sender for the exec plane. `response_state = None`
/// so the control end treats each frame as an event, not a one-shot response.
fn send_notification<T: serde::Serialize>(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    signaling_type: SignalingType,
    to_connection_id: Option<String>,
    data: &T,
    label: &str,
) {
    let value = match serde_json::to_value(data) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[router] failed to serialise {label}: {e} (request_id={request_id})");
            return;
        }
    };
    let frame = SignalingModel::new(
        request_id,
        signaling_type,
        None,
        to_connection_id,
        Some(value),
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = outbound_tx.send(text);
        }
        Err(e) => {
            log::warn!("[router] failed to serialise {label} frame: {e} (request_id={request_id})")
        }
    }
}

/// Build a non-executable [`ExecPreview`] (blocked / off-template / mode-denied /
/// gate-denied). No pending approval is created.
#[allow(clippy::too_many_arguments)]
fn non_executable_preview(
    shell: String,
    command: String,
    cwd: Option<String>,
    timeout_ms: u32,
    risk: desk_agent_protocol::RiskLevel,
    impact: String,
    policy_note: Option<String>,
    blocked_reason: Option<String>,
) -> ExecPreview {
    ExecPreview {
        exec_request_id: None,
        shell,
        command,
        cwd,
        timeout_ms,
        risk,
        impact,
        policy_note,
        requires_confirmation: false,
        executable: false,
        blocked_reason,
    }
}

/// Extract the shell label from an exec target (empty for a non-shell target).
fn exec_shell_label(input: &desk_agent_protocol::ExecInput) -> String {
    match &input.target {
        desk_agent_protocol::ExecTarget::Shell { shell } => shell.clone(),
        desk_agent_protocol::ExecTarget::Domain { .. } => String::new(),
    }
}

/// Route a control-end `ConfirmExec`: gate → classify → (on an executable
/// classification permitted by the current mode) park an immutable plan draft
/// and stream an `ExecPreview` with the minted `exec_request_id`; otherwise
/// stream a non-executable preview. Never executes — that needs an explicit
/// `ResolveExec(Approve)`.
async fn handle_confirm_exec_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let to = model.from_connection_id.clone();
    let request_id = model.request_id.clone();

    let data = match model.get_data::<ConfirmExecData>() {
        Ok(d) => d,
        Err(e) => {
            // A malformed payload still has to leave a trace. The manager records
            // its own authorization of this frame, so a rejection that reported
            // nothing would read as a dispatch the host never acknowledged. This
            // is a protocol error rather than a capability decision — no capability
            // has been determined yet — so it is a task failure. The parser's
            // message may echo payload fragments and is deliberately not stored;
            // only the error kind is.
            ctx.audit
                .record(AuditEvent::task_failed_for_request(
                    new_audit_event_id(),
                    audit_now(),
                    &request_id,
                    &agent_error(
                        AgentErrorKind::InvalidInput,
                        "bad ConfirmExec payload",
                        false,
                        true,
                    ),
                ))
                .await;
            send_exec_preview(
                &ctx.outbound_tx,
                &request_id,
                to,
                non_executable_preview(
                    String::new(),
                    String::new(),
                    None,
                    0,
                    desk_agent_protocol::RiskLevel::High,
                    "Invalid request".to_string(),
                    Some(format!("bad ConfirmExec payload: {e}")),
                    None,
                ),
            );
            return Ok(());
        }
    };

    // The operation must be an exec; a read operation is a protocol error.
    let OperationInput::Exec(exec_input) = data.operation.input else {
        // Recorded for the same reason as the parse failure above: a protocol
        // error, not a capability decision.
        ctx.audit
            .record(AuditEvent::task_failed_for_request(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                &agent_error(
                    AgentErrorKind::InvalidInput,
                    "ConfirmExec requires an exec operation",
                    false,
                    true,
                ),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                String::new(),
                String::new(),
                None,
                0,
                desk_agent_protocol::RiskLevel::High,
                "Invalid request".to_string(),
                Some("ConfirmExec requires an exec operation".to_string()),
                None,
            ),
        );
        return Ok(());
    };

    let shell = exec_shell_label(&exec_input);
    let command = exec_input.command.clone();
    let cwd = exec_input.cwd.clone();
    let limits = crate::exec::ExecLimits::clamped(&exec_input);

    // Gate: confirmed execution is unavailable in ServiceDaemon mode.
    if !ctx.exec_supported {
        // Unlike the two protocol errors above, this is a genuine capability
        // refusal: the request was well-formed and the host is declining the
        // capability outright. The risk is unknown here — classification has not
        // run — so the ceiling is recorded rather than a computed value.
        ctx.audit
            .record(AuditEvent::capability_denied(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                risk_str(desk_agent_protocol::RiskLevel::High),
                "exec unsupported in this startup mode".to_string(),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                shell,
                command,
                cwd,
                limits.timeout_ms,
                desk_agent_protocol::RiskLevel::High,
                "AI command execution is not available in this mode".to_string(),
                Some("unsupported in this startup mode".to_string()),
                None,
            ),
        );
        return Ok(());
    }

    // The execution mode is the device owner's local ceiling on AI action.
    // Provider credentials live on the central brain, so there is no local
    // "gateway configured" gate here: confirmed execution is gated by worker
    // support (above) and central authorization (the PDP checks below). On a
    // central link the policy decision's mode applies but the local mode is an
    // upper bound — the local setting can narrow a centrally issued authorization,
    // never widen it (a SuggestOnly / ReadOnly local config caps a broad central
    // grant). Off that link the local mode applies directly.
    let execution_mode = {
        let s = ctx.settings.read().await;
        match &ctx.inbound_authz {
            Some(authz) => authz.scope.mode.restrict_to(s.ai_policy.execution_mode),
            None => s.ai_policy.execution_mode,
        }
    };

    // Classify against the built-in baseline unioned with the operator
    // templates synced from the manager (empty on single-machine links), using
    // the effective blocklist (built-in floor on single-machine / unsynced links,
    // the manager's built-in-minus-disabled ∪ custom set on a fleet link).
    let operator_templates = ctx.command_templates.snapshot();
    let effective_blocklist = ctx.command_blocklist.snapshot();
    let outcome = crate::exec::classify_command_with_all(
        &exec_input,
        &operator_templates,
        &effective_blocklist,
    );
    let classification = outcome.classification;

    // Fleet PDP risk ceiling (manager link): refuse a command whose classified
    // risk exceeds the policy's `max_risk`, regardless of execution mode.
    if let Some(authz) = &ctx.inbound_authz
        && classification.risk > authz.max_risk
    {
        ctx.audit
            .record(AuditEvent::capability_denied(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                risk_str(classification.risk),
                classification.impact.clone(),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                shell,
                command,
                cwd,
                limits.timeout_ms,
                classification.risk,
                "command exceeds the policy risk ceiling".to_string(),
                Some("blocked by policy max_risk".to_string()),
                None,
            ),
        );
        return Ok(());
    }

    // Fleet PDP capability gate (manager link): the command's required exec
    // capability — the `shell.exec.readonly` vs `shell.exec.confirmed` split
    // decided by the server-side classification — must be in the policy-granted
    // scope. This mirrors the AgentRequest read path: a policy that grants only
    // `shell.exec.readonly` must not run a mutating command even when the mode
    // and `max_risk` would otherwise allow it. Without a manager authorization
    // (single-machine / remote-signaling) the local mode / template gating is
    // the authority, so the check is skipped.
    if let Some(authz) = &ctx.inbound_authz
        && let Some(required) = OperationInput::required_capability(&classification)
        && !authorize(required, &authz.scope.granted)
    {
        ctx.audit
            .record(AuditEvent::capability_denied(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                risk_str(classification.risk),
                classification.impact.clone(),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                shell,
                command,
                cwd,
                limits.timeout_ms,
                classification.risk,
                "command requires a capability the policy does not grant".to_string(),
                Some("blocked by policy scope".to_string()),
                None,
            ),
        );
        return Ok(());
    }

    // Decide executability from the classification + the active execution mode.
    let mode_note = match (
        classification.decision,
        classification.effect,
        execution_mode,
    ) {
        (ExecDecision::Blocked, _, _) => {
            ctx.audit
                .record(AuditEvent::capability_denied(
                    new_audit_event_id(),
                    audit_now(),
                    &request_id,
                    risk_str(classification.risk),
                    classification.impact.clone(),
                ))
                .await;
            send_exec_preview(
                &ctx.outbound_tx,
                &request_id,
                to,
                non_executable_preview(
                    shell,
                    command,
                    cwd,
                    limits.timeout_ms,
                    classification.risk,
                    classification.impact.clone(),
                    None,
                    Some(classification.impact),
                ),
            );
            return Ok(());
        }
        (ExecDecision::NotExecutable, _, _) => {
            Some("command does not match a safe template; run it manually instead".to_string())
        }
        (ExecDecision::ConfirmRequired, _, ExecutionMode::SuggestOnly) => {
            Some("AI command execution is disabled (suggest-only mode)".to_string())
        }
        (ExecDecision::ConfirmRequired, Some(ExecEffect::Mutating), ExecutionMode::ReadOnly) => {
            Some("read-only mode does not permit state-changing commands".to_string())
        }
        // SessionApproved executes like ConfirmEachAction, except the first
        // confirmation of a given template grants it for the rest of the
        // session (handled below). Automated (run without any confirmation)
        // is not implemented.
        (ExecDecision::ConfirmRequired, _, ExecutionMode::Automated) => {
            Some("execution mode not available".to_string())
        }
        (ExecDecision::ConfirmRequired, _, _) => None, // executable
    };

    // Executable iff the classification is ConfirmRequired and the mode allows
    // it (no `mode_note` was produced) and a draft was rendered.
    if mode_note.is_none()
        && classification.decision == ExecDecision::ConfirmRequired
        && let Some(draft) = outcome.draft
    {
        let capability = OperationInput::required_capability(&classification).map(|c| c.as_str());
        let risk = classification.risk;

        // On a manager link the ConfirmExec frame request_id is the PDP's
        // authorization-ledger key; carry it through the whole exec lifecycle so
        // every audit event (here and on the later ResolveExec / worker-result
        // paths) can be attributed to the real operator. Single-machine /
        // remote-signaling links have no ledger, so this stays None and the
        // audit `task_id` is unchanged.
        let audit_source = ctx.inbound_authz.as_ref().map(|_| request_id.clone());

        // SessionApproved grant eligibility: the active mode is SessionApproved,
        // the command matched a template (intersect with the whitelist — only
        // an already-executable template is ever granted), and the request came
        // over a connection we can key the grant to.
        let session_template = (execution_mode == ExecutionMode::SessionApproved)
            .then(|| classification.matched_template.clone())
            .flatten();
        let connection_id = model.from_connection_id.clone();

        // Already granted this session → auto-execute without re-prompting.
        if let (Some(template_id), Some(conn)) = (session_template.as_ref(), connection_id.as_ref())
            && ctx.session_approvals.is_granted(conn, template_id)
        {
            let exec_request_id = crate::daemon::exec_approval::mint_exec_request_id();
            // The frame that triggers execution is this ConfirmExec itself: the
            // session grant means no separate approval round happens.
            let (approval_id, plan) = crate::daemon::exec_approval::seal_plan(
                exec_request_id.clone(),
                &request_id,
                draft,
            );
            // No new approval prompt; the prior session grant authorizes it.
            ctx.audit
                .record(
                    AuditEvent::capability_allowed(
                        new_audit_event_id(),
                        audit_now(),
                        &exec_request_id.0,
                        capability,
                        risk_str(risk),
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            ctx.audit
                .record(
                    AuditEvent::command_executed(
                        new_audit_event_id(),
                        audit_now(),
                        &exec_request_id.0,
                        &approval_id.0,
                        capability,
                        risk_str(risk),
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            // Informational preview (no confirmation) so the control end can
            // show what ran; the result follows as an `ExecResult`.
            let preview = ExecPreview {
                exec_request_id: Some(exec_request_id),
                shell,
                command,
                cwd,
                timeout_ms: limits.timeout_ms,
                risk: classification.risk,
                impact: classification.impact,
                policy_note: classification
                    .matched_template
                    .map(|t| format!("session-approved template {t}")),
                requires_confirmation: false,
                executable: true,
                blocked_reason: None,
            };
            send_exec_preview(&ctx.outbound_tx, &request_id, to.clone(), preview);
            dispatch_exec_plan(ctx, &request_id, to, plan, audit_source).await;
            return Ok(());
        }

        // Not yet granted (or not session-approved mode) → park and prompt.
        // `session_template` (when present) is carried so that approving this
        // preview grants the template for the rest of the session.
        let exec_request_id = ctx.exec_approvals.insert(
            draft,
            classification.clone(),
            connection_id,
            session_template,
            audit_source.clone(),
        );
        ctx.audit
            .record(
                AuditEvent::capability_requested(
                    new_audit_event_id(),
                    audit_now(),
                    &exec_request_id.0,
                    capability,
                    risk_str(risk),
                    classification.impact.clone(),
                )
                .with_task_id(audit_source.as_deref()),
            )
            .await;
        let preview = ExecPreview {
            exec_request_id: Some(exec_request_id),
            shell,
            command,
            cwd,
            timeout_ms: limits.timeout_ms,
            risk: classification.risk,
            impact: classification.impact,
            policy_note: classification
                .matched_template
                .map(|t| format!("matched template {t}")),
            requires_confirmation: true,
            executable: true,
            blocked_reason: None,
        };
        send_exec_preview(&ctx.outbound_tx, &request_id, to, preview);
        return Ok(());
    }

    // Not executable under the current mode / classification.
    ctx.audit
        .record(AuditEvent::capability_denied(
            new_audit_event_id(),
            audit_now(),
            &request_id,
            risk_str(classification.risk),
            mode_note
                .clone()
                .unwrap_or_else(|| classification.impact.clone()),
        ))
        .await;
    send_exec_preview(
        &ctx.outbound_tx,
        &request_id,
        to,
        non_executable_preview(
            shell,
            command,
            cwd,
            limits.timeout_ms,
            classification.risk,
            classification.impact,
            mode_note,
            None,
        ),
    );
    Ok(())
}

/// Route a control-end `ResolveExec`: consume the pending approval (once) and,
/// on approve, seal the stored draft into an `ExecPlan` and dispatch it. Reject
/// just consumes the pending and ends. A missing / expired / already-consumed id
/// on approve returns an error `ExecResult`.
/// Handle an `ExecControl(623)`: stop an execution, or report on one.
///
/// Both actions answer with the same `ExecStateReply(624)` built from the durable
/// ledger. The ledger is asked *after* a cancel has been passed to the worker so
/// the reply reflects the request, and a generation the worker is not running is
/// not an error — it has very likely just finished, and the ledger says so.
async fn handle_exec_control_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request_id = model.request_id.clone();
    let to = model.from_connection_id.clone();

    let payload = match model.get_data::<ExecControlPayload>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[router] bad ExecControl payload: {e} (request_id={request_id})");
            return Ok(());
        }
    };
    let generation = payload.execution_generation.clone();

    if let ExecControlAction::Cancel { requested_by } = &payload.action {
        log::info!(
            "[router] exec cancel requested: generation={generation} by={requested_by} \
             (request_id={request_id})"
        );
        // Best-effort by design: the worker may be gone, or the command may have
        // just finished. Either way the ledger below reports what is actually
        // true, rather than this send's success standing in for it.
        if let Err(e) = ctx
            .worker_mgr
            .send_to_worker(ServiceToWorker::ExecCancel(ExecCancelPayload {
                execution_generation: generation.clone(),
            }))
            .await
        {
            log::warn!("[router] could not pass the cancel to the worker: {e}");
        }
        // `requested_by` is a wire hint only; the audit pipeline stamps the
        // authenticated actor, so a control end cannot name someone else as the
        // one who stopped a command.
        ctx.audit
            .record(AuditEvent::command_cancel_requested(
                new_audit_event_id(),
                audit_now(),
                &generation,
            ))
            .await;
    }

    let reply = match ctx.exec_ledger.describe(&generation).await {
        Ok(reply) => reply,
        Err(e) => {
            log::warn!("[router] could not read the exec ledger: {e} (generation={generation})");
            return Ok(());
        }
    };

    send_notification(
        &ctx.outbound_tx,
        &request_id,
        SignalingType::ExecStateReply,
        to,
        &reply,
        "ExecStateReply",
    );
    Ok(())
}

async fn handle_resolve_exec_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request_id = model.request_id.clone();
    let to = model.from_connection_id.clone();

    let data = match model.get_data::<ResolveExecData>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[router] bad ResolveExec payload: {e} (request_id={request_id})");
            return Ok(());
        }
    };

    use crate::daemon::exec_approval::TakeOutcome;
    use desk_agent_protocol::exec::ApprovalDecision;

    // Agentic (model-initiated) exec: the loop is awaiting this decision through the
    // coordinator. If it matches, deliver the decision and stop — the agentic seam
    // drives dispatch itself, so it must not also run the browser-initiated
    // park/consume flow below. The command's completion is audited on the worker
    // `ExecResult` round-trip (the `command_completed` event); approval-event audit
    // parity with the browser flow is a follow-up (the manager runtime records the
    // full approval lifecycle in its durable work-item audit).
    if ctx.agentic_exec.resolve_approval(
        &data.exec_request_id.0,
        matches!(data.decision, ApprovalDecision::Approve),
    ) {
        return Ok(());
    }

    // Approve / reject are bound to the connection that requested the preview.
    let outcome = ctx
        .exec_approvals
        .take(&data.exec_request_id, to.as_deref());
    match data.decision {
        ApprovalDecision::Reject => {
            match outcome {
                TakeOutcome::Consumed(consumed) => {
                    // Consumed so it cannot be approved later; the control end
                    // already updated its UI, so no result frame is sent. Carry
                    // the source ConfirmExec frame id (stored at park time) so the
                    // rejection is attributed to the real operator on a manager
                    // link, not the reporting host's token owner.
                    ctx.audit
                        .record(
                            AuditEvent::approval_denied(
                                new_audit_event_id(),
                                audit_now(),
                                &data.exec_request_id.0,
                            )
                            .with_task_id(consumed.source_request_id.as_deref()),
                        )
                        .await;
                }
                TakeOutcome::Forbidden => {
                    log::warn!(
                        "[router] ResolveExec(Reject) from a non-owning connection, ignored \
                         (exec_request_id={})",
                        data.exec_request_id.0
                    );
                }
                TakeOutcome::NotFound => {}
            }
            Ok(())
        }
        ApprovalDecision::Approve => {
            let consumed = match outcome {
                TakeOutcome::Consumed(c) => c,
                // Unknown/expired and cross-connection both return the same
                // generic error (do not leak whether the id exists).
                other => {
                    if matches!(other, TakeOutcome::Forbidden) {
                        log::warn!(
                            "[router] ResolveExec(Approve) from a non-owning connection, denied \
                             (exec_request_id={})",
                            data.exec_request_id.0
                        );
                    }
                    send_exec_result(
                        &ctx.outbound_tx,
                        &request_id,
                        to,
                        ExecResultPayload {
                            exec_request_id: data.exec_request_id,
                            outcome: AgentOutcome::Err(agent_error(
                                AgentErrorKind::InvalidInput,
                                "approval expired or already used",
                                false,
                                true,
                            )),
                        },
                    );
                    return Ok(());
                }
            };

            let capability =
                OperationInput::required_capability(&consumed.classification).map(|c| c.as_str());
            let risk = risk_str(consumed.classification.risk);
            // The ResolveExec frame carrying the approval is what triggers this
            // dispatch, so it is the generation; the ConfirmExec that produced the
            // preview only classified it.
            let (approval_id, plan) = crate::daemon::exec_approval::seal_plan(
                data.exec_request_id.clone(),
                &request_id,
                consumed.draft,
            );
            // Approval granted → capability allowed → command dispatched.
            // ResolveExec is not PDP-wrapped, so the operator ledger key comes
            // from the source ConfirmExec frame request_id stored at park time.
            let xr = data.exec_request_id.0.clone();
            let audit_source = consumed.source_request_id.clone();
            ctx.audit
                .record(
                    AuditEvent::approval_granted(
                        new_audit_event_id(),
                        audit_now(),
                        &xr,
                        &approval_id.0,
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            ctx.audit
                .record(
                    AuditEvent::capability_allowed(
                        new_audit_event_id(),
                        audit_now(),
                        &xr,
                        capability,
                        risk,
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            ctx.audit
                .record(
                    AuditEvent::command_executed(
                        new_audit_event_id(),
                        audit_now(),
                        &xr,
                        &approval_id.0,
                        capability,
                        risk,
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            // In SessionApproved mode, approving the first preview of a
            // template grants it for the rest of this connection's session, so
            // subsequent matching commands skip confirmation. The grant is
            // keyed to the connection that requested the preview.
            if let (Some(template_id), Some(conn)) = (
                consumed.session_grant_template.as_ref(),
                consumed.connection_id.as_ref(),
            ) {
                ctx.session_approvals.grant(conn, template_id);
            }
            let result_to = consumed.connection_id.or(to);
            dispatch_exec_plan(ctx, &request_id, result_to, plan, audit_source).await;
            Ok(())
        }
    }
}

/// Dispatch a sealed [`ExecPlan`] to the worker for execution. The worker runs
/// the argv verbatim and replies with `WorkerToService::ExecResult`, which the
/// signaling proxy turns into the outbound `ExecResult(609)` frame. The
/// `request_id` / `connection_id` are echoed through so the proxy can route that
/// frame back. If the worker is unreachable, synthesize an error result here so
/// the control end still gets a definite answer.
async fn dispatch_exec_plan(
    ctx: &RouterContext,
    request_id: &str,
    to_connection_id: Option<String>,
    plan: desk_agent_protocol::exec::ExecPlan,
    audit_source_request_id: Option<String>,
) {
    let exec_request_id = plan.exec_request_id.clone();
    let plan_generation = plan.execution_generation.clone();

    // Claim this dispatch in the ledger before the worker can start anything. A
    // redelivered frame is answered from the record instead of run a second time.
    match admit_exec(ctx, &plan).await {
        ExecAdmission::Spawn => {}
        ExecAdmission::Replay(result) => {
            let outcome = serde_json::from_str::<AgentOutcome>(&result).unwrap_or_else(|e| {
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    &format!("stored result could not be read: {e}"),
                    false,
                    true,
                ))
            });
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    outcome,
                },
            );
            return;
        }
        ExecAdmission::AcceptedOutcomeUnknown(reason) => {
            // Deliberately not an error that reads as "did not run": the change may
            // already have happened, and saying otherwise would invite a retry of it.
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    outcome: AgentOutcome::Err(agent_error(
                        AgentErrorKind::Internal,
                        &reason,
                        false,
                        true,
                    )),
                },
            );
            return;
        }
        ExecAdmission::Refused(reason) => {
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    outcome: AgentOutcome::Err(agent_error(
                        AgentErrorKind::PermissionDenied,
                        &reason,
                        false,
                        true,
                    )),
                },
            );
            return;
        }
        ExecAdmission::AtCapacity(reason) => {
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    // Retryable: nothing ran, and the ceiling frees up.
                    outcome: AgentOutcome::Err(agent_error(
                        AgentErrorKind::HostAtCapacity,
                        &reason,
                        true,
                        true,
                    )),
                },
            );
            return;
        }
    }

    let payload = ExecPlanPayload {
        request_id: request_id.to_string(),
        connection_id: to_connection_id.clone(),
        plan,
        audit_source_request_id,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ExecPlan(payload))
        .await
    {
        // Nothing was started, so the slot is free again immediately.
        ctx.exec_capacity.release(&plan_generation);
        send_exec_result(
            &ctx.outbound_tx,
            request_id,
            to_connection_id,
            ExecResultPayload {
                exec_request_id,
                outcome: AgentOutcome::Err(agent_error(
                    AgentErrorKind::TargetOffline,
                    &format!("worker unavailable: {e}"),
                    true,
                    true,
                )),
            },
        );
    }
}

/// Send a `EdgeExecResult(614)` toward the manager as a notification-style
/// frame, correlated by the per-attempt `request_id`. Used both for the early
/// PEP rejections (synthesized here) and for the worker's completed result
/// (relayed by the signaling proxy).
pub(crate) fn send_edge_exec_result(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    disposition: EdgeExecDisposition,
) {
    let payload = EdgeExecResultPayload {
        request_id: request_id.to_string(),
        disposition,
    };
    send_notification(
        outbound_tx,
        request_id,
        SignalingType::EdgeExecResult,
        None,
        &payload,
        "EdgeExecResult",
    );
}

/// Whether the sealed plan's own render matches the [`ExecPlanDraft`] a validator
/// authoritatively reconstructed. Compares every executable field (the ids and
/// approval token are not on the draft). Shared by the fleet and agentic PEP paths.
fn plan_matches_draft(
    plan: &ExecPlan,
    expected: &desk_agent_protocol::exec::ExecPlanDraft,
) -> bool {
    expected.program == plan.program
        && expected.argv == plan.argv
        && expected.risk == plan.risk
        && expected.shell == plan.shell
        && expected.cwd == plan.cwd
        && expected.template_id == plan.template_id
        && expected.timeout_ms == plan.timeout_ms
        && expected.max_stdout_bytes == plan.max_stdout_bytes
        && expected.max_stderr_bytes == plan.max_stderr_bytes
        && expected.fingerprint == plan.fingerprint
}

/// Source-agnostic PEP checks that every sealed [`ExecPlan`] must pass regardless
/// of how it was rendered: the effective blocklist over the full argv, and the
/// `risk <= max_risk` ceiling. The template-reproduction check differs by source
/// and lives in the per-source validators.
fn pep_common_checks(
    plan: &ExecPlan,
    max_risk: desk_agent_protocol::RiskLevel,
    blocklist: &[desk_agent_protocol::command_blocklist::BlocklistRule],
) -> Option<String> {
    // The blocklist operates over the full argv (program is `argv[0]`), matched
    // against the effective set (built-in floor on an unsynced link, the manager's
    // built-in-minus-disabled ∪ custom set on a fleet link) — never a second
    // compiled-in pass, so an admin-disabled rule is genuinely gone here too.
    let full_argv: Vec<String> = std::iter::once(plan.program.clone())
        .chain(plan.argv.iter().cloned())
        .collect();
    let lc = full_argv.join(" ").to_ascii_lowercase();
    if let Some(rule) = desk_agent_protocol::command_blocklist::blocklist_match(blocklist, &lc) {
        return Some(format!("pep_rejected:blocklist:{rule}"));
    }

    // max_risk ceiling (independent of the manager's per-device decision).
    if plan.risk > max_risk {
        return Some(format!(
            "pep_rejected:risk_exceeds_max:{:?}>{:?}",
            plan.risk, max_risk
        ));
    }

    // Enforcement-tier fail-closed: a plan that demands native-hard containment must
    // not spawn on a host that can only provide the baseline tier. This runs before
    // dispatch (the reason surfaces as RejectedBeforeDispatch), so the host never
    // silently downgrades — the manager only ever learns the command ran under the
    // tier it required, or that it was refused.
    if plan.containment.required_enforcement
        == desk_agent_protocol::exec::RequiredEnforcement::NativeHard
        && !crate::worker::exec_containment::provides_native_hard()
    {
        return Some("pep_rejected:native_hard_unavailable".to_string());
    }

    None
}

/// Re-validate a manager-sealed **fleet** [`ExecPlan`] against this daemon's own
/// view (defense in depth — the manager draft is never trusted). Returns the
/// model-safe rejection reason on the first failure, or `None` when the plan
/// passes. Order: common checks (blocklist, risk ceiling) → exact-argv whitelist
/// + fingerprint.
///
/// Fleet exec has no per-request limit input, so the authoritative render is
/// always the fixed fleet defaults with no cwd (identical to the sealing side in
/// `fleet_approval::verify_template_unchanged`). The plan's own `cwd` /
/// `timeout_ms` / output caps are therefore compared *against those authoritative
/// values* — never fed back into the expected render. If the render used the
/// plan's own limits, a tampered limit would be hashed into both sides and the
/// fingerprint would still agree (self-consistent tamper); rebuilding from the
/// fixed authority is what makes a widened timeout / output cap detectable. A
/// `template_id` is unique only per-org, so several synced **operator** templates
/// can share it; try every candidate and accept if any one reproduces the plan
/// exactly. This path never consults the built-in templates — a fleet plan must
/// come from an operator template.
fn validate_fleet_edge_exec(
    plan: &ExecPlan,
    max_risk: desk_agent_protocol::RiskLevel,
    templates: &[desk_agent_protocol::command_template::SyncedCommandTemplate],
    blocklist: &[desk_agent_protocol::command_blocklist::BlocklistRule],
) -> Option<String> {
    if let Some(reason) = pep_common_checks(plan, max_risk, blocklist) {
        return Some(reason);
    }

    let mut saw_candidate = false;
    let mut faithful = false;
    for template in templates
        .iter()
        .filter(|t| t.template_id == plan.template_id)
    {
        saw_candidate = true;
        let expected = build_exact_argv_draft(
            template,
            None,
            DEFAULT_OUTPUT_BYTES,
            DEFAULT_OUTPUT_BYTES,
            None,
        );
        if plan_matches_draft(plan, &expected) {
            faithful = true;
            break;
        }
    }
    if !faithful {
        return Some(if saw_candidate {
            "pep_rejected:template_drift".to_string()
        } else {
            "pep_rejected:template_not_in_allowlist".to_string()
        });
    }

    None
}

/// Re-validate a manager-sealed **agentic** [`ExecPlan`] by re-running the shared
/// command classifier over the daemon-only `validation_input` envelope. Returns
/// the model-safe rejection reason on the first failure, or `None` when the plan
/// passes. Order: common checks (blocklist, risk ceiling) → re-classification.
///
/// The agentic plan was sealed at the manager from a per-turn classification of
/// this exact input (built-in **or** operator template, clamped per-turn limits +
/// the input's cwd), which the fixed fleet render cannot reproduce. So instead of
/// re-rendering a template with fleet defaults, the daemon feeds `validation_input`
/// back through [`classify_command_with_all`] with its own operator snapshot and
/// effective blocklist — the same function, the same tables the manager used — and
/// requires the result to be `ConfirmRequired` with a draft that reproduces the
/// sealed plan field-for-field. This naturally covers both the built-in and
/// operator template families and the per-turn clamped limits / cwd, and it makes
/// an in-bounds limit tamper detectable: the classifier re-derives the limits from
/// the input, so a plan whose limits were altered away from what the input yields
/// no longer matches.
///
/// Honest boundary: this defeats a tamper that alters only the sealed plan's
/// **executable / classification draft fields** (program, argv, cwd, shell, risk,
/// template_id, limits, fingerprint) — a self-consistent forgery of what would run.
/// It does not by itself vouch for the two id fields that are not on the draft
/// (`exec_request_id`, `approval_id`); those are bound separately in
/// [`handle_edge_exec_request_inbound`] (frame-id match + non-empty approval token),
/// and their values remain transport-trusted manager metadata, not an independent
/// cryptographic commitment. Nor is it a commitment against a manager that alters
/// `validation_input` and `plan` in lockstep — that is the same trust level as the
/// fleet path, where the manager is the PDP.
fn validate_agentic_edge_exec(
    plan: &ExecPlan,
    validation_input: &desk_agent_protocol::ExecInput,
    max_risk: desk_agent_protocol::RiskLevel,
    templates: &[desk_agent_protocol::command_template::SyncedCommandTemplate],
    blocklist: &[desk_agent_protocol::command_blocklist::BlocklistRule],
) -> Option<String> {
    if let Some(reason) = pep_common_checks(plan, max_risk, blocklist) {
        return Some(reason);
    }

    let outcome = desk_diagnose_core::exec_classify::classify_command_with_all(
        validation_input,
        templates,
        blocklist,
    );
    if outcome.classification.decision != ExecDecision::ConfirmRequired {
        return Some("pep_rejected:agentic_not_executable".to_string());
    }
    let Some(expected) = outcome.draft else {
        return Some("pep_rejected:agentic_no_draft".to_string());
    };
    if !plan_matches_draft(plan, &expected) {
        return Some("pep_rejected:agentic_reclassify_drift".to_string());
    }

    None
}

/// Handle an inbound `EdgeExecRequest` from the manager. The frame has already
/// passed the proxy's source gate (Manager-only) and dedicated authz gate (which
/// unwrapped the inner [`ExecPlan`] and set `ctx.inbound_authz`). This re-
/// validates the plan (PEP) and, on success, dispatches it to the worker
/// correlated as a fleet execution; every exit emits exactly one
/// `EdgeExecResult` so the manager's pending entry always resolves.
async fn handle_edge_exec_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request_id = model.request_id.clone();

    // The dedicated authz gate sets `inbound_authz` on success; its absence here
    // is a routing fault. Reject (definitely not executed) rather than dispatch
    // an unauthorized plan.
    let Some(authz) = ctx.inbound_authz.clone() else {
        send_edge_exec_result(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                reason: "pep_rejected:missing_authorization".to_string(),
            },
        );
        return Ok(());
    };

    // The frame carries a source-tagged envelope (`Fleet` / `Agentic`), never a
    // bare `ExecPlan`: the two sources need different re-validation (fleet re-renders
    // an operator template with fixed defaults; agentic re-classifies the original
    // input). A missing tag / missing agentic input is a decode error → rejected.
    let payload = match model.get_data::<EdgeExecRequestPayload>() {
        Ok(p) => p,
        Err(e) => {
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: format!("pep_rejected:malformed_plan:{e}"),
                },
            );
            return Ok(());
        }
    };

    // Bind the plan's identifiers to the authz-validated frame. The authz block was
    // validated against `request_id` (the frame id) by the proxy gate, and the worker
    // is correlated on that same `request_id`; a plan whose own dispatch id names a
    // *different* attempt, or that carries an empty `approval_id`, is malformed — the
    // daemon must not let a plan self-report an id that diverges from the one the
    // authz proof covers, nor dispatch a plan with no approval token. The whole-draft
    // re-render can never catch these fields (they are not on the draft), so gate
    // them here.
    //
    // The frame id is bound to `execution_generation`, the per-dispatch axis, not to
    // `exec_request_id`. The task id is stable across retries by design, so requiring
    // it to equal a per-delivery frame id would force it to change on every retry and
    // collapse the two axes back into one. The task id is still checked, just for
    // presence: a plan that names no task cannot be reconciled with anything.
    {
        let plan = payload.plan();
        if plan.execution_generation != request_id {
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: "pep_rejected:execution_generation_mismatch".to_string(),
                },
            );
            return Ok(());
        }
        if plan.exec_request_id.0.is_empty() {
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: "pep_rejected:missing_exec_request_id".to_string(),
                },
            );
            return Ok(());
        }
        if plan.approval_id.0.is_empty() {
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: "pep_rejected:missing_approval_id".to_string(),
                },
            );
            return Ok(());
        }
    }

    // Exec must be runnable in this startup mode. The manager's pre-claim version
    // gate normally prevents dispatch to a daemon that cannot execute, but a PEP
    // must never assume the PDP got it right.
    if !ctx.exec_supported {
        send_edge_exec_result(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                reason: "pep_rejected:exec_unsupported_in_mode".to_string(),
            },
        );
        return Ok(());
    }

    let templates = ctx.command_templates.snapshot();
    let effective_blocklist = ctx.command_blocklist.snapshot();
    let rejection = match &payload {
        EdgeExecRequestPayload::Fleet { plan } => {
            validate_fleet_edge_exec(plan, authz.max_risk, &templates, &effective_blocklist)
        }
        EdgeExecRequestPayload::Agentic {
            plan,
            validation_input,
        } => validate_agentic_edge_exec(
            plan,
            validation_input,
            authz.max_risk,
            &templates,
            &effective_blocklist,
        ),
    };
    if let Some(reason) = rejection {
        log::warn!("[edge-exec] PEP rejected plan for request {request_id}: {reason}");
        send_edge_exec_result(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch { reason },
        );
        return Ok(());
    }

    // Drop the daemon-only `validation_input`: only the frozen `ExecPlan` argv
    // reaches the worker (the "worker never sees the command string" invariant).
    dispatch_fleet_exec_plan(ctx, &request_id, payload.into_plan()).await;
    Ok(())
}

/// Dispatch a PEP-validated fleet [`ExecPlan`] to the worker, correlated so the
/// worker's `WorkerToService::ExecResult` is relayed back to the manager as a
/// `EdgeExecResult(Executed{..})` (see the proxy's `ExecResult` handler). On a
/// send failure the plan never reached the worker, so the change definitely did
/// not run → `DispatchFailedBeforeWorker`.
async fn dispatch_fleet_exec_plan(ctx: &RouterContext, request_id: &str, plan: ExecPlan) {
    // Claim this dispatch in the ledger before the worker can start anything.
    match admit_exec(ctx, &plan).await {
        ExecAdmission::Spawn => {}
        ExecAdmission::Replay(result) => {
            let outcome = serde_json::from_str::<AgentOutcome>(&result).unwrap_or_else(|e| {
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    &format!("stored result could not be read: {e}"),
                    false,
                    true,
                ))
            });
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::Executed { outcome },
            );
            return;
        }
        ExecAdmission::AcceptedOutcomeUnknown(reason) => {
            // `ExecutionStateUnknown` rather than a pre-dispatch variant: only the
            // pre-dispatch ones assert the change did not run, and this one cannot.
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::ExecutionStateUnknown { reason },
            );
            return;
        }
        ExecAdmission::Refused(reason) => {
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::RejectedBeforeDispatch { reason },
            );
            return;
        }
        ExecAdmission::AtCapacity(reason) => {
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::HostAtCapacity { reason },
            );
            return;
        }
    }

    // Register the in-flight correlation BEFORE sending so a fast worker reply
    // cannot race ahead of the marker.
    if let Ok(mut pending) = ctx.edge_exec_pending.lock() {
        pending.insert(request_id.to_string());
    }
    let payload = ExecPlanPayload {
        request_id: request_id.to_string(),
        // No browser connection: a fleet result is routed by `request_id`, not a
        // control-end connection id.
        connection_id: None,
        plan,
        audit_source_request_id: Some(request_id.to_string()),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ExecPlan(payload))
        .await
    {
        if let Ok(mut pending) = ctx.edge_exec_pending.lock() {
            pending.remove(request_id);
        }
        // Nothing was started, so the slot is free again immediately.
        ctx.exec_capacity.release(request_id);
        send_edge_exec_result(
            &ctx.outbound_tx,
            request_id,
            EdgeExecDisposition::DispatchFailedBeforeWorker {
                reason: format!("worker unavailable: {e}"),
            },
        );
    }
}

/// Assemble the authoritative [`AgentEnvelope`] from a parsed control-end
/// operation by injecting every trusted field server-side. Pure so the
/// trusted-field-injection invariant is unit-testable.
fn build_agent_envelope(
    request_id: &str,
    operation: AgentOperation,
    reason: Option<String>,
    scope: AgentScope,
) -> AgentEnvelope {
    AgentEnvelope {
        protocol_version: ProtocolVersion::default(),
        // Server-owned: the control end's value (if any) is replaced.
        request_id: RequestId(request_id.to_string()),
        parent_task_id: None,
        // Single-machine local target. `device_id` empty until a device
        // registry assigns one; never self-reported by the control end.
        target: TargetRef::default(),
        actor: server_actor(),
        // No model caller yet (no orchestrator); a human operator
        // drove this directly.
        caller: CallerRef {
            caller_type: CallerType::Human,
            model_provider: None,
            model_name: None,
            adapter: None,
        },
        scope,
        operation,
        audit: desk_agent_protocol::AuditMeta {
            approval_id: None,
            reason,
        },
    }
}

/// Route a control-end `AgentRequest`: two-phase parse → capability
/// derivation → authorization → trusted-field stamp → typed worker IPC.
async fn handle_agent_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    // The AI read collectors expose host data beyond the remote view, so what
    // may leave this host is gated locally by the fail-closed collection policy
    // (`allow_logs` / `allow_screen`) and centrally by the authorization scope
    // below. Provider credentials live on the central brain, so there is no local
    // "gateway configured" gate here: an `AgentRequest` arrives already
    // authorized from the central link (or, off it, runs under the local read
    // scope).
    let Some(raw) = model.get_raw_data().as_ref() else {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::InvalidInput,
                "missing AgentRequest body",
                false,
                true,
            ),
        );
        return Ok(());
    };

    // Reject unknown kinds gracefully before typed parsing.
    if let Err(e) = validate_agent_request_kinds(raw) {
        emit_agent_error(ctx, model, e);
        return Ok(());
    }

    // Kinds are known → typed parse is safe.
    let request_data = match model.get_data::<AgentRequestData>() {
        Ok(d) => d,
        Err(e) => {
            emit_agent_error(
                ctx,
                model,
                agent_error(
                    AgentErrorKind::InvalidInput,
                    &format!("bad AgentRequest payload: {e}"),
                    false,
                    true,
                ),
            );
            return Ok(());
        }
    };

    // The `AgentRequest(600)` plane is **read-only, permanently**. Exec must go
    // through the `ConfirmExec` → `ResolveExec` confirm flow (which classifies,
    // requires explicit approval, and ships a sealed `ExecPlan`); it can never
    // ride the raw capability path, even once execution is wired up. Reject it
    // explicitly here regardless of `execution_mode` or prior approvals.
    if matches!(request_data.operation.input, OperationInput::Exec(_)) {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::UnsupportedCapability,
                "exec is not available on the agent-request plane; use the confirm-execution flow",
                false,
                true,
            ),
        );
        return Ok(());
    }

    // Capability is derived from the input (single source of truth). Exec is
    // already rejected above, so a `None` here is an unexpected non-exec input.
    let Some(capability) = request_data.operation.input.capability() else {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::UnsupportedCapability,
                "unsupported operation",
                false,
                true,
            ),
        );
        return Ok(());
    };

    // Authorize against the server-computed scope. On the manager link the
    // injected policy decision replaces the local default read scope (fleet
    // PDP); without it (single-machine / remote-signaling) the local read scope
    // applies.
    let scope = match &ctx.inbound_authz {
        Some(authz) => authz.scope.clone(),
        None => default_read_scope(),
    };
    if !authorize(capability, &scope.granted) {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::PermissionDenied,
                "capability not granted",
                false,
                false,
            ),
        );
        return Ok(());
    }

    // Stamp trusted fields and forward to the worker.
    let envelope = build_agent_envelope(
        &model.request_id,
        request_data.operation,
        request_data.reason,
        scope,
    );
    let payload = AgentRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: model.from_connection_id.clone(),
        envelope,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::AgentRequest(payload))
        .await
    {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::TargetOffline,
                &format!("worker unavailable: {e}"),
                true,
                true,
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
