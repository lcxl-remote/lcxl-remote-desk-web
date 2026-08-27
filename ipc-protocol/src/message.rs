use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[cfg(doc)]
use desk_signal_facade::model::system_info::SystemInfo;
#[cfg(doc)]
use desk_signal_facade::model::terminal::TerminalList;

/// Messages sent from Service Core (daemon) to Worker process over the
/// **event** transport (low-latency, never-drop). Large media payloads do
/// not travel here — they go on the dedicated media transport (see
/// [`MediaFrame`]).
///
/// Each variant that drives a specific peer connection carries a
/// `connection_id` field so the worker can route to the right
/// per-connection encoder / state. ID-less variants (`Init`, `Shutdown`,
/// etc.) are worker-process-wide.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(tag = "type", content = "payload")]
pub enum ServiceToWorker {
    /// Initialize the worker with session and configuration info
    Init(WorkerInitPayload),

    /// Force the worker to shut down immediately.
    Shutdown,

    /// Apply the daemon-authoritative remote-access gate and cancel worker-owned
    /// activity when transitioning to locked.
    SetRemoteAccessState(RemoteAccessStatePayload),

    // ---------- Media control (event pipe) ----------
    /// Start a per-connection media pipeline (capture + per-connection
    /// video/audio encoder). Capture is shared across connections; the
    /// encoder is exclusive to `connection_id`.
    StartMedia(StartMediaPayload),

    /// Stop a per-connection media pipeline. The worker fully drops the
    /// encoder + per-connection IPC sender + any per-connection state.
    /// Capture stops only when the last encoder for the current desktop
    /// has stopped.
    StopMedia(StopMediaPayload),

    /// Daemon → worker: register the validated capability ceiling for a
    /// connection admitted under a redeemed grant. Sent from
    /// `handle_request_remote` the moment the daemon stamps the connection's
    /// `SignalingState`, before any worker-bound frame for that connection, so the
    /// worker's per-connection ceiling map is populated ahead of the first
    /// file-list / terminal / media request. `ceiling = None` marks an
    /// owner/unrestricted connection (no cap). The map entry is cleared when the
    /// connection tears down via [`Self::StopMedia`]. Routed on the never-drop
    /// event pipe so it is FIFO-ordered ahead of the connection's other frames.
    SetConnectionCeiling(SetConnectionCeilingPayload),

    /// Update encoder parameters mid-stream (bitrate / fps / quality).
    UpdateMediaSettings(UpdateMediaSettingsPayload),

    /// Apply one serialized video/audio slot action for a connection.
    ApplyMediaSettings(ApplyMediaSettingsPayload),

    /// Force the per-connection encoder to emit an IDR (key-) frame on the
    /// next encode call. Sent by the daemon when (a) a new worker is taking
    /// over the connection, or (b) the daemon's RTCP reader saw a PLI for
    /// this connection. **Routed per-connection** (broadcasting would cause
    /// IDR bursts on unrelated browsers).
    ForceKeyframe(ForceKeyframePayload),

    // ---------- Input / clipboard (event pipe) ----------
    /// Mouse non-move (button / wheel) event from the browser DataChannel.
    /// `data` is the raw payload as decoded from the channel (currently
    /// JSON; the worker re-decodes with its existing input handler).
    /// The daemon authorises (`accept_control`) before forwarding.
    MouseInput(InputPayload),

    /// High-frequency mouse-move event. Carried separately because the
    /// browser sends them on a dedicated DC at >100 Hz; keeping the variant
    /// distinct lets the worker apply move-specific coalescing.
    MouseMoveInput(InputPayload),

    /// Keyboard event from the browser DataChannel.
    KeyboardInput(InputPayload),

    /// Browser → host clipboard write. Only delivered when
    /// `accept_clipboard_sync` is true on the daemon side.
    ClipboardWrite(ClipboardPayload),

    /// Browser asked the host to send back the current clipboard contents.
    /// Worker replies via [`WorkerToService::ClipboardRead`].
    ClipboardRequest(ConnectionRefPayload),

    // ---------- Whiteboard pass-through ----------
    // (File-transfer commands moved to a dedicated file lane; see
    // `dual_transport::FILE_QUEUE_CAP` and `WorkerInitPayload::file_pipe_name`.)
    /// Opaque whiteboard / private-screen / Tauri-shell command from the
    /// browser DataChannel. Worker dispatches via the existing
    /// host_control forwarder.
    WhiteboardCommand(OpaqueConnectionPayload),

    // ---------- Typed control plane ----------
    /// Browser-issued private-screen toggle. Worker enables / disables
    /// the per-connection private screen via its `host_control_helper`.
    SetPrivateScreenVisibility(SetPrivateScreenVisibilityPayload),

    // ---------- Manager plane (typed) ----------
    /// Browser → worker request for the host's [`SystemInfo`]. Worker
    /// replies via [`WorkerToService::SystemInfoRetrieved`].
    GetSystemInfo(ManagerRequestRefPayload),

    /// Browser → worker request to enumerate files. Worker replies
    /// via [`WorkerToService::FilesListed`].
    ListFiles(ListFilesPayload),

    /// Browser → worker request to delete a file. Worker replies via
    /// [`WorkerToService::FileDeleted`] (empty body).
    DeleteFile(DeleteFilePayload),

    // ---------- Terminal plane (typed) ----------
    /// Browser → worker request to launch a new PTY-backed terminal
    /// session. Worker replies via
    /// [`WorkerToService::TerminalStarted`] (empty body) on success;
    /// failures take the typed [`WorkerToService::SignalingError`]
    /// reverse path. The PTY reader thread emits `TerminalOutputProduced`
    /// chunks until the child exits, at which point the monitor task
    /// emits `TerminalClosed`.
    StartTerminal(StartTerminalPayload),

    /// Browser → worker keystroke / paste write to a running terminal.
    /// One-way — no response variant.
    SendTerminalInput(SendTerminalInputPayload),

    /// Browser → worker terminal window resize. One-way.
    ResizeTerminal(ResizeTerminalPayload),

    /// Browser → worker terminal close (force-kills the child process
    /// tree by OS-session id). One-way; `TerminalClosed` is emitted by
    /// the monitor task when the child actually exits.
    CloseTerminal(CloseTerminalPayload),

    /// Browser → worker request for the list of available shells on
    /// this host. Worker replies via
    /// [`WorkerToService::TerminalCommandsListed`] (carries
    /// [`TerminalList`]).
    ListTerminalCommands(ListTerminalCommandsPayload),

    // ---------- File-transfer error feedback (event pipe) ----------
    /// Daemon → worker notification that a `dc.send` for a file-transfer
    /// payload failed. The daemon writer task only sees the wire-level
    /// SCTP send error; the worker owns transfer state (upload buffers,
    /// download cancel flags) and the browser-facing `FileTransferMessage`
    /// JSON shape, so the worker is responsible for aborting the affected
    /// transfer and emitting a `TransferError` back to the browser.
    ///
    /// Routed on the *event* pipe rather than the file pipe so a stuck
    /// browser DataChannel (which is what triggered the failure in the
    /// first place) cannot also block the failure notification — putting
    /// it on the file pipe would deadlock when the file lane is what's
    /// already saturated.
    FileTransferSendFailed(FileTransferSendFailedPayload),

    // ---------- Virtual display (event pipe) ----------
    /// Daemon → worker: apply a new mode to the virtual monitor. Worker
    /// pushes the mode through the driver named-pipe and then calls
    /// `ChangeDisplaySettingsExW` on the attached `\\.\DISPLAYn`. Reply
    /// travels back via [`WorkerToService::VirtualDisplayMode`].
    SetVirtualDisplayMode(SetVirtualDisplayModePayload),

    /// Daemon → worker: a virtual display is live; rebuild any active
    /// capture pipeline so it targets the virtual monitor. Re-sent every
    /// time the daemon sees a worker [`WorkerToService::Capabilities`]
    /// while the supervisor is in the `Attached` state, so a freshly
    /// reattached worker recovers without polling.
    AttachVirtualDisplay(AttachVirtualDisplayPayload),

    /// Daemon → worker: the virtual display has gone away. Rebuild any
    /// active capture pipeline against the user's original physical-
    /// display target.
    DetachVirtualDisplay,

    /// Daemon → worker: re-publish the worker's [`MediaCapabilities`]
    /// so the daemon's cached snapshot (and any frontend that fetches
    /// it via the next `RemoteAccessInitializedData`) reflects the worker's
    /// latest enumeration. Sent by the daemon's
    /// `VirtualDisplaySupervisor` when an IDD virtual monitor finishes
    /// attaching (or finishes detaching) — these transitions change
    /// what `monitors::enum_display_infos` returns on the worker side,
    /// but the worker only emits `WorkerToService::Capabilities`
    /// proactively at startup, so without this push the daemon's
    /// cached capabilities would never see the IDD. Worker replies by
    /// emitting a fresh `WorkerToService::Capabilities` via its event
    /// writer. Unit variant — the worker re-reads `desktop_name` /
    /// `has_tauri` from its cached `WorkerInitPayload`.
    RefreshCapabilities,

    /// Daemon → worker: toggle the exclusive layer on top of the
    /// existing virtual-display attach. `desired = true` asks the
    /// worker to (a) show the pre-detach prompt for
    /// `prompt_duration_ms` ms on the physical displays then (b)
    /// snapshot + detach those physicals so Windows migrates windows
    /// onto the virtual display. `desired = false` reverses: reattach
    /// every physical to its snapshotted devmode.
    ///
    /// `op_id` is the daemon's monotonically-increasing operation
    /// counter; the worker echoes it back via
    /// [`WorkerToService::ExclusiveResult`] so the daemon can drop
    /// stale replies from a previous op (a fast user toggling control
    /// can leave one runner in flight while a newer one already
    /// supersedes it; the op_id gate disambiguates them).
    SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload),

    // ---------- AI agent plane (event pipe) ----------
    /// Daemon → worker: an AI capability call. The worker runs the
    /// matching collector / executor inside the user session (where
    /// WinSta0 / the authoritative capture frame live) and replies via
    /// [`WorkerToService::AgentCapabilityCompleted`]. `request_id` correlates the
    /// pair. The full [`desk_agent_protocol::AgentEnvelope`] is embedded
    /// verbatim — the daemon has already stamped its trusted fields
    /// (target / actor / scope / caller / request_id) before forwarding.
    InvokeAgentCapability(AgentRequestPayload),

    /// Daemon → worker: an immutable, exact-owner-approved Computer Use plan.
    /// This variant is the only IPC lane that can carry Computer Use mutation.
    ComputerActionPlan(ComputerActionPlanPayload),

    /// Daemon → worker: fence and cancel one Computer Use generation.
    ComputerActionCancel(ComputerActionCancelPayload),

    /// Daemon → worker: reconcile one durable Computer Use generation.
    ComputerActionStateQuery(ComputerActionStateQueryPayload),

    /// Daemon → worker: a sealed, user-approved execution plan. The worker
    /// executes `plan.program` + `plan.argv` **verbatim** (no shell re-parse,
    /// no elevation, no stdin) inside the user session and replies via
    /// [`WorkerToService::ExecutionCompleted`]. Unlike [`Self::InvokeAgentCapability`], exec never
    /// rides the capability envelope — only this dedicated variant carries an
    /// executable plan, so a read-only `InvokeAgentCapability` can never become one.
    ExecPlan(ExecPlanPayload),

    /// Daemon → worker: stop the execution running under this generation and
    /// reclaim its process tree.
    ///
    /// Fire-and-forget by design. The worker does not reply, because the only
    /// answer worth having is the execution's own terminal result, which already
    /// travels on [`WorkerToService::ExecutionCompleted`]. A separate acknowledgement
    /// would say a stop was *requested*, which no upstream can act on — the
    /// daemon answers "what state is it in now?" from its durable ledger, and
    /// naming a generation the worker is not running is not an error there.
    ExecCancel(ExecCancelPayload),

    /// Daemon → worker notification that the host-wide locale changed.
    ///
    /// The daemon has already persisted it; the worker applies it to its own
    /// process and settings copy and acknowledges with
    /// [`WorkerToService::LocaleApplied`].
    SetLocale(SetLocalePayload),

    // ---------- Security policy (event pipe) ----------
    /// Daemon → worker: the host security policy, as the daemon now holds it.
    ///
    /// The daemon is the only writer; a worker mirrors what arrives here and
    /// decides permission requests from the mirror. Ordering is carried in the
    /// snapshot rather than assumed from delivery: the worker keeps the
    /// higher-sequence policy and answers on
    /// [`WorkerToService::SecurityPolicyApplied`] with what it ended up holding.
    UpdateSecurityPolicy(UpdateSecurityPolicyPayload),

    /// Local host UI requested Wayland Portal authorization.
    AuthorizeWaylandPortal(AuthorizeWaylandPortalPayload),

    /// Local host UI cancelled the matching in-flight Portal operation.
    CancelWaylandPortal(CancelWaylandPortalPayload),
}

/// Messages sent from Worker process to Service Core (daemon) over the
/// **event** transport.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(tag = "type", content = "payload")]
pub enum WorkerToService {
    /// Worker has started and is ready to accept connections
    Ready,

    /// Worker reports its capability matrix once at startup (codecs +
    /// devices it can drive). The daemon uses this to pick the SDP m-line
    /// codec for new offers and to populate the UI's device pickers.
    Capabilities(MediaCapabilities),

    /// Worker mirrors non-sensitive Wayland Portal readiness to the daemon.
    WaylandPortalStatus(WaylandPortalStatusPayload),

    /// Worker → daemon error response carrying the original
    /// `SignalingType` + a `DeskErrorCode` numeric code + an optional
    /// human-readable message. Daemon rebuilds an outbound
    /// `SignalingModel::error(...)` and writes it onto the matching
    /// browser's signaling WS.
    ///
    /// This typed catch-all replaces the `WorkerToService::SignalingMessage`
    /// reverse path that previously carried `service::signaling::DeskSession::
    /// send_error` output verbatim. Worker-side helpers
    /// (`handle_manager_terminal_start`, `service::signaling`'s
    /// fallback `_ =>`, etc.) keep calling `session.send_error(...)`
    /// unchanged; the worker IPC main loop's outbound classifier
    /// detects `model.response_state.error_code != 0` and routes
    /// the error here regardless of the originating `SignalingType`.
    SignalingError(SignalingErrorPayload),

    /// Worker reports its health status
    Heartbeat(HeartbeatPayload),

    /// Worker detected the user-input desktop changed (e.g. user invoked UAC
    /// → secure desktop "Winlogon", switched to lock screen "ScreenSaver",
    /// etc.). The daemon's session-monitor running in session 0 cannot see
    /// across-window-station desktop changes — only a process living in
    /// the user's WinSta0 (i.e. the worker) can. The daemon reacts by
    /// shutting down this worker and launching a fresh one bound to the
    /// new desktop.
    DesktopChanged(DesktopChangedPayload),

    /// Worker reports an error
    Error(ErrorPayload),

    // ---------- Upstream events (event pipe) ----------
    /// Worker → daemon clipboard read: either spontaneous (host clipboard
    /// changed and `accept_clipboard_sync` is on for some connection) or
    /// in response to [`ServiceToWorker::ClipboardRequest`]. Daemon writes
    /// to the matching connection's clipboard DataChannel.
    ClipboardRead(ClipboardPayload),

    /// Cursor shape / position update for the cursor-sync DataChannel.
    /// Routed by daemon to the per-connection `cursor_sync` DC.
    CursorData(CursorDataPayload),
    /// File browsing or deletion was authorised and the first operation can execute.
    FileManagerOpened(FileManagerOpenedPayload),
    /// The worker accepted a file-transfer operation for processing.
    FileTransferStarted(FileTransferStartedPayload),
    /// A known file transfer reached one terminal outcome.
    FileTransferFinished(FileTransferFinishedPayload),

    // (File-transfer responses moved to a dedicated file lane; see
    // `dual_transport::FILE_QUEUE_CAP` and `WorkerInitPayload::file_pipe_name`.)

    // ---------- Typed control plane ----------
    /// Worker → daemon notification that the per-connection private
    /// screen visibility / support state changed. Sourced from the
    /// worker's `HostControlHub::subscribe_state` broadcast bus.
    /// The daemon forwards this to the browser as a
    /// `SignalingType::PrivateScreenStateChanged` reply on the matching
    /// signaling websocket.
    PrivateScreenStateChanged(PrivateScreenStateChangedPayload),

    /// Worker → daemon media state transition. Kept on the reliable event lane
    /// so a blocked notification cannot be dropped behind video backpressure.
    MediaPipelineState(MediaPipelineStatePayload),

    AudioPipelineStateChanged(AudioPipelineStateChangedPayload),

    MediaSettingsApplied(MediaSettingsAppliedPayload),

    // ---------- Manager plane (typed) ----------
    /// Worker → daemon response to
    /// [`ServiceToWorker::GetSystemInfo`]. Daemon
    /// rebuilds the matching `SignalingType::GetSystemInfo`
    /// outbound model and writes it to the browser's signaling WS.
    SystemInfoRetrieved(SystemInfoRetrievedPayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::ListFiles`].
    FilesListed(FilesListedPayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::DeleteFile`] (empty body —
    /// `request_id` correlates with the original request).
    FileDeleted(ManagerResponseRefPayload),

    // ---------- Terminal plane (typed) ----------
    /// Worker → daemon success reply for
    /// [`ServiceToWorker::StartTerminal`]. Empty body — the
    /// `request_id` correlates with the original request. The daemon
    /// rebuilds the matching `SignalingType::TerminalStarted` outbound
    /// model and writes it to the browser's signaling WS.
    TerminalStarted(TerminalStartedPayload),

    /// Worker → daemon notification that the PTY child process exited
    /// (either a clean exit observed by the monitor task or a forced
    /// close via [`ServiceToWorker::CloseTerminal`]). No
    /// `request_id` because this is a server-initiated notification
    /// rather than a response to any specific request.
    TerminalClosed(TerminalClosedPayload),

    /// Worker → daemon stdout chunk from the PTY reader thread.
    /// High-frequency keystroke-by-keystroke; chunks are 1 KB max.
    /// Travels on the event pipe (event traffic only — no media
    /// pressure on this path).
    TerminalOutputProduced(TerminalOutputProducedPayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::ListTerminalCommands`]. Carries the
    /// [`TerminalList`] (available shells + the configured default
    /// index).
    TerminalCommandsListed(TerminalCommandsListedPayload),

    // ---------- Virtual display (event pipe) ----------
    /// Worker → daemon reply to
    /// [`ServiceToWorker::SetVirtualDisplayMode`]. `outcome` carries
    /// either the mode the driver actually applied (which may have been
    /// snapped to the nearest supported configuration) or the error
    /// string from the user-mode controller. The daemon's outbound
    /// classifier maps this to a `SignalingType::ChangeDisplaySettings`
    /// response (Applied) or `SignalingModel::error(...)` (Failed).
    VirtualDisplayMode(VirtualDisplayModeResponsePayload),

    /// Worker → daemon reply to
    /// [`ServiceToWorker::AttachVirtualDisplay`]. The daemon stops at
    /// `SwDeviceCreate` (Session 0 cannot enumerate displays) and
    /// hands the PnP instance id to the worker; the worker resolves
    /// it to a GDI `\\.\DISPLAYn` inside the user session and reports
    /// the outcome back here. The daemon's supervisor uses this
    /// message — and **only** this message — to promote its state
    /// machine from `Attaching` to `Attached`. There is no
    /// browser-facing surface for this reply; attach failure is
    /// reflected to the browser indirectly via `is_active()` →
    /// `FEATURE_UNAVAILABLE` on subsequent `ChangeDisplaySettings`.
    VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload),

    /// Worker → daemon reply to
    /// [`ServiceToWorker::SetVirtualDisplayExclusive`]. The worker
    /// echoes the request's `op_id` back so the daemon's supervisor
    /// can `on_exclusive_result` op_id-gate: a stale result whose
    /// op_id no longer matches `current_op_id` is dropped without
    /// touching state. The supervisor's `ExclusiveState`
    /// transitions are driven exclusively from this reply.
    ExclusiveResult(ExclusiveResultPayload),

    // ---------- AI agent plane (event pipe) ----------
    /// Worker → daemon reply to [`ServiceToWorker::InvokeAgentCapability`]. The
    /// daemon rebuilds the outbound `SignalingType::AgentCapabilityCompleted` model
    /// for the control end (the `outcome` is reused verbatim as the
    /// signaling_data) and emits the audit event from the envelope +
    /// outcome. Capability-level errors travel inside `outcome`
    /// ([`desk_agent_protocol::AgentOutcome::Err`]), not on the
    /// transport-level response state — so the control-end UI receives
    /// the full structured [`desk_agent_protocol::AgentError`].
    AgentCapabilityCompleted(AgentResponsePayload),

    /// Worker → daemon: reports the conservative first-side-effect boundary.
    ComputerActionStarted(ComputerActionStartedPayload),

    /// Worker → daemon: terminal Computer Use facts and read-back verification.
    ComputerActionCompleted(ComputerActionCompletedPayload),

    /// Worker → daemon: response to a generation-fenced state query.
    ComputerActionStateReported(ComputerActionStateReportedPayload),

    /// Worker → daemon: bounded, dynamic Computer Use capability readiness.
    ComputerUseReadinessUpdated(ComputerUseReadinessPayload),

    /// Worker → daemon reply to [`ServiceToWorker::ExecPlan`]. The daemon
    /// rebuilds the outbound `SignalingType::ExecutionCompleted` model for the control
    /// end (the embedded [`desk_agent_protocol::exec::ExecResultPayload`] is
    /// reused verbatim) and routes it back to `connection_id`. Execution
    /// failures (timeout, spawn error) travel inside the payload's
    /// `AgentOutcome::Err`, not the transport.
    ExecutionCompleted(ExecResultIpcPayload),

    /// Worker → daemon, sent the moment a [`ServiceToWorker::ExecPlan`] either
    /// starts running or fails to start.
    ///
    /// Sent *before* the command's result, because the two answer different
    /// questions. The daemon reserved this execution before handing it over and
    /// until this arrives it cannot tell "still starting" from "started and lost",
    /// which is the gap that forces a crash there to be recorded as indeterminate.
    /// This closes that gap in the normal case and, on failure, upgrades it to the
    /// stronger and more useful "provably never started".
    ExecSpawnReport(ExecSpawnReportPayload),

    /// Worker → daemon: the command named by `request_id` is still running.
    ///
    /// Sent on a timer for as long as it runs. Losing one is harmless — it
    /// carries elapsed time rather than a sequence, so a gap says nothing an
    /// upstream would act on, and the authoritative answer is always a state
    /// query against the daemon's ledger.
    ExecHeartbeat(ExecHeartbeatPayload),

    /// Confirms that the worker applied a remote-access state transition and
    /// reports the worker-owned activity cancelled by it.
    RemoteAccessStateApplied(RemoteAccessStateAppliedPayload),

    /// Worker confirms that its live process locale and settings snapshot have
    /// converged to the host-wide locale.
    LocaleApplied(LocaleAppliedPayload),

    // ---------- Security policy (event pipe) ----------
    /// Worker → daemon: what the worker ended up holding after a published
    /// policy arrived. The daemon compares this against what it published to
    /// tell a converged worker from one that is still behind.
    SecurityPolicyApplied(SecurityPolicyAppliedPayload),

    /// Worker → daemon: a user answered a prompt with "remember this". Only the
    /// daemon can store it, so the worker forwards the answer along with the
    /// capability's stamp from when the prompt went out.
    RememberSecurityDecision(RememberSecurityDecisionPayload),
}

mod agent;
mod payloads;
mod virtual_display;

pub use agent::*;
pub use payloads::*;
pub use virtual_display::*;
#[cfg(test)]
mod tests;
