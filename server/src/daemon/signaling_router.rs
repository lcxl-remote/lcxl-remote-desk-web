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
};
use desk_agent_protocol::edge_exec::{
    EdgeExecDisposition, EdgeExecRequestPayload, EdgeExecResultPayload,
};
use desk_agent_protocol::exec::{
    ConfirmExecData, ExecDecision, ExecEffect, ExecPlan, ExecPreview, ExecResultPayload,
    ResolveExecData,
};
use desk_agent_protocol::exec_policy::DEFAULT_OUTPUT_BYTES;

use crate::terminal_copilot::copilot_signaling_sink;
use desk_agent_protocol::exec_lifecycle::{ExecControlAction, ExecControlPayload};
use desk_agent_protocol::{
    ActorRef, ActorType, AgentEnvelope, AgentError, AgentErrorKind, AgentOperation, AgentOutcome,
    AgentRequestData, AgentScope, CallerRef, CallerType, Capability, ExecutionMode, OperationInput,
    ProtocolVersion, RequestId, TargetRef,
};
use desk_ipc_protocol::message::{
    AgentRequestPayload, ApplyMediaSettingsPayload, AudioPipelinePhase, CloseTerminalPayload,
    DeleteFilePayload, ExecCancelPayload, ExecPlanPayload, ListFilesPayload,
    ListTerminalCommandsPayload, ManagerRequestRefPayload, MediaCodec, MediaKind,
    MediaSettingsAction, ResizeTerminalPayload, SendTerminalInputPayload, ServiceToWorker,
    SetPrivateScreenVisibilityPayload, SetVirtualDisplayModePayload, StartAudioSettings,
    StartMediaPayload, StartTerminalPayload, UpdateMediaSettingsPayload,
};
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams};
use desk_signal_facade::model::media_pipeline::{MediaPipelinePhase, MediaPipelineStateData};
use desk_signal_facade::model::private_screen::SetPrivateScreenVisibilityData;
use desk_signal_facade::model::remote_session::{
    ApplyRemoteSessionSettings, AudioSettingsEffect, ConnectionSettingsEffect,
    RemoteSessionSettings, RemoteSessionSettingsApplied, RemoteSessionSettingsEffects,
    RemoteSessionSettingsFieldError, RemoteSessionSettingsRuntimeOverrides,
    SystemAudioCaptureState, SystemAudioCaptureStateData, UpdateAdaptiveVideoQuality,
    VideoSettingsEffect,
};
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{
    OfferModel, RemoteSessionPurpose, RequestRemoteModel, SessionTargetListData, SignalingModel,
    SignalingResponseState, SignalingType,
};
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalResizeData,
};
use desk_signal_facade::model::virtual_display::ChangeDisplaySettingsPayload;
use desk_signal_facade::service::response_type_for_request;
use desk_utils::error::{CustomDeskError, DeskErrorCode};
use desk_virtual_display::{VirtualDisplayMode, validate_mode};
use tokio::sync::broadcast;

use crate::daemon::pc_manager::{
    self, MediaRestartStage, MediaRestartTrigger, MediaRetryAdmission, MediaSlotLifecycle,
    PcRegistry, RestartOutcome,
};
use crate::daemon::virtual_display::{EnsureAttachedOutcome, VirtualDisplaySupervisor};
use crate::daemon::worker_manager::WorkerManager;
use crate::diagnose::DiagnoseOrchestrator;

/// Bound on how long the `RequestRemoteAccess` branch waits for the IDD to
/// finish bring-up before falling through to a capabilities-without-IDD
/// RemoteAccessInitialized response. `resolve_attach_with_backoff` schedules retries at
/// `[250, 500, 1000, 2000, 4000, 8000]` ms; with the driver already
/// loaded the first one or two attempts usually succeed (< 1 s) and
/// the post-attach `RefreshCapabilities` round-trip lands within
/// another second. 5 s covers the typical cold-bring-up while still
/// bounding browser-perceived dialog latency if the driver hangs.
const VIRTUAL_DISPLAY_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound for obtaining the current worker incarnation's real media capability
/// snapshot before creating any remote-desktop admission state.
const REMOTE_DESKTOP_CAPABILITIES_TIMEOUT: Duration = Duration::from_secs(3);
use crate::error::DeskError;
use crate::host_control::HostControlHub;
use crate::model::security_approval::{
    SecurityPermissionType, check_security_permission, effective_permission,
};
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
pub(crate) use exec_lifecycle::send_edge_execution_completed;
use exec_lifecycle::*;
use external_requests::*;
use manager_terminal::*;

/// Whether a given `SignalingType` is owned by the daemon (handled
/// inline against the PC registry) or by the worker (forwarded over
/// IPC). Pure function — easy to unit-test exhaustively.
pub fn classify(signaling_type: SignalingType) -> RouteOwnership {
    match signaling_type {
        // ---- Daemon-owned: PC / SDP / ICE / SignalingState ----
        SignalingType::RequestRemoteAccess
        | SignalingType::RemoteAccessInitialized
        | SignalingType::Offer
        | SignalingType::Answer
        | SignalingType::IceCandidate
        | SignalingType::ReleaseControl
        | SignalingType::CloseRemoteSession
        | SignalingType::ConnectionRemoved => RouteOwnership::Daemon,

        // The daemon owns SignalingState, so the per-connection
        // control-approval flow runs daemon-side (browser → daemon →
        // host_control_hub → user → daemon updates SignalingState +
        // emits ControlAccepted/ControlDenied back). Worker no longer
        // sees RequireControl in daemon-worker mode.
        SignalingType::RequireControl => RouteOwnership::Daemon,

        // Daemon-emitted reply variants for the RequireControl flow.
        // The daemon emits ControlAccepted / ControlDenied outbound to the
        // browser from `pc_manager::handle_require_control`; browsers
        // never echo them back. If a stray inbound copy arrives the
        // daemon swallows it (worker's `DeskSession::handle_message`
        // has no arm for these and would only return
        // `UNKNOWN_SIGNALING_TYPE` — bridging would just bounce
        // confusing errors back to the browser).
        SignalingType::ControlAccepted
        | SignalingType::ControlDenied
        | SignalingType::ControlReleased => RouteOwnership::Daemon,

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
        // - `AudioPlaybackFailed`: emitted from the PC's `on_track`
        //   callback; in daemon-worker mode the daemon's
        //   pc_manager does not attach an `on_track` handler so the
        //   variant is dead until that work lands. Portable mode
        //   still produces it from `service::signaling`, but that
        //   path bypasses the router entirely.
        // - `TerminalOutputProduced` / `TerminalStarted` / `TerminalClosed`:
        //   worker → browser only. Worker emits them via
        //   typed `WorkerToService::TerminalOutputProduced` /
        //   `TerminalStarted` / `TerminalClosed`; the browser never
        //   echoes them back. A stray inbound copy is a protocol
        //   error from the browser — daemon swallows it rather than
        //   bridging to the worker (which has no `handle_message`
        //   arm for these and would only return
        //   `UNKNOWN_SIGNALING_TYPE`).
        // - `AgentCapabilityCompleted`: worker → control end only. The worker
        //   emits it via typed `WorkerToService::AgentCapabilityCompleted`; the
        //   control end never echoes it back. A stray inbound copy is a
        //   protocol error — daemon swallows it.
        // - `ExecutionPreviewGenerated` / `ExecutionCompleted`: host → control end only (the
        //   confirm-execution preview and result). Daemon-emitted; a stray
        //   inbound copy is a protocol error — swallow.
        SignalingType::PrivateScreenStateChanged
        | SignalingType::PrivateScreenVisibilitySet
        | SignalingType::AudioPlaybackFailed
        | SignalingType::MediaPipelineStateChanged
        | SignalingType::MediaPipelineRetryCompleted
        | SignalingType::SystemInfoRetrieved
        | SignalingType::DisplaySettingsChanged
        | SignalingType::FilesListed
        | SignalingType::FileDeleted
        | SignalingType::TerminalCommandsListed
        | SignalingType::TerminalOutputProduced
        | SignalingType::TerminalStarted
        | SignalingType::TerminalClosed
        | SignalingType::AgentCapabilityCompleted
        | SignalingType::TerminalCopilotUpdated
        | SignalingType::TerminalCompletionsGenerated
        | SignalingType::ExecutionPreviewGenerated
        | SignalingType::ExecutionCompleted
        | SignalingType::ExecutionProgressUpdated
        | SignalingType::ExecutionStateReported
        | SignalingType::ComputerActionStarted
        | SignalingType::ComputerActionCompleted
        | SignalingType::ComputerActionStateReported
        | SignalingType::ComputerUseReadinessUpdated => RouteOwnership::Daemon,

        // Browser → daemon media control. This is a local bounded restart of
        // the already-negotiated pipeline and never enters the worker's generic
        // signaling dispatcher.
        SignalingType::RetryMediaPipeline
        | SignalingType::ApplyRemoteSessionSettings
        | SignalingType::RemoteSessionSettingsApplied
        | SignalingType::UpdateAdaptiveVideoQuality
        | SignalingType::SystemAudioCaptureStateChanged => RouteOwnership::Daemon,

        // AI Diagnose request: control end → daemon. Unlike `InvokeAgentCapability`
        // (worker-bound raw capability call), the diagnose orchestrator runs
        // daemon-side (it owns the model call + redaction + streaming), so this
        // is handled inline against the daemon's orchestrator rather than
        // forwarded over IPC.
        // DiagnoseCancel stops a run the control end abandoned by starting over;
        // handle it inline against the daemon's orchestrator, like `Diagnose`.

        // In-terminal AI copilot: control end → daemon. Like `Diagnose`, the
        // copilot orchestrator runs daemon-side (model call + redaction +
        // streaming) in Default / DeskServer, so this is handled inline rather
        // than forwarded over IPC. `TerminalCopilotCancel` dismisses an in-flight
        // turn, handled inline like `DiagnoseCancel`.
        SignalingType::AskTerminalCopilot | SignalingType::CancelTerminalCopilot => {
            RouteOwnership::Daemon
        }

        // In-terminal AI command completion: control end → daemon. Like the
        // copilot, the completion turn runs daemon-side (a single tool-free model
        // call + redaction) in Default / DeskServer, so it is handled inline
        // rather than forwarded over IPC.
        SignalingType::GenerateTerminalCompletions => RouteOwnership::Daemon,

        // AI confirmed-execution: control end → daemon. The approval state
        // machine (classify → preview → approve/reject → dispatch) lives
        // daemon-side, so these are handled inline rather than forwarded over
        // IPC, like `Diagnose`. The worker only ever receives the sealed
        // `ServiceToWorker::ExecPlan` (a later step), never these.
        SignalingType::PreviewExecution | SignalingType::ResolveExecution => RouteOwnership::Daemon,

        // Acting on a running execution needs the durable ledger and the worker
        // handle, both of which are the daemon's. The worker is told to stop a
        // command, but never asked what it knows — the ledger outlives it.
        SignalingType::ControlExecution => RouteOwnership::Daemon,

        // Computer Use owns a dedicated daemon broker/lifecycle and typed IPC,
        // never the generic worker signaling path.
        SignalingType::DispatchComputerAction
        | SignalingType::CancelComputerAction
        | SignalingType::QueryComputerActionState => RouteOwnership::Daemon,

        // Daemon-emitted notifications. Browsers don't send these
        // back at us, but if they did the daemon should swallow them
        // rather than relay to the worker (which has no PC to act on).
        SignalingType::DesktopSwitching | SignalingType::DesktopReady => RouteOwnership::Daemon,

        // AI audit event is emitted by this daemon toward the manager; it is
        // never received inbound here. Classify as daemon-owned so a stray
        // inbound frame is swallowed rather than forwarded to the worker.
        SignalingType::ReportAiAuditEvent => RouteOwnership::Daemon,

        // Command-template sync is applied to the daemon's own cache (the exec
        // classifier reads it); never forwarded to the worker.
        SignalingType::SyncCommandTemplates => RouteOwnership::Daemon,

        // Command-blocklist sync is applied to the daemon's own cache (the exec
        // classifier's Step 0 reads it); never forwarded to the worker.
        SignalingType::SyncCommandBlocklist => RouteOwnership::Daemon,

        SignalingType::CollectEvidence | SignalingType::EvidenceCollectionUpdated => {
            RouteOwnership::Daemon
        }

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
        | SignalingType::UpdateRemoteAccessLock
        | SignalingType::RemoteAccessLockUpdated
        | SignalingType::TerminateRemotePeer
        | SignalingType::RemotePeerTerminationResolved => RouteOwnership::Daemon,

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

        // Fleet batch-execution: `EdgeExecRequest` is manager → daemon (the
        // daemon PEP re-validates the manager-sealed `ExecPlan` and dispatches it
        // to the worker); handled inline against the daemon's worker, never
        // forwarded as-is. `EdgeExecResult` is daemon-emitted toward the manager
        // and never received inbound here, so a stray inbound copy is swallowed
        // daemon-side.
        SignalingType::ExecuteEdgePlan | SignalingType::EdgeExecutionCompleted => RouteOwnership::Daemon,

        // Remote read-tool RPC (§8.3): `RemoteToolRequest` is manager → daemon
        // (the daemon runs the one server-stamped read locally); handled inline,
        // never forwarded. `RemoteToolResponse` is daemon-emitted toward the
        // manager and never received inbound here, so a stray inbound copy is
        // swallowed daemon-side.
        SignalingType::InvokeRemoteTool | SignalingType::RemoteToolOutputUpdated => {
            RouteOwnership::Daemon
        }

        // Device Assistant orchestration belongs to the central brain. These
        // frames must never reach a host in the normal path; classify them as
        // daemon-owned so a legacy/plain relay cannot forward them to a worker.
        SignalingType::AskDeviceAssistant
        | SignalingType::DeviceAssistantUpdated
        | SignalingType::CancelDeviceAssistant
        | SignalingType::GetDeviceAssistantCapabilities
        | SignalingType::DeviceAssistantCapabilitiesUpdated
        | SignalingType::UpdateDeviceAssistantContext
        | SignalingType::UpdateDeviceAssistantObjectContext
        | SignalingType::DeviceAssistantContextUpdated
        | SignalingType::DeviceAssistantObjectContextUpdated => RouteOwnership::Daemon,

        // Connection-list bookkeeping is daemon state too — the
        // daemon knows about every active PC, the worker only knows
        // its own per-connection encoder set.
        SignalingType::FetchConnections | SignalingType::ConnectionsFetched => RouteOwnership::Daemon,

        // Heartbeat is a WS keepalive — not for the worker.
        SignalingType::SendHeartbeat | SignalingType::HeartbeatAcknowledged => {
            RouteOwnership::Daemon
        }

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
        SignalingType::SetPrivateScreenVisibility
        | SignalingType::GetSystemInfo
        | SignalingType::ListFiles
        | SignalingType::DeleteFile
        | SignalingType::StartTerminal
        | SignalingType::SendTerminalInput
        | SignalingType::ResizeTerminal
        | SignalingType::CloseTerminal
        | SignalingType::ListTerminalCommands
        | SignalingType::ChangeDisplaySettings
        // AI agent capability request: control end → daemon → worker.
        // The daemon two-phase-parses + stamps trusted fields, then
        // ships a typed `ServiceToWorker::InvokeAgentCapability`.
        | SignalingType::InvokeAgentCapability => RouteOwnership::Worker,

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
    /// Exact upstream lane for admission provenance. Manager is set only by the
    /// manager connection loop and never inferred from `TrustedCentral`.
    pub admission_origin: pc_manager::AdmissionOrigin,
    /// Credential scope bound to the current manager WebSocket. `None` on local
    /// and bare remote-signaling lanes.
    pub manager_credential_link:
        Option<crate::daemon::manager_credential_scope::ManagerCredentialLink>,
    /// The exact upstream WebSocket carrying a prepared one-shot PTY. `None` in
    /// base/test contexts and set per live signaling connection; never durable.
    pub exec_pty_link: Option<crate::daemon::exec_pty_carrier::ExecPtyLinkContext>,
    pub outbound_tx: broadcast::Sender<String>,
    pub settings: web::Data<SharedSettings>,
    /// What the daemon-side permission gates read. Backed by the host's
    /// settings coordinator, so a gate and a settings update can never disagree
    /// about the policy.
    pub policy: Arc<crate::model::policy_access::PolicyAccess>,
    pub host_control_hub: Arc<HostControlHub>,
    /// handle_request_remote reads `worker_capabilities` from
    /// here to populate the RemoteAccessInitialized response, and handle_offer issues
    /// `ServiceToWorker::StartMedia` through it once the SDP exchange
    /// completes (so the worker knows to spin up the per-connection
    /// encoder).
    pub worker_mgr: WorkerManager,
    /// `Some(...)` only in service-daemon mode. Default / signaling
    /// / desk-server-only modes leave this `None`, so the
    /// `ChangeDisplaySettings` route always replies with
    /// `FEATURE_UNAVAILABLE` outside service mode.
    pub virtual_display: Option<Arc<VirtualDisplaySupervisor>>,
    /// Enterprise fleet evidence collector.
    pub diagnose_orchestrator: Option<Arc<DiagnoseOrchestrator>>,
    /// `Some(...)` in modes with an in-process worker (Default / DeskServer),
    /// where the host can collect read-only evidence locally. AI diagnosis is
    /// orchestrated by the central signaling brain, so this serves the
    /// remote-collect edge path (`collect_for_remote`): the central server pushes a
    /// `CollectRequest` and this host streams the redacted evidence back. `None` in
    /// ServiceDaemon mode, where a `CollectRequest` replies with a wholesale error.
    /// Serves a manager remote read tool call (§8.3) against the in-process device
    /// agent. Present in the same modes as `diagnose_orchestrator` (an in-process
    /// worker can read locally); `None` in ServiceDaemon, where a `RemoteToolRequest`
    /// replies with a wholesale error.
    pub remote_read: Option<Arc<crate::agent_adapter::remote_read::EdgeReadInvoker>>,
    /// Whether direct confirmed execution is available in this startup mode. `true`
    /// only where an in-process worker can execute (Default / DeskServer). Trusted-
    /// central `EdgeExecRequest` has a separate ServiceDaemon resident-worker gate.
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
    /// Manager-injected authorization for the current inbound AI frame, set per
    /// call by the proxy when a validated `AuthorizedControlPayload` arrives on
    /// the Manager link. `None` on the local / remote-signaling links, where the
    /// AI handlers fall back to local-config gating (no fleet PDP). Threaded
    /// through the context (rather than the handler signatures) so the existing
    /// `route()` / handler call sites stay untouched.
    pub inbound_authz: Option<desk_agent_protocol::authz::AuthorizationBlock>,
    /// The validated capability-ceiling stamp for the current inbound
    /// `RequestRemoteAccess`, set per call by the proxy after `gate_request_remote_frame`
    /// accepts a wrapped frame on the trusted-central link. `None` for a bare
    /// (non-central) request or a non-`RequestRemoteAccess` frame. A `Some` whose
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
    /// WS is a distinct connection that never does a `RequestRemoteAccess`).
    pub inbound_start_terminal_authz:
        Option<desk_signal_facade::model::request_remote_authz::RequestRemoteAuthz>,
    /// Per-attempt `request_id`s of fleet executions currently dispatched to the
    /// worker. When the worker replies with `WorkerToService::ExecutionCompleted` whose
    /// `request_id` is in this set, the proxy emits a `EdgeExecResult(614)`
    /// toward the manager instead of an `ExecutionCompleted(609)` toward a browser.
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

async fn current_exec_pty_capabilities(
    settings: &web::Data<SharedSettings>,
) -> crate::worker::exec_pty::ExecPtyCapabilities {
    let settings = settings.read().await;
    crate::worker::exec_pty::effective_capabilities(&settings.ai_policy)
}

fn pty_dispatch_refusal(
    plan: &desk_agent_protocol::exec::ExecPlan,
    capabilities: crate::worker::exec_pty::ExecPtyCapabilities,
) -> Option<&'static str> {
    if !plan.io_mode.is_pty() {
        None
    } else if !capabilities.exec_pty {
        Some("interactive execution is not enabled on this host")
    } else if plan.requires_root_pty_containment() && !capabilities.exec_pty_elevation {
        Some("interactive elevation is not enabled on this host")
    } else {
        None
    }
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

fn emit_session_target_error(
    ctx: &RouterContext,
    model: &SignalingModel,
    capability: crate::daemon::session_target::SessionCapability,
    error: crate::daemon::session_target::SessionTargetSelectionError,
) {
    let code = match error {
        crate::daemon::session_target::SessionTargetSelectionError::Unavailable => {
            DeskErrorCode::SESSION_UNAVAILABLE
        }
        crate::daemon::session_target::SessionTargetSelectionError::SelectionRequired => {
            DeskErrorCode::SESSION_SELECTION_REQUIRED
        }
        crate::daemon::session_target::SessionTargetSelectionError::Stale => {
            DeskErrorCode::SESSION_TARGET_STALE
        }
    };
    let (revision, targets) = ctx.worker_mgr.session_targets().list_for(capability);
    let data = SessionTargetListData { revision, targets };
    let response_type =
        response_type_for_request(model.signaling_type).unwrap_or(SignalingType::Error);
    let response = SignalingModel::new_response(
        &model.request_id,
        response_type,
        None,
        model.from_connection_id.clone(),
        Some(&data),
        SignalingResponseState {
            error_code: code.code(),
            message: Some(error.to_string()),
        },
    );
    match response.and_then(|response| serde_json::to_string(&response).map_err(Into::into)) {
        Ok(text) => {
            let _ = ctx.outbound_tx.send(text);
        }
        Err(error) => log::warn!("[router] failed to encode session target error: {error}"),
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
        if matches!(
            model.signaling_type,
            SignalingType::ApplyRemoteSessionSettings | SignalingType::UpdateAdaptiveVideoQuality
        ) {
            emit_standard_error_response(
                ctx,
                model,
                DeskErrorCode::PERMISSION_ERROR,
                "This connection is not permitted to perform the requested action",
            );
        }
        return Ok(());
    }
    match model.signaling_type {
        SignalingType::RequestRemoteAccess => {
            let request_remote = model
                .get_data::<RequestRemoteModel>()
                .map_err(DeskError::from)?;
            let connection_id = model
                .check_and_get_from_connection_id()
                .map_err(DeskError::from)?;
            let session_capability = match request_remote.purpose {
                RemoteSessionPurpose::RemoteDesktop => {
                    crate::daemon::session_target::SessionCapability::RemoteDesktop
                }
                RemoteSessionPurpose::FileManager => {
                    crate::daemon::session_target::SessionCapability::FileManager
                }
            };
            let selected_session = match ctx.worker_mgr.resolve_session_target(
                session_capability,
                request_remote.session_target_id.as_deref(),
            ) {
                Ok(session) => session,
                Err(error) => {
                    emit_session_target_error(ctx, model, session_capability, error);
                    return Ok(());
                }
            };
            let s = ctx.settings.read().await.clone();
            // Block on virtual display attach BEFORE assembling the
            // RemoteAccessInitialized response so the daemon's capabilities cache reflects
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
                             {VIRTUAL_DISPLAY_ATTACH_TIMEOUT:?}; RemoteAccessInitialized response will omit \
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

            let capabilities = if let Some(session) = selected_session.as_ref() {
                ctx.worker_mgr.session_worker_capabilities(session).await
            } else if request_remote.purpose == RemoteSessionPurpose::RemoteDesktop {
                Some(
                    ctx.worker_mgr
                        .wait_current_worker_capabilities(
                            REMOTE_DESKTOP_CAPABILITIES_TIMEOUT,
                        )
                        .await
                        .ok_or_else(|| {
                            DeskError::CustomError(CustomDeskError::new(
                                DeskErrorCode::REMOTE_DESKTOP_CAPABILITIES_NOT_READY,
                                "the current desktop worker has not published media capabilities yet",
                            ))
                        })?,
                )
            } else {
                ctx.worker_mgr.worker_capabilities()
            };
            if selected_session.is_some() && capabilities.is_none()
            {
                emit_session_target_error(
                    ctx,
                    model,
                    session_capability,
                    crate::daemon::session_target::SessionTargetSelectionError::Unavailable,
                );
                return Ok(());
            }

            // Capability readiness is checked before creating manager admission,
            // PC, pending-request, or HostActivity residue.
            let manager_permit = if let Some(link) = ctx.manager_credential_link.as_ref() {
                match link.begin_admission(connection_id).await {
                    Ok(permit) => Some(permit),
                    Err(
                        crate::daemon::manager_credential_scope::AdmissionRejection::AwaitingProof,
                    ) => {
                        send_manager_admission_retry(ctx, model);
                        return Ok(());
                    }
                    Err(crate::daemon::manager_credential_scope::AdmissionRejection::Terminal) => {
                        return Ok(());
                    }
                }
            } else {
                None
            };
            // Hold a pending guard for the lifetime of PC creation so cleanup_pc
            // on a concurrently-closing old PC cannot N→0 detach the IDD out
            // from under us.
            let pending_guard = ctx.pc_registry.enter_pending();

            let user_name = "worker_node".to_string();
            let has_tauri = ctx.host_control_hub.has_tauri_ui();
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
            let admission = match access_ceiling.as_ref() {
                Some(ceiling) => pc_manager::Admission::Capped(ceiling.clone()),
                None => pc_manager::Admission::OwnerFull,
            };
            ctx.host_control_hub.host_activity().ensure_session(
                connection_id,
                ctx.inbound_request_remote_authz
                    .as_ref()
                    .map(|authz| authz.actor.clone())
                    .unwrap_or_else(
                        desk_signal_facade::model::request_remote_authz::ActorSummary::unknown,
                    ),
            );
            if let Some(session) = selected_session.as_ref()
                && let Err(error) = ctx
                    .worker_mgr
                    .bind_connection_target(connection_id, session)
            {
                emit_error_response(
                    ctx,
                    model,
                    DeskErrorCode::INVALID_STATE,
                    &error,
                );
                return Ok(());
            }
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

            if result.is_err() {
                ctx.worker_mgr.clear_connection_target(connection_id);
            }

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
                && ctx.pc_registry.admission(connection_id).await.is_none()
            {
                log::info!(
                    "[router] post-RequestRemoteAccess cleanup: registry empty and no pending; \
                     detaching virtual display"
                );
                if let Err(e) = supervisor.apply(false).await {
                    log::warn!("[router] post-RequestRemoteAccess cleanup detach failed: {e}");
                }
            }

            result?;
            ctx.pc_registry
                .record_admission_with_origin(
                    connection_id,
                    admission,
                    ctx.admission_origin.clone(),
                )
                .await;
            if let Some(permit) = manager_permit
                && !permit.commit().await
            {
                pc_manager::force_disconnect_connection(
                    &ctx.pc_registry,
                    &ctx.worker_mgr,
                    ctx.virtual_display.as_ref(),
                    connection_id,
                    "manager-credential-admission-fenced",
                )
                .await;
            }
            Ok(())
        }
        SignalingType::Offer => {
            let offer = model.get_data::<OfferModel>().map_err(DeskError::from)?;
            if offer.offer.sdp.contains("m=video") {
                promote_desktop_resources(model, ctx, "video offer").await?;
            }
            match pc_manager::handle_offer(
                &ctx.pc_registry,
                &ctx.outbound_tx,
                &ctx.worker_mgr,
                model,
            )
            .await
            {
                Ok(()) => {
                    if let Some(connection_id) = model.from_connection_id.clone() {
                        desktop_settings::spawn_initial_audio_authorization(ctx, connection_id);
                    }
                    Ok(())
                }
                Err(DeskError::CustomError(error))
                    if is_offer_business_error(error.error_code) =>
                {
                    let response = SignalingModel::error(
                        &model.request_id,
                        SignalingType::Offer,
                        None,
                        model.from_connection_id.clone(),
                        error.error_code,
                        &error.message,
                    );
                    if let Ok(response) = response
                        && let Ok(text) = serde_json::to_string(&response)
                    {
                        let _ = ctx.outbound_tx.send(text);
                    }
                    Ok(())
                }
                Err(error) => Err(error.into()),
            }
        }
        SignalingType::IceCandidate => {
            pc_manager::handle_ice_candidate(&ctx.pc_registry, model).await?;
            Ok(())
        }
        SignalingType::ReleaseControl => {
            let outcome = pc_manager::handle_release_control(
                &ctx.pc_registry,
                &ctx.outbound_tx,
                model,
            )
            .await?;
            ctx.host_control_hub
                .host_activity()
                .set_remote_control(&outcome.connection_id, false);
            pc_manager::hide_private_screen_best_effort(
                &ctx.worker_mgr,
                &outcome.connection_id,
                "control_released",
            )
            .await;
            update_exclusive_after_control_change(ctx, &outcome).await;
            Ok(())
        }
        SignalingType::CloseRemoteSession => {
            // Closing the session revokes any session-scoped exec approvals the
            // connection accrued in SessionApproved mode.
            revoke_session_approvals(ctx, model);
            pc_manager::handle_close_remote_session(
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
            if !outcome.accept_control {
                pc_manager::hide_private_screen_best_effort(
                    &ctx.worker_mgr,
                    &outcome.connection_id,
                    "control_denied",
                )
                .await;
            }
            update_exclusive_after_control_change(ctx, &outcome).await;
            Ok(())
        }
        SignalingType::RetryMediaPipeline => handle_retry_media_pipeline(ctx, model).await,
        // Daemon-emitted or dead inbound; the browser should never
        // send these at us but if it does, swallow rather than
        // routing onward. See classify() doc-comments for per-variant
        // rationale. `Error` and `Unknown` are in this group too (they
        // used to be worker-bound for verbose logging, but since the
        // bridge is gone there is no point round-tripping them).
        SignalingType::Answer
        | SignalingType::RemoteAccessInitialized
        | SignalingType::ControlAccepted
        | SignalingType::ControlDenied
        | SignalingType::ControlReleased
        | SignalingType::PrivateScreenStateChanged
        | SignalingType::PrivateScreenVisibilitySet
        | SignalingType::AudioPlaybackFailed
        | SignalingType::MediaPipelineStateChanged
        | SignalingType::MediaPipelineRetryCompleted
        | SignalingType::RemoteSessionSettingsApplied
        | SignalingType::SystemAudioCaptureStateChanged
        | SignalingType::SystemInfoRetrieved
        | SignalingType::DisplaySettingsChanged
        | SignalingType::FilesListed
        | SignalingType::FileDeleted
        | SignalingType::TerminalCommandsListed
        | SignalingType::TerminalOutputProduced
        | SignalingType::TerminalStarted
        | SignalingType::TerminalClosed
        | SignalingType::DesktopSwitching
        | SignalingType::DesktopReady
        | SignalingType::FetchConnections
        | SignalingType::ConnectionsFetched
        | SignalingType::SendHeartbeat
        | SignalingType::HeartbeatAcknowledged
        // AgentCapabilityCompleted only flows worker → control end; an inbound
        // copy is a protocol error — swallow it.
        | SignalingType::AgentCapabilityCompleted
        // DiagnoseEvent only flows host → control end (streamed); an
        // inbound copy is a protocol error — swallow it.
        // TerminalCopilotEvent only flows host → control end (streamed); an
        // inbound copy is a protocol error — swallow it.
        | SignalingType::TerminalCopilotUpdated
        // TerminalCompleteResult only flows host → control end; an inbound copy
        // is a protocol error — swallow it.
        | SignalingType::TerminalCompletionsGenerated
        // ExecPreview / ExecutionCompleted and the lifecycle frames only flow host →
        // control end; an inbound copy is a protocol error — swallow it.
        | SignalingType::ExecutionPreviewGenerated
        | SignalingType::ExecutionCompleted
        | SignalingType::ExecutionProgressUpdated
        | SignalingType::ExecutionStateReported
        | SignalingType::UpdateRemoteAccessLock
        | SignalingType::RemoteAccessLockUpdated
        | SignalingType::TerminateRemotePeer
        | SignalingType::RemotePeerTerminationResolved
        | SignalingType::Error
        | SignalingType::Unknown => {
            log::trace!(
                "[router] daemon-emitted or unknown variant arrived inbound, dropping: {:?}",
                model.signaling_type,
            );
            Ok(())
        }
        SignalingType::SetPrivateScreenVisibility => {
            handle_set_private_screen_visibility_inbound(ctx, model).await
        }
        SignalingType::ApplyRemoteSessionSettings => {
            handle_apply_remote_session_settings_inbound(ctx, model).await
        }
        SignalingType::UpdateAdaptiveVideoQuality => {
            handle_update_adaptive_video_quality_inbound(ctx, model).await
        }
        // Manager-plane typed-IPC dispatch.
        SignalingType::GetSystemInfo => handle_manager_system_info_inbound(ctx, model).await,
        SignalingType::ListFiles => handle_manager_file_list_inbound(ctx, model).await,
        SignalingType::DeleteFile => handle_manager_file_delete_inbound(ctx, model).await,
        // Terminal-plane typed-IPC dispatch.
        SignalingType::StartTerminal => handle_start_terminal_inbound(ctx, model).await,
        SignalingType::SendTerminalInput => handle_send_data_to_terminal_inbound(ctx, model).await,
        SignalingType::ResizeTerminal => handle_resize_terminal_inbound(ctx, model).await,
        SignalingType::CloseTerminal => handle_close_terminal_inbound(ctx, model).await,
        SignalingType::ListTerminalCommands => handle_list_terminal_inbound(ctx, model).await,
        // Virtual display integration: browser → daemon ChangeDisplaySettings.
        // Daemon validates input, surfaces error responses for the
        // un-routable cases (FEATURE_UNAVAILABLE / INVALID_PARAMS /
        // REMOTE_DESK_OFFLINE / INVALID_STATE), and forwards a typed
        // SetVirtualDisplayMode IPC only when the supervisor is active.
        SignalingType::ChangeDisplaySettings => {
            handle_change_display_settings_inbound(ctx, model).await
        }
        // AI agent capability request: two-phase parse + trusted-field
        // stamp, then ship a typed `ServiceToWorker::InvokeAgentCapability`.
        SignalingType::InvokeAgentCapability => handle_invoke_agent_capability_inbound(ctx, model).await,
        // AI Diagnose: run the daemon-side orchestrator (Default / DeskServer)
        // or reply `FEATURE_UNAVAILABLE` (ServiceDaemon, where the orchestrator
        // is not injected). Streams `DiagnoseEvent` frames back to the browser.
        // AI Diagnose cancellation: stop the run abandoned by a UI start-over and
        // record an `ai.task.cancelled` audit; no `DiagnoseEvent` is streamed back.
        // In-terminal AI copilot: run the daemon-side orchestrator (Default /
        // DeskServer) or reply `FEATURE_UNAVAILABLE` (ServiceDaemon, where the
        // orchestrator is not injected). Streams `TerminalCopilotEvent` frames
        // back to the control end.
        SignalingType::AskTerminalCopilot => handle_terminal_copilot_inbound(ctx, model).await,
        // Copilot dismissal: a UI-side action with no orchestrator state branch
        // yet; recorded as a no-op cancellation, like `DiagnoseCancel`.
        SignalingType::CancelTerminalCopilot => Ok(()),
        // In-terminal AI command completion: run the daemon-side single-shot
        // completion (Default / DeskServer) or reply with an error result
        // (ServiceDaemon, where the runtime is not injected). Answers with one
        // `TerminalCompleteResult` frame back to the control end.
        SignalingType::GenerateTerminalCompletions => handle_terminal_complete_inbound(ctx, model).await,
        // AI confirmed-execution: classify the command, store an immutable
        // pending approval, and stream an `ExecPreview` back (Default /
        // DeskServer) or reply `UnsupportedCapability` (ServiceDaemon).
        SignalingType::PreviewExecution => handle_confirm_exec_inbound(ctx, model).await,
        // AI confirmed-execution: consume a pending approval and (on approve)
        // dispatch the sealed plan. The execution itself + outbound
        // `ExecutionCompleted` land with the worker executor in a later step.
        SignalingType::ResolveExecution => handle_resolve_exec_inbound(ctx, model).await,
        SignalingType::ControlExecution => handle_exec_control_inbound(ctx, model).await,
        SignalingType::DispatchComputerAction => handle_computer_action_inbound(ctx, model).await,
        SignalingType::CancelComputerAction => handle_computer_action_cancel_inbound(ctx, model).await,
        SignalingType::QueryComputerActionState => {
            emit_standard_error_response(
                ctx,
                model,
                DeskErrorCode::FEATURE_UNAVAILABLE,
                "Computer Use broker is not enabled in this build",
            );
            Ok(())
        }
        SignalingType::ComputerActionStarted
        | SignalingType::ComputerActionCompleted
        | SignalingType::ComputerActionStateReported
        | SignalingType::ComputerUseReadinessUpdated => Ok(()),
        // AI audit events are emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never persists audit itself).
        SignalingType::ReportAiAuditEvent => Ok(()),
        // Command-template sync from the manager. The source gate
        // (`handle_inbound_signaling_text`) has already dropped any non-Manager
        // origin before reaching here; this only applies the validated set.
        SignalingType::SyncCommandTemplates => handle_command_template_sync_inbound(ctx, model),
        // Command-blocklist sync from the manager. The source gate
        // (`handle_inbound_signaling_text`) has already dropped any non-central
        // origin before reaching here; this only applies the validated set.
        SignalingType::SyncCommandBlocklist => handle_command_blocklist_sync_inbound(ctx, model),
        SignalingType::CollectEvidence => handle_collect_request_inbound(ctx, model).await,
        SignalingType::EvidenceCollectionUpdated => Ok(()),
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
        // CollectResponse is emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never consumes its own stream).
        // Fleet batch-execution request from the manager: PEP re-validate the
        // manager-sealed `ExecPlan` and dispatch it to the worker, correlating
        // the worker's result back to the manager as a `EdgeExecResult`. The
        // source gate + dedicated authz gate (`signaling_proxy`) have already
        // dropped non-Manager origins and unwrapped/validated the authorization.
        SignalingType::ExecuteEdgePlan => handle_edge_exec_request_inbound(ctx, model).await,
        // EdgeExecResult is emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never consumes its own replies).
        SignalingType::EdgeExecutionCompleted => Ok(()),
        // Remote read-tool request from the manager (agentic loop running
        // centrally): run the one server-stamped read locally and stream a chunked
        // RemoteToolResponse back. The source gate has already dropped any
        // non-Manager origin before reaching here.
        SignalingType::InvokeRemoteTool => handle_remote_tool_request_inbound(ctx, model).await,
        // RemoteToolResponse is emitted by this daemon toward the manager; a stray
        // inbound frame is swallowed (the daemon never consumes its own stream).
        SignalingType::RemoteToolOutputUpdated => Ok(()),
        // Central-orchestrator-only Device Assistant frames are never executed
        // by the edge. Swallow a stray/legacy-relayed copy fail closed.
        SignalingType::AskDeviceAssistant
        | SignalingType::DeviceAssistantUpdated
        | SignalingType::CancelDeviceAssistant
        | SignalingType::GetDeviceAssistantCapabilities
        | SignalingType::DeviceAssistantCapabilitiesUpdated
        | SignalingType::UpdateDeviceAssistantContext
        | SignalingType::UpdateDeviceAssistantObjectContext
        | SignalingType::DeviceAssistantContextUpdated
        | SignalingType::DeviceAssistantObjectContextUpdated => {
            log::warn!("[router] dropped central-only Device Assistant frame at edge");
            Ok(())
        }
    }
}

fn is_offer_business_error(code: DeskErrorCode) -> bool {
    matches!(
        code,
        DeskErrorCode::INVALID_PARAMS
            | DeskErrorCode::FEATURE_UNAVAILABLE
            | DeskErrorCode::VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED
            | DeskErrorCode::VIDEO_PIPELINE_RENEGOTIATION_REQUIRED
    )
}

async fn handle_retry_media_pipeline(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = model.from_connection_id.as_deref() else {
        send_media_retry_error(
            ctx,
            model,
            DeskErrorCode::INVALID_PARAMS,
            "RetryMediaPipeline requires a source connection",
        );
        return Ok(());
    };
    let payload = match model
        .get_data::<desk_signal_facade::model::remote_session::ConnectionEpochPayload>()
    {
        Ok(payload) => payload,
        Err(error) => {
            send_media_retry_error(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                &format!("bad RetryMediaPipeline payload: {error}"),
            );
            return Ok(());
        }
    };
    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        send_media_retry_error(
            ctx,
            model,
            DeskErrorCode::CLIENT_ID_NOT_FOUND,
            "media connection no longer exists",
        );
        return Ok(());
    };
    if pc.read().await.connection_epoch != payload.connection_epoch {
        return Ok(());
    }

    match ctx
        .pc_registry
        .claim_media_pipeline_retry(connection_id, &model.request_id)
        .await
    {
        MediaRetryAdmission::Duplicate => return Ok(()),
        MediaRetryAdmission::UnknownConnection => {
            send_media_retry_error(
                ctx,
                model,
                DeskErrorCode::CLIENT_ID_NOT_FOUND,
                "media connection no longer exists",
            );
            return Ok(());
        }
        MediaRetryAdmission::NotRetryable => {
            send_media_retry_error(
                ctx,
                model,
                DeskErrorCode::INVALID_STATE,
                "media pipeline is not blocked or failed",
            );
            return Ok(());
        }
        MediaRetryAdmission::RequiresRenegotiation => {
            send_media_retry_error(
                ctx,
                model,
                DeskErrorCode::VIDEO_PIPELINE_RENEGOTIATION_REQUIRED,
                "media retry requires choosing an encoder and sending a fresh offer",
            );
            return Ok(());
        }
        MediaRetryAdmission::Accepted => {}
    }

    let outcome = ctx
        .pc_registry
        .restart_media_from_cached_payload(
            connection_id,
            &ctx.worker_mgr,
            MediaRestartTrigger::UserRetry,
        )
        .await;
    match outcome {
        RestartOutcome::Restarted => {
            send_media_retry_success(ctx, model);
            Ok(())
        }
        RestartOutcome::NoCachedPayload { left_paused } => {
            let message =
                format!("media retry requires a fresh offer; connection left_paused={left_paused}");
            publish_media_pipeline_state(
                ctx,
                connection_id,
                MediaPipelinePhase::Blocked,
                DeskErrorCode::VIDEO_PIPELINE_RENEGOTIATION_REQUIRED,
                message.clone(),
            )
            .await;
            send_media_retry_error(
                ctx,
                model,
                DeskErrorCode::VIDEO_PIPELINE_RENEGOTIATION_REQUIRED,
                &message,
            );
            Ok(())
        }
        RestartOutcome::Failed { stage } => {
            let message = format!("media retry failed during {}", restart_stage_name(stage));
            publish_media_pipeline_state(
                ctx,
                connection_id,
                MediaPipelinePhase::Failed,
                DeskErrorCode::VIDEO_PIPELINE_RESTART_FAILED,
                message.clone(),
            )
            .await;
            send_media_retry_error(
                ctx,
                model,
                DeskErrorCode::VIDEO_PIPELINE_RESTART_FAILED,
                &message,
            );
            Ok(())
        }
    }
}

const fn restart_stage_name(stage: MediaRestartStage) -> &'static str {
    match stage {
        MediaRestartStage::UnknownConnection => "connection lookup",
        MediaRestartStage::StartMedia => "StartMedia",
        MediaRestartStage::ForceKeyframe => "ForceKeyframe",
    }
}

async fn publish_media_pipeline_state(
    ctx: &RouterContext,
    connection_id: &str,
    phase: MediaPipelinePhase,
    reason_code: DeskErrorCode,
    message: String,
) {
    let data = MediaPipelineStateData {
        phase,
        encoder: None,
        source_resolution: None,
        compatible_encoders: Vec::new(),
        reason_code: Some(reason_code),
        message: Some(message),
    };
    if let Some(pc) = ctx.pc_registry.get(connection_id).await {
        let pc = pc.read().await;
        let connection_epoch = pc.connection_epoch.clone();
        let generation = pc
            .cached_start_media
            .read()
            .await
            .as_ref()
            .map_or(0, |payload| payload.video_generation);
        drop(pc);
        ctx.pc_registry
            .record_media_pipeline_state(connection_id, &connection_epoch, generation, data.clone())
            .await;
    }
    if let Ok(model) = SignalingModel::new_request(
        SignalingType::MediaPipelineStateChanged,
        Some(connection_id.to_string()),
        Some(&data),
    ) && let Ok(text) = serde_json::to_string(&model)
    {
        let _ = ctx.outbound_tx.send(text);
    }
}

fn send_media_retry_error(
    ctx: &RouterContext,
    model: &SignalingModel,
    code: DeskErrorCode,
    message: &str,
) {
    let response = SignalingModel::error(
        &model.request_id,
        SignalingType::MediaPipelineRetryCompleted,
        None,
        model.from_connection_id.clone(),
        code,
        message,
    );
    if let Ok(response) = response
        && let Ok(text) = serde_json::to_string(&response)
    {
        let _ = ctx.outbound_tx.send(text);
    }
}

fn send_media_retry_success(ctx: &RouterContext, model: &SignalingModel) {
    let response = SignalingModel::success_response::<()>(
        &model.request_id,
        SignalingType::MediaPipelineRetryCompleted,
        None,
        model.from_connection_id.clone(),
        None,
    );
    if let Ok(response) = response
        && let Ok(text) = serde_json::to_string(&response)
    {
        let _ = ctx.outbound_tx.send(text);
    }
}

fn send_manager_admission_retry(ctx: &RouterContext, model: &SignalingModel) {
    let response_type =
        response_type_for_request(model.signaling_type).unwrap_or(SignalingType::Error);
    let response = SignalingModel::error(
        &model.request_id,
        response_type,
        None,
        model.from_connection_id.clone(),
        DeskErrorCode::ACTION_NEED_RETRY,
        "Manager credential verification is temporarily unavailable",
    );
    if let Ok(response) = response
        && let Ok(text) = serde_json::to_string(&response)
    {
        let _ = ctx.outbound_tx.send(text);
    }
}

fn allowed_for_tombstoned_connection(signaling_type: SignalingType) -> bool {
    matches!(
        signaling_type,
        SignalingType::CloseRemoteSession
            | SignalingType::ConnectionRemoved
            | SignalingType::CloseTerminal
    )
}

fn allowed_while_remote_access_locked(signaling_type: SignalingType) -> bool {
    matches!(
        signaling_type,
        SignalingType::ReleaseControl
            | SignalingType::CloseRemoteSession
            | SignalingType::ConnectionRemoved
            | SignalingType::CloseTerminal
            | SignalingType::SyncCommandTemplates
            | SignalingType::SyncCommandBlocklist
            | SignalingType::SupportCodeIssued
            | SignalingType::RevokeAccessGrant
            | SignalingType::RemoteAccessLockUpdated
            | SignalingType::RemotePeerTerminationResolved
            | SignalingType::CancelComputerAction
    )
}

#[cfg(test)]
mod tests;
