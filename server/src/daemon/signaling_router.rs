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
//! The typed IPC path has no transitional
//! `ServiceToWorker::SignalingMessage` / `WorkerToService::SignalingMessage`
//! variants or `RouteOutcome::ForwardToWorker` fallback. `route`
//! never falls back: every inbound `SignalingType`
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
    ManagerFileListRequestPayload, ManagerRequestRefPayload, ResizeTerminalPayload,
    SendDataToTerminalPayload, ServiceToWorker, SetVirtualDisplayModePayload,
    StartTerminalRequestPayload, UpdateDeskSettingsPayload,
};
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams};
use desk_signal_facade::model::private_screen::EnablePrivateScreenData;
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{
    OfferModel, RemoteSessionPurpose, RequestRemoteModel, SignalingModel, SignalingType,
};
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

mod access_policy;
mod agent_protocol;
mod desktop_settings;
mod edge_exec;
mod exec_lifecycle;
mod external_requests;
mod manager_terminal;

use access_policy::*;
pub(crate) use agent_protocol::send_exec_preview;
use agent_protocol::*;
use desktop_settings::*;
pub use desktop_settings::{compute_desired_with_active, update_exclusive_after_control_change};
use edge_exec::*;
pub(crate) use exec_lifecycle::send_edge_exec_result;
use exec_lifecycle::*;
use external_requests::*;
use manager_terminal::*;

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
        // DiagnoseCancel stops a run the control end abandoned by starting over;
        // handle it inline against the daemon's orchestrator, like `Diagnose`.
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
        // helpers. The legacy `SignalingMessage` bridge does not
        // exist.
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
    /// What the daemon-side permission gates read. Backed by the host's
    /// settings coordinator, so a gate and a settings update can never disagree
    /// about the policy.
    pub policy: Arc<crate::model::policy_access::PolicyAccess>,
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

            // Post-ensure cleanup: if handle_request_remote
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
            let outcome = pc_manager::handle_require_control(
                &ctx.pc_registry,
                &ctx.outbound_tx,
                &ctx.policy,
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
        SignalingType::ManagerFileList => handle_manager_file_list_inbound(ctx, model).await,
        SignalingType::ManagerFileDelete => handle_manager_file_delete_inbound(ctx, model).await,
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
        // AI Diagnose cancellation: stop the run abandoned by a UI start-over and
        // record an `ai.task.cancelled` audit; no `DiagnoseEvent` is streamed back.
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

#[cfg(test)]
mod tests;
