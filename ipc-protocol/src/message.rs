use desk_signal_facade::model::audio_capture::AudioDevice;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams, FileListResponse};
use desk_signal_facade::model::image_capture::DisplayInfo;
use desk_signal_facade::model::private_screen::PrivateScreenStateChangedData;
use desk_signal_facade::model::signal::SignalingType;
use desk_signal_facade::model::system_info::SystemInfo;
use desk_signal_facade::model::system_settings::RemoteSystemSettings;
use desk_signal_facade::model::terminal::{
    StartTerminalSession, TerminalInputData, TerminalList, TerminalOutputData, TerminalResizeData,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wincode::{SchemaRead, SchemaWrite};

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
    EnablePrivateScreen(EnablePrivateScreenPayload),

    /// Browser-issued desk-settings update. Carries the full
    /// `DeskSettings` so the worker can apply non-media fields
    /// (`wayland_control_mode`, `private_screen`, ...). The daemon
    /// also sniffs the media-relevant knobs and fans them out as
    /// [`Self::UpdateMediaSettings`] separately so the per-connection
    /// encoder pipeline retunes live (see `pc_manager::
    /// broadcast_media_settings_update`).
    UpdateDeskSettings(UpdateDeskSettingsPayload),

    // ---------- Manager plane (typed) ----------
    /// Browser → worker request for the host's [`SystemInfo`]. Worker
    /// replies via [`WorkerToService::ManagerSystemInfoResponse`].
    ManagerSystemInfoRequest(ManagerRequestRefPayload),

    /// Browser → worker request to enumerate files. Worker replies
    /// via [`WorkerToService::ManagerFileListResponse`].
    ManagerFileListRequest(ManagerFileListRequestPayload),

    /// Browser → worker request to delete a file. Worker replies via
    /// [`WorkerToService::ManagerFileDeleteResponse`] (empty body).
    ManagerFileDeleteRequest(ManagerFileDeleteRequestPayload),

    /// Browser → worker request for [`RemoteSystemSettings`]. Worker
    /// replies via [`WorkerToService::ManagerQuerySettingsResponse`].
    ManagerQuerySettingsRequest(ManagerRequestRefPayload),

    /// Browser → worker update of [`RemoteSystemSettings`]. Worker
    /// persists the new values and replies via
    /// [`WorkerToService::ManagerUpdateSettingsResponse`] (empty body).
    ManagerUpdateSettingsRequest(ManagerUpdateSettingsRequestPayload),

    // ---------- Terminal plane (typed) ----------
    /// Browser → worker request to launch a new PTY-backed terminal
    /// session. Worker replies via
    /// [`WorkerToService::TerminalStarted`] (empty body) on success;
    /// failures take the typed [`WorkerToService::SignalingError`]
    /// reverse path. The PTY reader thread emits `ReplyFromTerminal`
    /// chunks until the child exits, at which point the monitor task
    /// emits `TerminalClosed`.
    StartTerminalRequest(StartTerminalRequestPayload),

    /// Browser → worker keystroke / paste write to a running terminal.
    /// One-way — no response variant.
    SendDataToTerminalRequest(SendDataToTerminalPayload),

    /// Browser → worker terminal window resize. One-way.
    ResizeTerminalRequest(ResizeTerminalPayload),

    /// Browser → worker terminal close (force-kills the child process
    /// tree by OS-session id). One-way; `TerminalClosed` is emitted by
    /// the monitor task when the child actually exits.
    CloseTerminalRequest(CloseTerminalPayload),

    /// Browser → worker request for the list of available shells on
    /// this host. Worker replies via
    /// [`WorkerToService::ListTerminalResponse`] (carries
    /// [`TerminalList`]).
    ListTerminalRequest(ListTerminalRequestPayload),

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
    /// it via the next `InitSignalingData`) reflects the worker's
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
    /// [`WorkerToService::AgentResponse`]. `request_id` correlates the
    /// pair. The full [`desk_agent_protocol::AgentEnvelope`] is embedded
    /// verbatim — the daemon has already stamped its trusted fields
    /// (target / actor / scope / caller / request_id) before forwarding.
    AgentRequest(AgentRequestPayload),

    /// Daemon → worker: a sealed, user-approved execution plan. The worker
    /// executes `plan.program` + `plan.argv` **verbatim** (no shell re-parse,
    /// no elevation, no stdin) inside the user session and replies via
    /// [`WorkerToService::ExecResult`]. Unlike [`AgentRequest`], exec never
    /// rides the capability envelope — only this dedicated variant carries an
    /// executable plan, so a read-only `AgentRequest` can never become one.
    ExecPlan(ExecPlanPayload),

    /// Daemon → worker: stop the execution running under this generation and
    /// reclaim its process tree.
    ///
    /// Fire-and-forget by design. The worker does not reply, because the only
    /// answer worth having is the execution's own terminal result, which already
    /// travels on [`WorkerToService::ExecResult`]. A separate acknowledgement
    /// would say a stop was *requested*, which no upstream can act on — the
    /// daemon answers "what state is it in now?" from its durable ledger, and
    /// naming a generation the worker is not running is not an error there.
    ExecCancel(ExecCancelPayload),
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

    // ---------- Manager plane (typed) ----------
    /// Worker → daemon response to
    /// [`ServiceToWorker::ManagerSystemInfoRequest`]. Daemon
    /// rebuilds the matching `SignalingType::ManagerSystemInfo`
    /// outbound model and writes it to the browser's signaling WS.
    ManagerSystemInfoResponse(ManagerSystemInfoResponsePayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::ManagerFileListRequest`].
    ManagerFileListResponse(ManagerFileListResponsePayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::ManagerFileDeleteRequest`] (empty body —
    /// `request_id` correlates with the original request).
    ManagerFileDeleteResponse(ManagerResponseRefPayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::ManagerQuerySettingsRequest`].
    ManagerQuerySettingsResponse(ManagerQuerySettingsResponsePayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::ManagerUpdateSettingsRequest`] (empty
    /// body — settings persistence happens on the worker side).
    ManagerUpdateSettingsResponse(ManagerResponseRefPayload),

    // ---------- Terminal plane (typed) ----------
    /// Worker → daemon success reply for
    /// [`ServiceToWorker::StartTerminalRequest`]. Empty body — the
    /// `request_id` correlates with the original request. The daemon
    /// rebuilds the matching `SignalingType::TerminalStarted` outbound
    /// model and writes it to the browser's signaling WS.
    TerminalStarted(TerminalStartedPayload),

    /// Worker → daemon notification that the PTY child process exited
    /// (either a clean exit observed by the monitor task or a forced
    /// close via [`ServiceToWorker::CloseTerminalRequest`]). No
    /// `request_id` because this is a server-initiated notification
    /// rather than a response to any specific request.
    TerminalClosed(TerminalClosedPayload),

    /// Worker → daemon stdout chunk from the PTY reader thread.
    /// High-frequency keystroke-by-keystroke; chunks are 1 KB max.
    /// Travels on the event pipe (event traffic only — no media
    /// pressure on this path).
    ReplyFromTerminal(ReplyFromTerminalPayload),

    /// Worker → daemon response to
    /// [`ServiceToWorker::ListTerminalRequest`]. Carries the
    /// [`TerminalList`] (available shells + the configured default
    /// index).
    ListTerminalResponse(ListTerminalResponsePayload),

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
    /// Worker → daemon reply to [`ServiceToWorker::AgentRequest`]. The
    /// daemon rebuilds the outbound `SignalingType::AgentResponse` model
    /// for the control end (the `outcome` is reused verbatim as the
    /// signaling_data) and emits the audit event from the envelope +
    /// outcome. Capability-level errors travel inside `outcome`
    /// ([`desk_agent_protocol::AgentOutcome::Err`]), not on the
    /// transport-level response state — so the control-end UI receives
    /// the full structured [`desk_agent_protocol::AgentError`].
    AgentResponse(AgentResponsePayload),

    /// Worker → daemon reply to [`ServiceToWorker::ExecPlan`]. The daemon
    /// rebuilds the outbound `SignalingType::ExecResult` model for the control
    /// end (the embedded [`desk_agent_protocol::exec::ExecResultPayload`] is
    /// reused verbatim) and routes it back to `connection_id`. Execution
    /// failures (timeout, spawn error) travel inside the payload's
    /// `AgentOutcome::Err`, not the transport.
    ExecResult(ExecResultIpcPayload),

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
}

// ==================== Payload Types ====================

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct WorkerInitPayload {
    /// Session ID for this worker instance
    pub session_id: String,
    /// OS session ID
    pub os_session_id: u32,
    /// Desktop name being served
    pub desktop_name: Option<String>,
    /// Configuration JSON (DeskSettings serialized)
    pub config_json: String,
    /// Signaling server URL to connect to (or proxy through service)
    pub signaling_url: Option<String>,
    /// Authentication token. In daemon-spawned workers this is the
    /// `tauri_ipc_token` used to authenticate the worker's host-control
    /// upstream ws connection back to the daemon.
    pub auth_token: Option<String>,
    /// URL of the daemon's `/ws/host_upstream` endpoint. When `Some`, the
    /// worker constructs a Forwarder-mode `HostControlHub` that bridges
    /// approval / private-screen / whiteboard traffic through the daemon to
    /// the connected Tauri shell. When `None`, the worker falls back to a
    /// Local hub (used by tests / standalone runs).
    #[serde(default)]
    pub host_upstream_url: Option<String>,

    /// Media pipe name. The worker connects to this pipe *in
    /// addition* to the event pipe (the `--pipe` CLI arg) so encoded
    /// video / audio frames travel over a dedicated transport
    /// independent of the event traffic. `None` on portable / standalone
    /// runs that do not need a separate media pipe — the worker treats
    /// this as "fall back to single-pipe mode".
    #[serde(default)]
    pub media_pipe_name: Option<String>,

    /// File-transfer pipe name. The worker connects to this
    /// pipe in addition to the event + media pipes so file-transfer
    /// chunks (download responses, upload commands) travel over a
    /// dedicated bidirectional transport. Carries
    /// [`FileTransferPayload`] in both directions at
    /// `FILE_QUEUE_CAP = 32` per direction. `None` only in portable /
    /// in-process mode where the daemon constructs an in-process
    /// channel pair instead — in named-pipe `ServiceDaemon` mode the
    /// daemon **must** populate this field; the worker treats a
    /// missing `file_pipe_name` in that mode as a fatal init error
    /// and exits via `WorkerToService::Error`.
    #[serde(default)]
    pub file_pipe_name: Option<String>,

    /// Absolute path of the on-disk settings file the daemon is using.
    ///
    /// `Settings.args` carries `#[serde(skip)]`, so when the worker
    /// deserializes `config_json` it cannot recover `args.config_file_path`
    /// from the wire payload — and any worker-side `Settings::save()` call
    /// (e.g. when the user picks "remember" on a security approval prompt)
    /// would fall back to the default empty path and fail with
    /// `FILE_PATH_NOT_FOUND`.
    ///
    /// The daemon fills this with `args.config_file_path.clone()` so the
    /// worker writes back to the exact same on-disk file the daemon
    /// loaded. `Option<String>` with `#[serde(default)]` keeps backwards
    /// compatibility with older daemons whose Init payloads predate the
    /// field.
    #[serde(default)]
    pub config_file_path: Option<String>,
}

/// Payload for [`WorkerToService::SignalingError`]. Carries the
/// originating request's correlation fields plus a numeric
/// `DeskErrorCode` and optional message. The daemon rebuilds the
/// outbound `SignalingModel::error(...)` wire shape from these fields
/// — `signaling_type` is what the browser keys off when matching the
/// reply to its pending request.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SignalingErrorPayload {
    pub request_id: String,
    pub connection_id: String,
    pub signaling_type: SignalingType,
    pub error_code: i32,
    pub error_message: Option<String>,
}

// =============== Media + per-connection control ===============

/// One encoded media frame travelling worker → daemon over the dedicated
/// `MediaTransport`. Sized for 4K H.264 IDR frames (up to ~2 MB) which
/// the POC validated end-to-end at P99 < 16 ms.
///
/// `ts_ns` is wall-clock nanoseconds (`SystemTime::now()`) stamped at the
/// instant the encoder finished producing the frame; the daemon uses
/// `now_ns - ts_ns` for one-way latency telemetry only — RTP timestamps
/// for `webrtc-rs::TrackLocalStaticSample` are derived from the per-frame
/// `duration` field separately.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct MediaFrame {
    pub connection_id: String,
    pub seq: u64,
    /// Wall-clock ns since `UNIX_EPOCH` at encode-out time.
    pub ts_ns: u64,
    /// Frame duration in nanoseconds (passed through to
    /// `TrackLocalStaticSample::write_sample`).
    pub duration_ns: u64,
    pub kind: MediaFrameKind,
    /// Codec id, mirrored from [`MediaCapabilities`]. Worker stamps it so
    /// the daemon does not have to track per-connection codec separately.
    pub codec: MediaCodec,
    pub payload: Vec<u8>,
}

/// Frame classification on the media transport. The daemon uses this
/// (a) to know whether to suppress write_sample during worker swaps
/// (resume only on `VideoI`), and (b) to record latency histograms.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq, Hash,
)]
pub enum MediaFrameKind {
    VideoI,
    VideoP,
    Audio,
}

/// Encoder identity. Stays an enum (not free-form string) so we don't end
/// up with case-sensitive mismatches between worker and daemon.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq, Hash,
)]
pub enum MediaCodec {
    H264,
    Vp8,
    Vp9,
    Av1,
    Opus,
}

/// Worker advertises which codecs / capture sources it can drive on the
/// current desktop. Daemon decides which codec the SDP m-line offers and
/// echoes the device lists into the `InitSignalingData` reply so the
/// browser can render its capture-source picker.
///
/// `*_codecs` are ordered: index 0 is the worker's preferred choice.
/// `*_device_list` mirrors the structured maps that
/// `desk_capture_engine::list_image_capture` /
/// `list_audio_capture` produce so the daemon can pass them through to
/// `InitSignalingData::{video,audio}_device_list` without losing any
/// per-driver grouping or device metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct MediaCapabilities {
    pub video_codecs: Vec<MediaCodec>,
    pub audio_codecs: Vec<MediaCodec>,
    /// Concrete video-encoder identifiers reported verbatim from the
    /// capture-engine factory (e.g. `["X264", "VP8", "VP9", "H264",
    /// "AV1"]`). Distinct from `video_codecs` because the latter
    /// collapses every H.264 implementation onto a single
    /// `MediaCodec::H264` for SDP m-line negotiation, while this list
    /// preserves the per-implementation distinction the UI needs (so
    /// the user can pick libx264 vs OpenH264). The daemon copies this
    /// straight into `InitSignalingData::video_encoder_list`.
    #[serde(default)]
    pub video_encoders: Vec<String>,
    /// Audio counterpart of `video_encoders`. Today only `"OPUS"` is
    /// reported, but kept symmetrical so a future encoder addition
    /// doesn't need a wire-format bump.
    #[serde(default)]
    pub audio_encoders: Vec<String>,
    /// Per-backend display map (e.g. `"dxgi" -> [DISPLAY1, DISPLAY2]`).
    pub video_device_list: BTreeMap<String, Vec<DisplayInfo>>,
    /// Per-backend audio device map.
    pub audio_device_list: BTreeMap<String, Vec<AudioDevice>>,
    /// Whether this worker can talk to a Tauri shell on the same desktop
    /// (for whiteboard / private-screen rendering).
    pub has_tauri: bool,
    /// Whether the worker process token is elevated. The daemon uses this
    /// to decide whether to enable UAC-shielding paths.
    pub is_admin: bool,
    /// `OpenInputDesktop` desktop name the worker is bound to. Empty on
    /// non-Windows.
    pub desktop_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct StartMediaPayload {
    pub connection_id: String,
    pub video_codec: MediaCodec,
    pub audio_codec: MediaCodec,
    pub video_device: Option<String>,
    pub audio_device: Option<String>,
    /// Frames per second the encoder should target.
    pub fps: u32,
    /// Encoder bitrate in kbps (0 = encoder default). Historic field:
    /// it is *not* consumed as an initial bitrate cap — a fresh
    /// connection always starts at the encoder's initial ceiling and
    /// the first cap can only come from the daemon's REMB controller
    /// via `UpdateMediaSettingsPayload.bitrate_kbps`.
    pub bitrate_kbps: u32,
    /// Encoder quality knob (codec-specific 0–100; 0 = default).
    pub quality: u32,
    /// Whether the connection's SDP offer included an `m=video` section.
    /// `false` means the worker should *not* spawn the video pipeline
    /// (DXGI capture + encoder) for this connection — typical for the
    /// browser file-management page, which opens a PC purely for the
    /// `file_transfer_event` DataChannel and never wants screen capture.
    /// Defaults to `true` for back-compat with older daemons that did not
    /// thread through SDP track presence.
    #[serde(default = "default_true")]
    pub start_video: bool,
    /// Whether the connection's SDP offer included an `m=audio` section.
    /// `false` means the worker should *not* spawn the audio pipeline
    /// (WASAPI capture + Opus encoder) for this connection. Defaults to
    /// `true` for back-compat (see `start_video`).
    #[serde(default = "default_true")]
    pub start_audio: bool,
    /// Per-connection image-capture backend choice (e.g. "DXGI", "GDI"
    /// on Windows). `None` lets the worker fall back to its
    /// startup-time `DeskSettings.image_capture` (which itself
    /// defaults to the platform's preferred backend). Threading the
    /// per-connection choice through the IPC payload is required
    /// because the worker's base settings are a snapshot taken at
    /// worker spawn — without this field, a browser opening a second
    /// connection cannot pick a different backend than the first.
    #[serde(default)]
    pub image_capture: Option<String>,
    /// Per-connection override for the BGRA→YUV dirty-rect fast path
    /// in `PersistentYuvBuffer`. `None` means "use the worker's base
    /// `DeskSettings.enable_dirty_rect`" (back-compat with older
    /// daemons). `Some(false)` forces every frame through a full
    /// conversion — needed for the browser Advanced-tab kill-switch
    /// to take effect on the *first* StartMedia, before any live
    /// `UpdateMediaSettings` would be issued. Without this field a
    /// fresh connection always picks up the worker's default
    /// (`true`) regardless of the browser's offer-time setting.
    #[serde(default)]
    pub enable_dirty_rect: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct StopMediaPayload {
    pub connection_id: String,
}

/// Registers a connection's validated capability ceiling with the worker (see
/// [`ServiceToWorker::SetConnectionCeiling`]). `ceiling = None` means the
/// connection is an owner/unrestricted session with no cap.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SetConnectionCeilingPayload {
    pub connection_id: String,
    pub ceiling: Option<desk_signal_facade::model::security_settings::SecuritySettings>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct UpdateMediaSettingsPayload {
    pub connection_id: String,
    pub fps: Option<u32>,
    /// Runtime bitrate-cap directive with tri-state semantics —
    /// **`Some(0)` is meaningful, do not filter it out as an invalid
    /// value**:
    /// - `None` — leave the current cap alone (field not part of this
    ///   update).
    /// - `Some(0)` — clear the cap; the encoder returns to its initial
    ///   ceiling. Sent by the daemon when adaptive bitrate is switched
    ///   off so a previously tightened connection recovers.
    /// - `Some(k)` (k > 0) — cap the encoder at `k` kbps without
    ///   rebuilding it. Sent by the daemon's REMB controller at ~1 Hz
    ///   while adaptive bitrate is on.
    pub bitrate_kbps: Option<u32>,
    pub quality: Option<u32>,
    /// Toggle for the BGRA→YUV dirty-rect fast path in
    /// `PersistentYuvBuffer`. `None` means "leave the current value
    /// alone" (older daemons that never sniff the field). `Some(false)`
    /// forces every frame through a full conversion; `Some(true)`
    /// re-enables partial updates. Threaded through so the browser's
    /// Advanced-tab kill-switch can be retuned mid-stream without
    /// tearing down the encoder.
    #[serde(default)]
    pub enable_dirty_rect: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ForceKeyframePayload {
    pub connection_id: String,
}

/// Generic per-connection wrapper for opaque payloads (mouse / keyboard /
/// file-transfer / whiteboard etc.). The byte buffer is the raw DataChannel
/// payload as received from the browser; the worker's existing handlers
/// continue to deserialize JSON / bincode internally so we don't have to
/// duplicate every event schema in the IPC layer.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct InputPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ClipboardPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

/// Per-connection request that carries no payload of its own (e.g.
/// `ClipboardRequest`).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ConnectionRefPayload {
    pub connection_id: String,
}

/// Same shape as [`InputPayload`] but used for command-style payloads
/// (file-transfer / whiteboard) where the worker dispatches into a
/// handler module by re-decoding `data`. Distinct type alias keeps the
/// callsite intent explicit.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct OpaqueConnectionPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

/// File-transfer wire shape. Same as [`OpaqueConnectionPayload`] plus
/// an `is_text` discriminator: the WebRTC DataChannel distinguishes
/// text vs binary frames at the wire level (`DataChannelMessage::
/// is_string`), and the file-transfer protocol uses both — JSON
/// control messages travel as text, file chunks travel as binary —
/// so the IPC has to preserve that bit for the daemon's `dc.send_text`
/// vs `dc.send` decision.
///
/// `transfer_id` is populated by the worker so the daemon-side writer
/// task can reference a specific transfer when reporting a `dc.send`
/// failure back via [`ServiceToWorker::FileTransferSendFailed`]. The
/// daemon itself never parses `data` (the binary chunk header /
/// `FileTransferMessage` JSON shape lives in the worker), so without
/// this field the daemon would only know the failing `connection_id`
/// and the worker would have to abort *every* in-flight transfer on
/// that PC. The field is optional so legacy binaries / synthetic test
/// payloads still decode cleanly.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct FileTransferPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
    pub is_text: bool,
    #[serde(default)]
    pub transfer_id: Option<String>,
}

/// Classification of a `dc.send` failure observed by the daemon's
/// per-connection file-transfer writer task. The worker uses this to
/// pick its abort policy and to log the failure at an appropriate
/// severity:
///
/// - `PacketTooLarge` is a programmer / configuration bug — the chosen
///   chunk size exceeds the remote `a=max-message-size` SCTP advertise.
///   The whole transfer is doomed (every subsequent chunk will trip the
///   same check) so the worker must abort the transfer and surface a
///   `TransferError` to the browser. Logged at `error!` so it gets
///   investigated rather than absorbed into the warning noise.
/// - `TransportClosed` is normal teardown (peer disconnected /
///   `RTCPeerConnection` closed mid-transfer). Logged at `debug!` —
///   the failure is expected during shutdown and the transfer cleanup
///   is already on its way via `cleanup_pc`.
/// - `Other` is any unclassified `webrtc::Error`. Logged at `warn!`
///   so it surfaces in production but doesn't pollute the error budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub enum FileTransferSendErrorKind {
    /// The serialized message exceeded SCTP `max_message_size` for the
    /// outbound stream. Caused by chunk_size + binary-header > remote
    /// SDP advertise. Always fatal for the transfer.
    PacketTooLarge,
    /// The DataChannel / SCTP transport was closed before the send
    /// completed. Normal during teardown / peer disconnect.
    TransportClosed,
    /// Any unclassified error returned by `webrtc-rs`. Treated as
    /// transport-level failure for abort purposes but logged at a
    /// lower severity than `PacketTooLarge`.
    Other,
}

/// Daemon → worker payload carried by
/// [`ServiceToWorker::FileTransferSendFailed`]. The worker uses the
/// `transfer_id` (when present) to scope the abort to a single
/// in-flight transfer; if it's `None`, the worker falls back to
/// aborting every transfer on `connection_id` (legacy / unscoped
/// chunks emitted before the worker started populating `transfer_id`
/// on the file lane).
///
/// `chunk_index` is informational — the worker doesn't need it to abort,
/// but it goes straight into the `TransferError.message` so logs and
/// the browser-side toast can show exactly which chunk tripped the
/// failure.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct FileTransferSendFailedPayload {
    pub connection_id: String,
    #[serde(default)]
    pub transfer_id: Option<String>,
    #[serde(default)]
    pub chunk_index: Option<u32>,
    pub kind: FileTransferSendErrorKind,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct CursorDataPayload {
    pub connection_id: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct HeartbeatPayload {
    /// Current timestamp
    pub timestamp_ms: u64,
    /// Number of active WebRTC connections
    pub active_connections: u32,
    /// CPU usage percentage
    pub cpu_usage: Option<f32>,
    /// Memory usage in bytes
    pub memory_usage: Option<u64>,
}

/// Sentinel `ErrorPayload::code` value the worker emits when its media
/// transport blocked an I-frame send for longer than the configured
/// `MediaTransport` timeout. The daemon uses the matching `connection_id`
/// to issue `StopMedia` + `StartMedia` for that connection so the encoder
/// pipeline is reset rather than left wedged behind a saturated pipe.
///
/// Picked deliberately outside the `DeskErrorCode` u16 range so daemon-
/// side dispatch can match on it without colliding with broader IPC
/// error codes.
pub const ERROR_CODE_MEDIA_TRANSPORT_STUCK: i32 = -1001;

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ErrorPayload {
    /// Error code
    pub code: i32,
    /// Error message
    pub message: String,
    /// Whether the worker can continue operating
    pub recoverable: bool,
    /// Optional per-connection scope. When `Some`, the daemon can
    /// take per-connection recovery action (e.g. `StopMedia` +
    /// `StartMedia` on [`ERROR_CODE_MEDIA_TRANSPORT_STUCK`]). `None`
    /// for worker-wide errors that don't map to a single PC.
    #[serde(default)]
    pub connection_id: Option<String>,
}

// ---------- Typed control plane ----------

/// Payload for [`ServiceToWorker::EnablePrivateScreen`]. Mirrors the
/// JSON shape of `desk_signal_facade::model::private_screen::
/// EnablePrivateScreenData` plus the `connection_id` the daemon
/// already had at the WS-router boundary.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct EnablePrivateScreenPayload {
    pub connection_id: String,
    pub enable: bool,
}

/// Payload for [`ServiceToWorker::UpdateDeskSettings`]. Carries the
/// full `DeskSettings` struct so the worker applies every field; the
/// daemon separately sniffs the media-relevant knobs and emits
/// [`ServiceToWorker::UpdateMediaSettings`] for the encoder pipeline
/// (see `pc_manager::broadcast_media_settings_update`).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct UpdateDeskSettingsPayload {
    pub connection_id: String,
    pub settings: DeskSettings,
}

/// Payload for [`WorkerToService::PrivateScreenStateChanged`].
/// Mirrors `desk_signal_facade::model::private_screen::
/// PrivateScreenStateChangedData` plus the `connection_id` the
/// daemon needs to pick the right outbound signaling websocket.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct PrivateScreenStateChangedPayload {
    pub connection_id: String,
    pub data: PrivateScreenStateChangedData,
}

// ---------- Manager plane (typed) ----------

/// Shared envelope for body-less manager *requests*
/// (`ManagerSystemInfoRequest`, `ManagerQuerySettingsRequest`).
/// Carries the `request_id` so the worker can echo it back on the
/// matching response payload, and the `connection_id` so the daemon
/// can pick the right outbound signaling websocket when it ferries
/// the response. `connection_id` is `Option` because manager-plane
/// requests originating from a HTTP REST controller (e.g.
/// `signal-facade::controller::sysinfo` →
/// `connection.request_peer_with_callback`) carry no `from_connection_id`
/// — the daemon correlates the response by `request_id` alone in that
/// path.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerRequestRefPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
}

/// Shared envelope for body-less manager *responses*
/// (`ManagerFileDeleteResponse`, `ManagerUpdateSettingsResponse`).
/// Same shape as [`ManagerRequestRefPayload`] but kept distinct so
/// the daemon's response-direction code is symmetric with the
/// request-direction code at the type-system level. `connection_id`
/// is `Option` for the same reason — see that type's docstring.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerResponseRefPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
}

/// Payload for [`ServiceToWorker::ManagerFileListRequest`]. Carries
/// `FileListParams` (filtering knobs, paging) verbatim from the
/// browser-issued signaling envelope. `connection_id` is `Option`
/// because manager-plane queries can be HTTP-API-triggered (no
/// originating browser PC) — see [`ManagerRequestRefPayload`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerFileListRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub params: FileListParams,
}

/// Payload for [`ServiceToWorker::ManagerFileDeleteRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerFileDeleteRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub request: DeleteFileRequest,
}

/// Payload for [`ServiceToWorker::ManagerUpdateSettingsRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerUpdateSettingsRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub settings: RemoteSystemSettings,
}

/// Payload for [`WorkerToService::ManagerSystemInfoResponse`].
/// `SystemInfo` is the wire shape the worker computed from
/// `sysinfo::System` and the legacy handler used to send via
/// `send_response`. `connection_id` is `Option` because the matching
/// request can be HTTP-API-triggered with no `from_connection_id` —
/// see [`ManagerRequestRefPayload`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerSystemInfoResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub info: SystemInfo,
}

/// Payload for [`WorkerToService::ManagerFileListResponse`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerFileListResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub response: FileListResponse,
}

/// Payload for [`WorkerToService::ManagerQuerySettingsResponse`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ManagerQuerySettingsResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub settings: RemoteSystemSettings,
}

// ---------- Terminal plane (typed) ----------

/// Payload for [`ServiceToWorker::StartTerminalRequest`]. Carries the
/// browser-supplied [`StartTerminalSession`] (the comma-separated
/// command + args string the worker splits in
/// `handle_manager_terminal_start`). `request_id` is echoed back on
/// the [`WorkerToService::TerminalStarted`] reply.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct StartTerminalRequestPayload {
    pub request_id: String,
    pub connection_id: String,
    pub session: StartTerminalSession,
}

/// Payload for [`ServiceToWorker::SendDataToTerminalRequest`]. One-way —
/// no `request_id` because the worker does not reply.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SendDataToTerminalPayload {
    pub connection_id: String,
    pub data: TerminalInputData,
}

/// Payload for [`ServiceToWorker::ResizeTerminalRequest`]. One-way.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ResizeTerminalPayload {
    pub connection_id: String,
    pub data: TerminalResizeData,
}

/// Payload for [`ServiceToWorker::CloseTerminalRequest`]. Body-less
/// (the only thing the worker needs is the connection id). Distinct
/// from [`ConnectionRefPayload`] / [`TerminalClosedPayload`] so the
/// terminal-plane direction is symmetric at the type level.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct CloseTerminalPayload {
    pub connection_id: String,
}

/// Payload for [`ServiceToWorker::ListTerminalRequest`]. Body-less;
/// `request_id` is echoed back on the
/// [`WorkerToService::ListTerminalResponse`]. `connection_id` is
/// `Option` because `signal-facade::controller::terminal::list_terminal`
/// dispatches via `connection.request_peer_with_callback` with no
/// originating browser PC — the response is correlated by
/// `request_id`.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ListTerminalRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
}

/// Payload for [`WorkerToService::TerminalStarted`]. Empty body —
/// `request_id` correlates with the originating
/// [`ServiceToWorker::StartTerminalRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct TerminalStartedPayload {
    pub request_id: String,
    pub connection_id: String,
}

/// Payload for [`WorkerToService::TerminalClosed`]. No `request_id` —
/// this is a notification fired by the worker's monitor task when
/// the PTY child process exits, not a response to a specific request.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct TerminalClosedPayload {
    pub connection_id: String,
}

/// Payload for [`WorkerToService::ReplyFromTerminal`]. Each chunk is
/// at most ~1 KB (the worker's PTY reader buffer size), so the event
/// pipe handles the rate fine without competing with media frames.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ReplyFromTerminalPayload {
    pub connection_id: String,
    pub data: TerminalOutputData,
}

/// Payload for [`WorkerToService::ListTerminalResponse`]. Carries the
/// fully resolved [`TerminalList`] the worker built from
/// `which::which`/`which_re` lookups. `connection_id` is `Option` for
/// the same reason as [`ListTerminalRequestPayload`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ListTerminalResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub terminals: TerminalList,
}

#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct DesktopChangedPayload {
    /// New input desktop name as returned by `OpenInputDesktop` +
    /// `GetUserObjectInformationW(UOI_NAME)`. Examples: "Default", "Winlogon",
    /// "Screen-saver". The daemon launches the next worker with this name as
    /// the `lpDesktop` argument to `CreateProcessAsUserW`.
    pub name: String,
}

// ============= Virtual display IPC payloads =============

/// Payload for [`ServiceToWorker::SetVirtualDisplayMode`]. The browser
/// sends a `SignalingType::ChangeDisplaySettings`; the daemon validates
/// it (`desk_virtual_display::validate_mode`) and forwards it here. The
/// worker calls `VirtualDisplayController::set_mode` (driver pipe + CDS).
/// `request_id` correlates with [`WorkerToService::VirtualDisplayMode`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct SetVirtualDisplayModePayload {
    pub request_id: String,
    pub connection_id: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Payload for [`ServiceToWorker::AttachVirtualDisplay`]. The daemon
/// holds the `SwDevice` handle and forwards the OS-assigned PnP
/// instance id (e.g. `SWD\LcxlVirtualDisplay\LcxlVirtualDisplay`) the
/// IDD monitor was assigned. The worker resolves the instance id to a
/// GDI `\\.\DISPLAYn` from inside the user session (where
/// `EnumDisplayDevicesW` actually sees the virtual monitor) and replies
/// with [`WorkerToService::VirtualDisplayAttachResult`]. The daemon
/// cannot resolve the display name itself because Session 0 (the
/// LocalSystem service desktop) does not see any GDI displays.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct AttachVirtualDisplayPayload {
    pub instance_id: String,
}

/// Wire form of `desk_virtual_display::VirtualDisplayMode`. Duplicated
/// here intentionally so `desk-ipc-protocol` does not need a reverse
/// dependency onto `desk-virtual-display`.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq, Hash,
)]
pub struct VirtualDisplayModeData {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

/// Result of a worker-side `set_mode`. The IDD driver is free to snap
/// the requested mode to the nearest supported configuration, so
/// `Applied` carries what actually took effect, not what was requested.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
#[serde(tag = "status", content = "data")]
pub enum VirtualDisplayModeOutcome {
    Applied(VirtualDisplayModeData),
    Failed(String),
}

/// Payload for [`WorkerToService::VirtualDisplayMode`]. Correlates with
/// the originating [`ServiceToWorker::SetVirtualDisplayMode`] via
/// `request_id` so the daemon's outbound classifier can wire it back to
/// the matching browser signaling websocket.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct VirtualDisplayModeResponsePayload {
    pub request_id: String,
    pub connection_id: String,
    pub outcome: VirtualDisplayModeOutcome,
}

/// Result of the worker resolving a PnP instance id (forwarded from the
/// daemon via [`ServiceToWorker::AttachVirtualDisplay`]) into a usable
/// GDI display name. Modelled as an explicit two-variant enum so the
/// wincode / serde wire shapes match the rest of `message.rs` rather
/// than the ad-hoc `Result<T, E>` envelope.
///
/// - `Attached(display_name)` — `display_name` is the GDI
///   `\\.\DISPLAYn` form the worker captured against.
/// - `Failed(message)` — exhaustive worker-side retries did not turn
///   up a GDI device matching the instance id (e.g. the driver
///   crashed, PnP node disappeared, or `EnumDisplayDevicesW` raced
///   with the IDD monitor arrival window).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
#[serde(tag = "status", content = "data")]
pub enum VirtualDisplayAttachOutcome {
    Attached(String),
    Failed(String),
}

/// Payload for [`WorkerToService::VirtualDisplayAttachResult`]. The
/// `instance_id` field correlates the reply with a specific
/// `SwDeviceCreate` round so the supervisor can drop stale replies
/// that arrive after the daemon has re-created the underlying handle
/// (i.e. after a daemon restart, where the PnP id is identical but the
/// in-memory supervisor state is fresh).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct VirtualDisplayAttachResultPayload {
    pub instance_id: String,
    pub outcome: VirtualDisplayAttachOutcome,
}

/// Payload for [`ServiceToWorker::SetVirtualDisplayExclusive`].
///
/// `op_id` is monotonically incremented by the daemon's supervisor
/// each time it issues a new exclusive command (enter or leave). The
/// worker stores it on the runner currently doing the work and feeds
/// it back via [`ExclusiveResultPayload::op_id`] so the daemon can
/// drop stale results from a superseded runner.
///
/// `prompt_duration_ms` is the system-level
/// `Settings.virtual_display.prompt_ms` snapshot at the moment the
/// daemon decided to enter exclusive. `0` skips the prompt entirely.
/// Ignored on a `desired = false` request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct SetVirtualDisplayExclusivePayload {
    pub op_id: u64,
    pub desired: bool,
    pub prompt_duration_ms: u32,
}

/// Direction the worker was driving when it produced this result.
/// Disambiguates [`ExclusiveOutcome::Entered`] vs `Left` at the
/// daemon state machine, which transitions different states for each.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExclusiveDirection {
    Entering,
    Leaving,
}

/// Outcome reported by the worker's exclusive runner. Four variants
/// only — `EnterCancelled` was removed in design review round 6
/// because the new pipeline never emits one: a cancelled enter
/// returns silently and the next runner publishes the actual final
/// state (Entered / Left).
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
#[serde(tag = "status", content = "data")]
pub enum ExclusiveOutcome {
    Entered,
    EnterFailed(String),
    Left,
    LeftWithErrors(String),
}

/// Payload for [`WorkerToService::ExclusiveResult`]. `op_id` echoes
/// the originating request; the daemon's supervisor drops anything
/// whose `op_id != current_op_id` at the lock boundary.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead, PartialEq, Eq)]
pub struct ExclusiveResultPayload {
    pub op_id: u64,
    pub direction: ExclusiveDirection,
    pub outcome: ExclusiveOutcome,
}

// ================= AI agent IPC payloads =================

/// Payload for [`ServiceToWorker::AgentRequest`]. Embeds the full
/// [`desk_agent_protocol::AgentEnvelope`] (already server-stamped) so the
/// IPC layer does not re-spell any of its fields — `desk-agent-protocol`
/// derives the same `wincode` schema this transport uses. `connection_id`
/// is `Option` for the same reason as the manager-plane payloads: an
/// orchestrator-initiated call may carry no originating control-end
/// connection and is correlated by `request_id` alone.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct AgentRequestPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub envelope: desk_agent_protocol::AgentEnvelope,
}

/// Payload for [`WorkerToService::AgentResponse`]. Reuses
/// [`desk_agent_protocol::AgentOutcome`] verbatim — the same shape the
/// daemon then ships to the control end as `AgentResponse`
/// signaling_data, so there is no daemon-side re-mapping. Mirrors the
/// `VirtualDisplayModeOutcome` Applied/Failed precedent.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct AgentResponsePayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub outcome: desk_agent_protocol::AgentOutcome,
}

/// Payload for [`ServiceToWorker::ExecPlan`]. Carries the sealed
/// [`desk_agent_protocol::exec::ExecPlan`] plus the signaling correlation
/// (`request_id`) and originating control-end `connection_id`, which the worker
/// echoes back in [`ExecResultIpcPayload`] so the daemon can route the outbound
/// `ExecResult` without keeping its own in-flight map.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecPlanPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub plan: desk_agent_protocol::exec::ExecPlan,
    /// Originating ConfirmExec frame `request_id` (the manager's authorization
    /// ledger key). The worker echoes it back in [`ExecResultIpcPayload`] so the
    /// `command_completed` audit event can be attributed to the real operator.
    /// `None` on the single-machine / non-manager path.
    pub audit_source_request_id: Option<String>,
}

/// Payload for [`ServiceToWorker::ExecCancel`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecCancelPayload {
    /// The one dispatch to stop. Keyed on the generation rather than the task so
    /// a cancel aimed at an earlier attempt can never kill its retry.
    pub execution_generation: String,
}

/// Payload for [`WorkerToService::ExecResult`]. Embeds the
/// [`desk_agent_protocol::exec::ExecResultPayload`] (tagged with
/// `exec_request_id`) the daemon ships to the control end verbatim, plus the
/// echoed `request_id` / `connection_id` for routing.
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecResultIpcPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    pub result: desk_agent_protocol::exec::ExecResultPayload,
    /// Echoed `audit_source_request_id` from the [`ExecPlanPayload`] so the
    /// daemon can attribute the `command_completed` audit event to the
    /// originating ConfirmExec frame (the manager's ledger key). `None` on the
    /// single-machine / non-manager path.
    pub audit_source_request_id: Option<String>,
}

/// Payload for [`WorkerToService::ExecSpawnReport`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecSpawnReportPayload {
    /// The dispatch this reports on: the `request_id` the plan was sent under,
    /// which is also the execution generation the daemon's ledger keys on.
    pub request_id: String,
    /// Echoed from the [`ExecPlanPayload`] so the daemon can route the outbound
    /// lifecycle frame to whoever asked for the execution, exactly as it routes
    /// the result.
    pub connection_id: Option<String>,
    pub report: ExecSpawnReport,
}

/// Payload for [`WorkerToService::ExecHeartbeat`].
#[derive(Debug, Clone, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub struct ExecHeartbeatPayload {
    pub request_id: String,
    pub connection_id: Option<String>,
    /// Milliseconds since the worker began this execution. The worker's own
    /// elapsed time, not a wall clock, so nothing downstream has to reconcile two
    /// machines' clocks to decide whether progress is being made.
    pub running_ms: u64,
}

/// What became of a spawn attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead)]
pub enum ExecSpawnReport {
    /// The process is running and contained.
    Started {
        /// How to find and reclaim its process tree — a job name on Windows, a
        /// process group on Unix. `None` if the platform could not name the
        /// container even after the spawn.
        containment_identity: Option<String>,
    },
    /// The command never started, so it provably did not run. Worth distinguishing
    /// from an unknown outcome: a caller may safely retry this one.
    Failed {
        /// Operator-facing reason (missing program, containment refused, …).
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spawn report round-trips both ways, including the containment identity
    /// the daemon needs to reclaim a tree it has lost track of.
    #[test]
    fn exec_spawn_report_round_trips() {
        for report in [
            ExecSpawnReport::Started {
                containment_identity: Some("pgid:4242".to_string()),
            },
            ExecSpawnReport::Started {
                containment_identity: None,
            },
            ExecSpawnReport::Failed {
                reason: "no such program".to_string(),
            },
        ] {
            let original = WorkerToService::ExecSpawnReport(ExecSpawnReportPayload {
                request_id: "gen-1".to_string(),
                connection_id: Some("conn-1".to_string()),
                report: report.clone(),
            });
            match wincode_round_trip(&original) {
                WorkerToService::ExecSpawnReport(p) => {
                    assert_eq!(p.request_id, "gen-1");
                    assert_eq!(p.report, report);
                }
                other => panic!("expected ExecSpawnReport, got {other:?}"),
            }
        }
    }

    /// New `host_upstream_url` + repurposed `auth_token` fields round-trip cleanly.
    #[test]
    fn worker_init_payload_round_trip_with_host_upstream_fields() {
        let original = WorkerInitPayload {
            session_id: "session-1".to_string(),
            os_session_id: 7,
            desktop_name: Some("Default".to_string()),
            config_json: "{}".to_string(),
            signaling_url: None,
            auth_token: Some("ipc-token".to_string()),
            host_upstream_url: Some("ws://127.0.0.1:8082/ws/host_upstream".to_string()),
            media_pipe_name: Some(r"\\.\pipe\lcxl-desk-ipc-7-uuid-media".to_string()),
            file_pipe_name: Some(r"\\.\pipe\lcxl-desk-file-ipc-7-uuid".to_string()),
            config_file_path: Some(r"C:\ProgramData\lcxl\settings.toml".to_string()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: WorkerInitPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.session_id, original.session_id);
        assert_eq!(decoded.os_session_id, original.os_session_id);
        assert_eq!(decoded.auth_token, original.auth_token);
        assert_eq!(decoded.host_upstream_url, original.host_upstream_url);
        assert_eq!(decoded.media_pipe_name, original.media_pipe_name);
        assert_eq!(decoded.file_pipe_name, original.file_pipe_name);
        assert_eq!(decoded.config_file_path, original.config_file_path);
    }

    /// `DesktopChanged` round-trips with the same JSON shape the IPC reader
    /// expects (tag = "type", content = "payload").
    #[test]
    fn desktop_changed_round_trips() {
        let msg = WorkerToService::DesktopChanged(DesktopChangedPayload {
            name: "Winlogon".to_string(),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: WorkerToService = serde_json::from_str(&json).unwrap();
        match decoded {
            WorkerToService::DesktopChanged(payload) => assert_eq!(payload.name, "Winlogon"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Older daemons that don't yet emit `host_upstream_url` /
    /// `media_pipe_name` / `file_pipe_name` / `config_file_path` must
    /// still be accepted by newer workers (all four fields carry
    /// `#[serde(default)]`).
    #[test]
    fn worker_init_payload_accepts_missing_optional_fields() {
        let legacy = serde_json::json!({
            "session_id": "session-1",
            "os_session_id": 7,
            "desktop_name": null,
            "config_json": "{}",
            "signaling_url": null,
            "auth_token": null,
        });
        let decoded: WorkerInitPayload = serde_json::from_value(legacy).unwrap();
        assert!(decoded.host_upstream_url.is_none());
        assert!(decoded.auth_token.is_none());
        assert!(decoded.media_pipe_name.is_none());
        assert!(decoded.file_pipe_name.is_none());
        assert!(decoded.config_file_path.is_none());
    }

    // ============== IPC variants — wincode round-trips ==============

    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    /// Unbounded wincode `Configuration` matching the production IPC
    /// path (`IPC_CONFIG` in `transport.rs` / `dual_transport.rs`):
    /// preallocation limit disabled so encode + decode accept the full
    /// 16 MB transport-layer ceiling without firing the 4 MiB default
    /// safety net.
    type WincodeUnbounded = Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED>;

    fn wincode_round_trip<T>(value: &T) -> T
    where
        T: wincode::SchemaWrite<WincodeUnbounded, Src = T>
            + for<'de> wincode::SchemaRead<'de, WincodeUnbounded, Dst = T>,
    {
        let config: WincodeUnbounded = Configuration::new();
        let bytes = wincode::config::serialize(value, config).expect("encode");
        wincode::config::deserialize(&bytes, config).expect("decode")
    }

    #[test]
    fn start_media_round_trips_wincode() {
        let msg = ServiceToWorker::StartMedia(StartMediaPayload {
            connection_id: "conn-1".to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: Some("\\\\.\\DISPLAY1".to_string()),
            audio_device: None,
            fps: 60,
            bitrate_kbps: 6_000,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: Some(false),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::StartMedia(p) => {
                assert_eq!(p.connection_id, "conn-1");
                assert_eq!(p.video_codec, MediaCodec::H264);
                assert_eq!(p.audio_codec, MediaCodec::Opus);
                assert_eq!(p.fps, 60);
                assert!(p.start_video);
                assert!(p.start_audio);
                assert_eq!(
                    p.enable_dirty_rect,
                    Some(false),
                    "enable_dirty_rect must survive StartMedia wincode round-trip"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// DataChannel-only connections (browser file-management UI) ship
    /// `start_video=false, start_audio=false` so the worker skips both
    /// capture pipelines. Round-trip the negative case so a wincode
    /// schema bump that drops the new fields is caught here.
    #[test]
    fn start_media_data_channel_only_round_trips() {
        let msg = ServiceToWorker::StartMedia(StartMediaPayload {
            connection_id: "conn-files".to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: None,
            audio_device: None,
            fps: 0,
            bitrate_kbps: 0,
            quality: 0,
            start_video: false,
            start_audio: false,
            image_capture: None,
            enable_dirty_rect: None,
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::StartMedia(p) => {
                assert!(!p.start_video, "start_video=false must round-trip");
                assert!(!p.start_audio, "start_audio=false must round-trip");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `default_true` exists for the JSON deserialisation back-compat
    /// path: a payload missing both fields must default to "media on"
    /// so an old daemon talking to a new worker keeps the legacy
    /// behaviour. Bincode is positional and will not exercise this
    /// branch, so this test pokes the JSON path directly.
    #[test]
    fn start_media_json_missing_flags_defaults_to_media_on() {
        let json = r#"{
            "connection_id": "conn-legacy",
            "video_codec": "H264",
            "audio_codec": "Opus",
            "video_device": null,
            "audio_device": null,
            "fps": 30,
            "bitrate_kbps": 0,
            "quality": 0
        }"#;
        let payload: StartMediaPayload = serde_json::from_str(json).expect("parse");
        assert!(
            payload.start_video,
            "missing start_video must default to true"
        );
        assert!(
            payload.start_audio,
            "missing start_audio must default to true"
        );
    }

    /// `UpdateMediaSettings` carries the live-tune knobs the daemon
    /// sniffs out of an inbound `UpdateDeskSettings` and fans out to
    /// every active worker. Round-trip pins the field set (especially
    /// `enable_dirty_rect`) so a future schema bump that drops the
    /// dirty-rect flag fails this test instead of silently regressing
    /// the kill-switch back to "frontend toggle ignored".
    #[test]
    fn update_media_settings_round_trips_wincode_with_dirty_rect() {
        let msg = ServiceToWorker::UpdateMediaSettings(UpdateMediaSettingsPayload {
            connection_id: "conn-dr".to_string(),
            fps: Some(45),
            bitrate_kbps: None,
            quality: Some(22),
            enable_dirty_rect: Some(false),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::UpdateMediaSettings(p) => {
                assert_eq!(p.connection_id, "conn-dr");
                assert_eq!(p.fps, Some(45));
                assert_eq!(p.bitrate_kbps, None);
                assert_eq!(p.quality, Some(22));
                assert_eq!(
                    p.enable_dirty_rect,
                    Some(false),
                    "enable_dirty_rect must survive wincode round-trip"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JSON back-compat: a payload from an older daemon that does not
    /// know about `enable_dirty_rect` must deserialise with the field
    /// as `None` (meaning: "leave current setting alone") rather than
    /// erroring or defaulting to `Some(false)`.
    #[test]
    fn update_media_settings_json_missing_enable_dirty_rect_is_none() {
        let json = r#"{
            "connection_id": "conn-legacy",
            "fps": 30,
            "bitrate_kbps": null,
            "quality": 50
        }"#;
        let payload: UpdateMediaSettingsPayload = serde_json::from_str(json).expect("parse");
        assert_eq!(payload.enable_dirty_rect, None);
    }

    #[test]
    fn stop_media_round_trips_wincode() {
        let msg = ServiceToWorker::StopMedia(StopMediaPayload {
            connection_id: "conn-2".to_string(),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::StopMedia(p) => assert_eq!(p.connection_id, "conn-2"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn force_keyframe_round_trips_wincode() {
        let msg = ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
            connection_id: "conn-3".to_string(),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::ForceKeyframe(p) => assert_eq!(p.connection_id, "conn-3"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// MouseInput / MouseMoveInput / KeyboardInput share `InputPayload` —
    /// verify the variant tag survives round-trip (wincode discriminant).
    #[test]
    fn input_variants_distinguishable_after_round_trip() {
        let mouse = ServiceToWorker::MouseInput(InputPayload {
            connection_id: "c".to_string(),
            data: vec![1, 2, 3],
        });
        let mouse_move = ServiceToWorker::MouseMoveInput(InputPayload {
            connection_id: "c".to_string(),
            data: vec![1, 2, 3],
        });
        let keyboard = ServiceToWorker::KeyboardInput(InputPayload {
            connection_id: "c".to_string(),
            data: vec![1, 2, 3],
        });
        assert!(matches!(
            wincode_round_trip(&mouse),
            ServiceToWorker::MouseInput(_)
        ));
        assert!(matches!(
            wincode_round_trip(&mouse_move),
            ServiceToWorker::MouseMoveInput(_)
        ));
        assert!(matches!(
            wincode_round_trip(&keyboard),
            ServiceToWorker::KeyboardInput(_)
        ));
    }

    #[test]
    fn capabilities_round_trips_wincode() {
        use desk_signal_facade::model::audio_capture::{AudioDataFlow, AudioDevice};
        use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};

        let mut video_device_list: BTreeMap<String, Vec<DisplayInfo>> = BTreeMap::new();
        video_device_list.insert(
            "dxgi".to_string(),
            vec![DisplayInfo {
                device_name: "\\\\.\\DISPLAY1".to_string(),
                display_device_name: Some("Generic PnP Monitor".to_string()),
                desktop_coordinates: DisplayRect {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                },
                resolutions: vec![],
                attached_to_desktop: true,
                rotation: 0,
            }],
        );
        let mut audio_device_list: BTreeMap<String, Vec<AudioDevice>> = BTreeMap::new();
        audio_device_list.insert(
            "wasapi".to_string(),
            vec![AudioDevice {
                id: "mic-1".to_string(),
                firendly_name: "Microphone (Realtek)".to_string(),
                data_flow: AudioDataFlow::Capture,
                default: true,
            }],
        );
        let msg = WorkerToService::Capabilities(MediaCapabilities {
            video_codecs: vec![MediaCodec::H264, MediaCodec::Vp9],
            audio_codecs: vec![MediaCodec::Opus],
            video_encoders: vec!["X264".to_string(), "H264".to_string(), "VP9".to_string()],
            audio_encoders: vec!["OPUS".to_string()],
            video_device_list: video_device_list.clone(),
            audio_device_list: audio_device_list.clone(),
            has_tauri: true,
            is_admin: false,
            desktop_name: "Default".to_string(),
        });
        match wincode_round_trip(&msg) {
            WorkerToService::Capabilities(c) => {
                assert_eq!(c.video_codecs, vec![MediaCodec::H264, MediaCodec::Vp9]);
                assert_eq!(
                    c.video_encoders,
                    vec!["X264".to_string(), "H264".to_string(), "VP9".to_string()],
                    "X264 and H264 must remain distinct entries — the UI \
                     needs them to expose the libx264 vs OpenH264 choice"
                );
                assert_eq!(c.audio_encoders, vec!["OPUS".to_string()]);
                assert_eq!(c.video_device_list.len(), 1);
                assert_eq!(
                    c.video_device_list["dxgi"][0].device_name,
                    "\\\\.\\DISPLAY1"
                );
                assert_eq!(c.audio_device_list.len(), 1);
                assert_eq!(c.audio_device_list["wasapi"][0].id, "mic-1");
                assert!(c.has_tauri);
                assert!(!c.is_admin);
                assert_eq!(c.desktop_name, "Default");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `FileTransferPayload` carries an `is_text` flag alongside
    /// `connection_id` + `data`. Verify both true and false survive
    /// round-trip — a flipped bit would break the daemon's
    /// `dc.send_text` vs `dc.send` decision and corrupt downloads.
    /// Since the file-transfer payload now rides its own dedicated
    /// IPC lane (see `dual_transport::FILE_QUEUE_CAP`), this round-trip
    /// is on the bare `FileTransferPayload` struct rather than a
    /// `WorkerToService` / `ServiceToWorker` enum wrapper.
    #[test]
    fn file_transfer_payload_round_trip_preserves_is_text_flag() {
        for is_text in [true, false] {
            let original = FileTransferPayload {
                connection_id: "ft-1".to_string(),
                data: vec![1, 2, 3],
                is_text,
                transfer_id: None,
            };
            let decoded = wincode_round_trip(&original);
            assert_eq!(decoded.connection_id, "ft-1");
            assert_eq!(decoded.data, vec![1, 2, 3]);
            assert_eq!(decoded.is_text, is_text);
            assert!(decoded.transfer_id.is_none());
        }
    }

    /// `transfer_id` survives wincode round-trip in both `Some` and
    /// `None` form. The daemon-side writer task reads this field on
    /// `dc.send` failure and forwards it via
    /// [`ServiceToWorker::FileTransferSendFailed`] so the worker can
    /// abort the specific transfer rather than all transfers on the
    /// PC; losing the field would silently coarsen the abort scope.
    #[test]
    fn file_transfer_payload_transfer_id_round_trips_wincode() {
        for transfer_id in [
            None,
            Some("11111111-2222-3333-4444-555555555555".to_string()),
        ] {
            let original = FileTransferPayload {
                connection_id: "ft-1".to_string(),
                data: vec![1, 2, 3],
                is_text: false,
                transfer_id: transfer_id.clone(),
            };
            let decoded = wincode_round_trip(&original);
            assert_eq!(decoded.transfer_id, transfer_id);
        }
    }

    /// `FileTransferSendFailedPayload` survives a wincode round trip
    /// in every error-kind variant. The worker dispatches its abort
    /// policy off `kind`; a silent re-ordering of the enum would map
    /// `PacketTooLarge` to `Other` (or vice versa), demoting a
    /// configuration bug to a warning and skipping the
    /// fatal-transfer abort.
    #[test]
    fn file_transfer_send_failed_round_trips_all_kinds() {
        for kind in [
            FileTransferSendErrorKind::PacketTooLarge,
            FileTransferSendErrorKind::TransportClosed,
            FileTransferSendErrorKind::Other,
        ] {
            let msg = ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
                connection_id: "conn-ft".to_string(),
                transfer_id: Some("00000000-0000-0000-0000-000000000001".to_string()),
                chunk_index: Some(42),
                kind,
                error: "outbound packet too large".to_string(),
            });
            match wincode_round_trip(&msg) {
                ServiceToWorker::FileTransferSendFailed(p) => {
                    assert_eq!(p.connection_id, "conn-ft");
                    assert_eq!(
                        p.transfer_id.as_deref(),
                        Some("00000000-0000-0000-0000-000000000001")
                    );
                    assert_eq!(p.chunk_index, Some(42));
                    assert_eq!(p.kind, kind);
                    assert_eq!(p.error, "outbound packet too large");
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    /// Coarse-grained variant: `transfer_id` and `chunk_index` are
    /// optional so the daemon can still send a failure notification
    /// when it cannot attribute the failure to a specific transfer
    /// (e.g. the failing payload was a legacy chunk emitted before the
    /// worker started populating `transfer_id`). The worker treats
    /// `None` as "abort everything for this connection".
    #[test]
    fn file_transfer_send_failed_round_trips_without_transfer_id() {
        let msg = ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
            connection_id: "conn-ft".to_string(),
            transfer_id: None,
            chunk_index: None,
            kind: FileTransferSendErrorKind::TransportClosed,
            error: "channel closed".to_string(),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::FileTransferSendFailed(p) => {
                assert_eq!(p.connection_id, "conn-ft");
                assert!(p.transfer_id.is_none());
                assert!(p.chunk_index.is_none());
                assert_eq!(p.kind, FileTransferSendErrorKind::TransportClosed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Wincode payloads written by older binaries that pre-date the
    /// `transfer_id` field MUST still decode — `#[serde(default)]`
    /// makes the field optional so a worker built before the
    /// `FileTransferSendFailed` rollout can still hand chunks to a
    /// newer daemon (and vice versa) without an IPC framing mismatch.
    #[test]
    fn file_transfer_payload_accepts_legacy_json_without_transfer_id() {
        let legacy = serde_json::json!({
            "connection_id": "ft-1",
            "data": [1, 2, 3],
            "is_text": false,
        });
        let decoded: FileTransferPayload = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.connection_id, "ft-1");
        assert_eq!(decoded.data, vec![1, 2, 3]);
        assert!(!decoded.is_text);
        assert!(decoded.transfer_id.is_none());
    }

    /// `ErrorPayload.connection_id` survives a wincode round-trip in
    /// both `Some` and `None` forms. The daemon's `MediaTransportStuck`
    /// recovery path keys off this field — losing it would silently
    /// regress the self-heal we just wired up.
    #[test]
    fn error_payload_connection_id_round_trips_wincode() {
        let scoped = WorkerToService::Error(ErrorPayload {
            code: ERROR_CODE_MEDIA_TRANSPORT_STUCK,
            message: "stuck".to_string(),
            recoverable: true,
            connection_id: Some("conn-7".to_string()),
        });
        match wincode_round_trip(&scoped) {
            WorkerToService::Error(p) => {
                assert_eq!(p.code, ERROR_CODE_MEDIA_TRANSPORT_STUCK);
                assert_eq!(p.connection_id.as_deref(), Some("conn-7"));
                assert!(p.recoverable);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let global = WorkerToService::Error(ErrorPayload {
            code: -1,
            message: "init failed".to_string(),
            recoverable: false,
            connection_id: None,
        });
        match wincode_round_trip(&global) {
            WorkerToService::Error(p) => {
                assert_eq!(p.code, -1);
                assert!(p.connection_id.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JSON payloads emitted by older binaries that pre-date the
    /// `connection_id` field must still decode (the `#[serde(default)]`
    /// attribute is what makes this work).
    #[test]
    fn error_payload_accepts_legacy_json_without_connection_id() {
        let legacy = serde_json::json!({
            "code": -1,
            "message": "boom",
            "recoverable": false,
        });
        let decoded: ErrorPayload = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.code, -1);
        assert!(!decoded.recoverable);
        assert!(decoded.connection_id.is_none());
    }

    /// `ERROR_CODE_MEDIA_TRANSPORT_STUCK` is part of the IPC contract;
    /// pin its numeric value so a refactor that accidentally renames or
    /// shadows it shows up as a test failure.
    #[test]
    fn media_transport_stuck_error_code_is_stable() {
        assert_eq!(ERROR_CODE_MEDIA_TRANSPORT_STUCK, -1001);
    }

    /// `MediaFrame` is the hot path on the media transport — sanity check
    /// 200 KB P-frame size encodes/decodes cleanly.
    #[test]
    fn media_frame_round_trips_wincode_200kb() {
        let payload = vec![0xABu8; 200 * 1024];
        let original = MediaFrame {
            connection_id: "conn-1".to_string(),
            seq: 42,
            ts_ns: 1_700_000_000_000_000_000,
            duration_ns: 16_666_666,
            kind: MediaFrameKind::VideoP,
            codec: MediaCodec::H264,
            payload: payload.clone(),
        };
        let decoded = wincode_round_trip(&original);
        assert_eq!(decoded.connection_id, "conn-1");
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.kind, MediaFrameKind::VideoP);
        assert_eq!(decoded.payload.len(), payload.len());
        assert_eq!(decoded.payload, payload);
    }

    // === Typed control plane — round-trip tests ===

    /// `EnablePrivateScreen` carries the same bool the legacy
    /// `EnablePrivateScreenData` JSON used. Round-tripping it under
    /// wincode pins the wire shape — a reorder of `connection_id`
    /// vs `enable` would silently flip enable-vs-disable on
    /// matched-version daemon/worker pairs.
    #[test]
    fn enable_private_screen_round_trips_wincode() {
        for enable in [true, false] {
            let msg = ServiceToWorker::EnablePrivateScreen(EnablePrivateScreenPayload {
                connection_id: "conn-priv".to_string(),
                enable,
            });
            match wincode_round_trip(&msg) {
                ServiceToWorker::EnablePrivateScreen(p) => {
                    assert_eq!(p.connection_id, "conn-priv");
                    assert_eq!(p.enable, enable);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    /// `UpdateDeskSettings` ferries `DeskSettings` over the wincode
    /// derive. Verify non-default media + non-media
    /// fields both survive — these are the ones the worker's
    /// `handle_update_desk_settings` and the daemon's
    /// `broadcast_media_settings_update` both read.
    #[test]
    fn update_desk_settings_round_trips_wincode() {
        let settings = DeskSettings {
            video_fps: 45,
            video_quality: 33,
            wayland_control_mode: Some("portal".to_string()),
            ..DeskSettings::default()
        };
        let msg = ServiceToWorker::UpdateDeskSettings(UpdateDeskSettingsPayload {
            connection_id: "conn-uds".to_string(),
            settings: settings.clone(),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::UpdateDeskSettings(p) => {
                assert_eq!(p.connection_id, "conn-uds");
                assert_eq!(p.settings.video_fps, 45);
                assert_eq!(p.settings.video_quality, 33);
                assert_eq!(p.settings.wayland_control_mode.as_deref(), Some("portal"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `PrivateScreenStateChanged` is the reverse path (worker →
    /// daemon → browser). Round-trip with `is_supported = false` +
    /// an `error_msg` so a future schema change to
    /// `PrivateScreenStateChangedData` shows up as a test failure
    /// rather than as a silent wire-format drift.
    #[test]
    fn private_screen_state_changed_round_trips_wincode() {
        let msg = WorkerToService::PrivateScreenStateChanged(PrivateScreenStateChangedPayload {
            connection_id: "conn-pss".to_string(),
            data: PrivateScreenStateChangedData {
                visible: false,
                is_supported: false,
                error_msg: Some("hub denied".to_string()),
            },
        });
        match wincode_round_trip(&msg) {
            WorkerToService::PrivateScreenStateChanged(p) => {
                assert_eq!(p.connection_id, "conn-pss");
                assert!(!p.data.visible);
                assert!(!p.data.is_supported);
                assert_eq!(p.data.error_msg.as_deref(), Some("hub denied"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // === Manager plane — round-trip tests ===

    /// Body-less manager request envelopes carry only `request_id` +
    /// `connection_id`; verify the field order survives wincode (a
    /// reorder would silently swap them on matched-version pairs).
    #[test]
    fn manager_request_ref_round_trips_wincode() {
        let msg = ServiceToWorker::ManagerSystemInfoRequest(ManagerRequestRefPayload {
            request_id: "req-info-1".to_string(),
            connection_id: Some("conn-mgr".to_string()),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::ManagerSystemInfoRequest(p) => {
                assert_eq!(p.request_id, "req-info-1");
                assert_eq!(p.connection_id.as_deref(), Some("conn-mgr"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // HTTP-API-triggered manager requests (e.g.
        // `signal-facade::controller::sysinfo`) have no originating
        // browser PC; verify a `None` connection_id round-trips so
        // the daemon's manager handlers don't drop the request.
        let none_msg = ServiceToWorker::ManagerSystemInfoRequest(ManagerRequestRefPayload {
            request_id: "req-info-no-conn".to_string(),
            connection_id: None,
        });
        match wincode_round_trip(&none_msg) {
            ServiceToWorker::ManagerSystemInfoRequest(p) => {
                assert!(p.connection_id.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ManagerFileListRequest` ferries `FileListParams` (carries 4
    /// `Option<DateTime<Local>>` fields via the wincode chrono adapter).
    /// Use a non-default page_count (and filename
    /// filter) so a stripped field shows up as a test failure.
    #[test]
    fn manager_file_list_request_round_trips_wincode() {
        let params = FileListParams {
            path: "C:\\Users".to_string(),
            page_no: 2,
            page_count: 50,
            file_name: Some("readme".to_string()),
            ..Default::default()
        };
        let msg = ServiceToWorker::ManagerFileListRequest(ManagerFileListRequestPayload {
            request_id: "req-fl".to_string(),
            connection_id: Some("conn-fl".to_string()),
            params,
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::ManagerFileListRequest(p) => {
                assert_eq!(p.request_id, "req-fl");
                assert_eq!(p.connection_id.as_deref(), Some("conn-fl"));
                assert_eq!(p.params.path, "C:\\Users");
                assert_eq!(p.params.page_no, 2);
                assert_eq!(p.params.page_count, 50);
                assert_eq!(p.params.file_name.as_deref(), Some("readme"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ManagerUpdateSettingsRequest` ferries `RemoteSystemSettings`
    /// over the wincode derive. Round-trip a
    /// non-default payload so a reorder/strip in the facade struct
    /// trips here rather than silently corrupting persisted settings.
    #[test]
    fn manager_update_settings_request_round_trips_wincode() {
        let settings = RemoteSystemSettings {
            enable_ipv6: true,
            port: 8443,
            listen_addr_ipv4: "0.0.0.0".to_string(),
            listen_addr_ipv6: "::".to_string(),
            locale: Some("zh-CN".to_string()),
            signaling_url: Some("wss://signal.example".to_string()),
            signaling_token: Some("tok".to_string()),
            manager_url: Some("https://mgr.example".to_string()),
            auto_start: Some(true),
            manager_api_token: Some("mtok".to_string()),
        };
        let msg =
            ServiceToWorker::ManagerUpdateSettingsRequest(ManagerUpdateSettingsRequestPayload {
                request_id: "req-upd".to_string(),
                connection_id: Some("conn-upd".to_string()),
                settings,
            });
        match wincode_round_trip(&msg) {
            ServiceToWorker::ManagerUpdateSettingsRequest(p) => {
                assert_eq!(p.request_id, "req-upd");
                assert!(p.settings.enable_ipv6);
                assert_eq!(p.settings.port, 8443);
                assert_eq!(p.settings.locale.as_deref(), Some("zh-CN"));
                assert_eq!(p.settings.auto_start, Some(true));
                assert_eq!(p.settings.manager_api_token.as_deref(), Some("mtok"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Body-less manager response envelopes are distinct from request
    /// envelopes at the type level; round-trip pins the variant tag.
    #[test]
    fn manager_response_ref_round_trips_wincode() {
        let msg = WorkerToService::ManagerFileDeleteResponse(ManagerResponseRefPayload {
            request_id: "req-del".to_string(),
            connection_id: Some("conn-del".to_string()),
        });
        match wincode_round_trip(&msg) {
            WorkerToService::ManagerFileDeleteResponse(p) => {
                assert_eq!(p.request_id, "req-del");
                assert_eq!(p.connection_id.as_deref(), Some("conn-del"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ManagerSystemInfoResponse` carries the full `SystemInfo`
    /// blob; verify `startup_mode` + `is_admin` survive (the legacy
    /// handler set both at runtime so they are the most likely
    /// round-trip regression points).
    #[test]
    fn manager_system_info_response_round_trips_wincode() {
        let info = SystemInfo {
            name: Some("alice-pc".to_string()),
            is_admin: Some(true),
            ..SystemInfo::default()
        };
        let msg = WorkerToService::ManagerSystemInfoResponse(ManagerSystemInfoResponsePayload {
            request_id: "req-info".to_string(),
            connection_id: Some("conn-info".to_string()),
            info,
        });
        match wincode_round_trip(&msg) {
            WorkerToService::ManagerSystemInfoResponse(p) => {
                assert_eq!(p.request_id, "req-info");
                assert_eq!(p.info.name.as_deref(), Some("alice-pc"));
                assert_eq!(p.info.is_admin, Some(true));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // === Terminal plane — round-trip tests ===

    /// `StartTerminalRequest` ferries `StartTerminalSession` over the
    /// wincode derive. A non-trivial `command` (with
    /// comma-separated args) survives the round-trip — a stripped or
    /// reordered field would break terminal launch on matched-version
    /// daemon/worker pairs.
    #[test]
    fn start_terminal_request_round_trips_wincode() {
        let msg = ServiceToWorker::StartTerminalRequest(StartTerminalRequestPayload {
            request_id: "req-start".to_string(),
            connection_id: "conn-term".to_string(),
            session: StartTerminalSession {
                command: "C:\\Windows\\System32\\cmd.exe,/k,echo,hello".to_string(),
                device_id: None,
                grant_session_id: None,
            },
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::StartTerminalRequest(p) => {
                assert_eq!(p.request_id, "req-start");
                assert_eq!(p.connection_id, "conn-term");
                assert_eq!(
                    p.session.command,
                    "C:\\Windows\\System32\\cmd.exe,/k,echo,hello"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `SendDataToTerminalRequest` is the keystroke / paste path —
    /// arbitrary UTF-8 (including newlines + escape codes) must
    /// round-trip verbatim.
    #[test]
    fn send_data_to_terminal_request_round_trips_wincode() {
        let msg = ServiceToWorker::SendDataToTerminalRequest(SendDataToTerminalPayload {
            connection_id: "conn-term".to_string(),
            data: TerminalInputData {
                content: "ls -la\n\x1b[1;31mred\x1b[0m\n".to_string(),
            },
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::SendDataToTerminalRequest(p) => {
                assert_eq!(p.connection_id, "conn-term");
                assert_eq!(p.data.content, "ls -la\n\x1b[1;31mred\x1b[0m\n");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ResizeTerminalRequest` carries a u16 rows × cols pair; pin
    /// the round-trip so a future field reorder does not silently
    /// swap rows and cols at the wire.
    #[test]
    fn resize_terminal_request_round_trips_wincode() {
        let msg = ServiceToWorker::ResizeTerminalRequest(ResizeTerminalPayload {
            connection_id: "conn-term".to_string(),
            data: TerminalResizeData {
                rows: 50,
                cols: 200,
            },
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::ResizeTerminalRequest(p) => {
                assert_eq!(p.connection_id, "conn-term");
                assert_eq!(p.data.rows, 50);
                assert_eq!(p.data.cols, 200);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `CloseTerminalRequest` and `ListTerminalRequest` are body-less;
    /// verify the variant tag survives wincode (a reorder of the
    /// terminal-plane variants would silently misroute one onto the
    /// other on matched-version pairs).
    #[test]
    fn close_and_list_terminal_requests_round_trip_wincode() {
        let close = ServiceToWorker::CloseTerminalRequest(CloseTerminalPayload {
            connection_id: "conn-term".to_string(),
        });
        assert!(matches!(
            wincode_round_trip(&close),
            ServiceToWorker::CloseTerminalRequest(_)
        ));

        let list = ServiceToWorker::ListTerminalRequest(ListTerminalRequestPayload {
            request_id: "req-list".to_string(),
            connection_id: Some("conn-list".to_string()),
        });
        match wincode_round_trip(&list) {
            ServiceToWorker::ListTerminalRequest(p) => {
                assert_eq!(p.request_id, "req-list");
                assert_eq!(p.connection_id.as_deref(), Some("conn-list"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // HTTP-API-triggered list_terminal (signal-facade controller)
        // dispatches with no `from_connection_id`; verify `None`
        // round-trips so the daemon's terminal handler doesn't drop
        // it.
        let list_no_conn = ServiceToWorker::ListTerminalRequest(ListTerminalRequestPayload {
            request_id: "req-list-no-conn".to_string(),
            connection_id: None,
        });
        match wincode_round_trip(&list_no_conn) {
            ServiceToWorker::ListTerminalRequest(p) => {
                assert!(p.connection_id.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `TerminalStarted` is the success response for `StartTerminal`.
    /// Empty body — `request_id` correlates back to the original
    /// `StartTerminalRequest`. Verify the variant survives wincode
    /// alongside `TerminalClosed` (notification, no `request_id`)
    /// so the daemon's reverse-direction code can keep them
    /// straight.
    #[test]
    fn terminal_started_and_closed_round_trip_wincode() {
        let started = WorkerToService::TerminalStarted(TerminalStartedPayload {
            request_id: "req-start".to_string(),
            connection_id: "conn-term".to_string(),
        });
        match wincode_round_trip(&started) {
            WorkerToService::TerminalStarted(p) => {
                assert_eq!(p.request_id, "req-start");
                assert_eq!(p.connection_id, "conn-term");
            }
            other => panic!("unexpected: {other:?}"),
        }

        let closed = WorkerToService::TerminalClosed(TerminalClosedPayload {
            connection_id: "conn-term".to_string(),
        });
        match wincode_round_trip(&closed) {
            WorkerToService::TerminalClosed(p) => {
                assert_eq!(p.connection_id, "conn-term");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ReplyFromTerminal` is the high-frequency PTY-output path.
    /// Verify a reasonably large chunk (4 KB — well above the
    /// worker's 1 KB read buffer to leave headroom) survives wincode
    /// without truncation.
    #[test]
    fn reply_from_terminal_round_trips_wincode_with_large_chunk() {
        let body = "abcdefgh".repeat(512); // 4 KB
        let msg = WorkerToService::ReplyFromTerminal(ReplyFromTerminalPayload {
            connection_id: "conn-term".to_string(),
            data: TerminalOutputData {
                content: body.clone(),
            },
        });
        match wincode_round_trip(&msg) {
            WorkerToService::ReplyFromTerminal(p) => {
                assert_eq!(p.connection_id, "conn-term");
                assert_eq!(p.data.content.len(), body.len());
                assert_eq!(p.data.content, body);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // === Additional payloads — round-trip tests ===

    /// `SignalingError.signaling_type` is `SignalingType`, a facade enum
    /// whose wincode derive uses `tag_encoding = "i32"` matched to its
    /// `#[repr(i32)]` discriminants. Round-trip a representative
    /// type + non-zero `error_code` + an explicit message so a
    /// wire-format drift on any field shows up as a test failure rather
    /// than a silent corruption that swaps which `SignalingType` the
    /// browser thinks the error belongs to.
    #[test]
    fn signaling_error_round_trips_wincode() {
        let msg = WorkerToService::SignalingError(SignalingErrorPayload {
            request_id: "req-err-1".to_string(),
            connection_id: "conn-err".to_string(),
            signaling_type: SignalingType::StartTerminal,
            error_code: 401,
            error_message: Some("Permission denied".to_string()),
        });
        match wincode_round_trip(&msg) {
            WorkerToService::SignalingError(p) => {
                assert_eq!(p.request_id, "req-err-1");
                assert_eq!(p.connection_id, "conn-err");
                assert_eq!(p.signaling_type, SignalingType::StartTerminal);
                assert_eq!(p.error_code, 401);
                assert_eq!(p.error_message.as_deref(), Some("Permission denied"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        // `error_message` is Option<String>; verify the None case
        // (some send_error callers omit a message).
        let msg = WorkerToService::SignalingError(SignalingErrorPayload {
            request_id: "req-err-2".to_string(),
            connection_id: "conn-err".to_string(),
            signaling_type: SignalingType::ManagerFileList,
            error_code: -1,
            error_message: None,
        });
        match wincode_round_trip(&msg) {
            WorkerToService::SignalingError(p) => {
                assert_eq!(p.error_code, -1);
                assert!(p.error_message.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ListTerminalResponse` ferries `TerminalList` over the wincode
    /// derive. Round-trip a non-empty list so a
    /// stripped field shows up as a test failure rather than a silent
    /// wire-format drift.
    #[test]
    fn list_terminal_response_round_trips_wincode() {
        let terminals = TerminalList {
            commands: vec![
                vec!["C:\\Windows\\System32\\cmd.exe".to_string()],
                vec!["C:\\Program Files\\PowerShell\\7\\pwsh.exe".to_string()],
            ],
            current: 1,
        };
        let msg = WorkerToService::ListTerminalResponse(ListTerminalResponsePayload {
            request_id: "req-list".to_string(),
            connection_id: Some("conn-list".to_string()),
            terminals,
        });
        match wincode_round_trip(&msg) {
            WorkerToService::ListTerminalResponse(p) => {
                assert_eq!(p.request_id, "req-list");
                assert_eq!(p.connection_id.as_deref(), Some("conn-list"));
                assert_eq!(p.terminals.commands.len(), 2);
                assert_eq!(p.terminals.current, 1);
                assert_eq!(p.terminals.commands[0][0], "C:\\Windows\\System32\\cmd.exe");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // === ServiceToWorker / WorkerToService full-variant coverage ===

    /// Exhaustive `ServiceToWorker` round-trip. Per-variant tests above
    /// cover the field-level invariants for the high-traffic variants;
    /// this table-driven test guarantees **every** variant — including
    /// the body-less ones (`Shutdown`) and the manager / terminal
    /// envelopes — has wincode `SchemaWrite` + `SchemaRead` wired up
    /// before e2e. Any variant missing a derive or carrying a payload
    /// without wincode support breaks here, not on the wire.
    ///
    /// If the compiler complains about a non-exhaustive `match` after a
    /// new `ServiceToWorker` variant is added, extend the `cases`
    /// vec **and** the matching round-trip discriminant check below.
    #[test]
    fn service_to_worker_all_variants_round_trip() {
        let cases: Vec<ServiceToWorker> = vec![
            ServiceToWorker::Init(WorkerInitPayload {
                session_id: "s".to_string(),
                os_session_id: 1,
                desktop_name: Some("Default".to_string()),
                config_json: "{}".to_string(),
                signaling_url: None,
                auth_token: None,
                host_upstream_url: None,
                media_pipe_name: None,
                file_pipe_name: None,
                config_file_path: None,
            }),
            ServiceToWorker::Shutdown,
            ServiceToWorker::StartMedia(StartMediaPayload {
                connection_id: "c".to_string(),
                video_codec: MediaCodec::H264,
                audio_codec: MediaCodec::Opus,
                video_device: None,
                audio_device: None,
                fps: 30,
                bitrate_kbps: 4_000,
                quality: 0,
                start_video: true,
                start_audio: true,
                image_capture: None,
                enable_dirty_rect: None,
            }),
            ServiceToWorker::StopMedia(StopMediaPayload {
                connection_id: "c".to_string(),
            }),
            ServiceToWorker::UpdateMediaSettings(UpdateMediaSettingsPayload {
                connection_id: "c".to_string(),
                fps: Some(60),
                bitrate_kbps: Some(6_000),
                quality: Some(50),
                enable_dirty_rect: Some(false),
            }),
            ServiceToWorker::ForceKeyframe(ForceKeyframePayload {
                connection_id: "c".to_string(),
            }),
            ServiceToWorker::MouseInput(InputPayload {
                connection_id: "c".to_string(),
                data: vec![1, 2, 3],
            }),
            ServiceToWorker::MouseMoveInput(InputPayload {
                connection_id: "c".to_string(),
                data: vec![4, 5, 6],
            }),
            ServiceToWorker::KeyboardInput(InputPayload {
                connection_id: "c".to_string(),
                data: vec![7, 8, 9],
            }),
            ServiceToWorker::ClipboardWrite(ClipboardPayload {
                connection_id: "c".to_string(),
                data: vec![0xAA],
            }),
            ServiceToWorker::ClipboardRequest(ConnectionRefPayload {
                connection_id: "c".to_string(),
            }),
            ServiceToWorker::WhiteboardCommand(OpaqueConnectionPayload {
                connection_id: "c".to_string(),
                data: vec![0xBB, 0xCC],
            }),
            ServiceToWorker::EnablePrivateScreen(EnablePrivateScreenPayload {
                connection_id: "c".to_string(),
                enable: true,
            }),
            ServiceToWorker::UpdateDeskSettings(UpdateDeskSettingsPayload {
                connection_id: "c".to_string(),
                settings: DeskSettings::default(),
            }),
            ServiceToWorker::ManagerSystemInfoRequest(ManagerRequestRefPayload {
                request_id: "r1".to_string(),
                connection_id: Some("c".to_string()),
            }),
            ServiceToWorker::ManagerFileListRequest(ManagerFileListRequestPayload {
                request_id: "r2".to_string(),
                connection_id: Some("c".to_string()),
                params: FileListParams::default(),
            }),
            ServiceToWorker::ManagerFileDeleteRequest(ManagerFileDeleteRequestPayload {
                request_id: "r3".to_string(),
                connection_id: Some("c".to_string()),
                request: DeleteFileRequest::default(),
            }),
            ServiceToWorker::ManagerQuerySettingsRequest(ManagerRequestRefPayload {
                request_id: "r4".to_string(),
                connection_id: None,
            }),
            ServiceToWorker::ManagerUpdateSettingsRequest(ManagerUpdateSettingsRequestPayload {
                request_id: "r5".to_string(),
                connection_id: Some("c".to_string()),
                settings: RemoteSystemSettings::default(),
            }),
            ServiceToWorker::StartTerminalRequest(StartTerminalRequestPayload {
                request_id: "r6".to_string(),
                connection_id: "c".to_string(),
                session: StartTerminalSession {
                    command: "cmd.exe".to_string(),
                    device_id: None,
                    grant_session_id: None,
                },
            }),
            ServiceToWorker::SendDataToTerminalRequest(SendDataToTerminalPayload {
                connection_id: "c".to_string(),
                data: TerminalInputData {
                    content: "ls\n".to_string(),
                },
            }),
            ServiceToWorker::ResizeTerminalRequest(ResizeTerminalPayload {
                connection_id: "c".to_string(),
                data: TerminalResizeData { rows: 24, cols: 80 },
            }),
            ServiceToWorker::CloseTerminalRequest(CloseTerminalPayload {
                connection_id: "c".to_string(),
            }),
            ServiceToWorker::ListTerminalRequest(ListTerminalRequestPayload {
                request_id: "r7".to_string(),
                connection_id: Some("c".to_string()),
            }),
            ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
                connection_id: "c".to_string(),
                transfer_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
                chunk_index: Some(0),
                kind: FileTransferSendErrorKind::PacketTooLarge,
                error: "outbound packet too large".to_string(),
            }),
            ServiceToWorker::SetVirtualDisplayMode(SetVirtualDisplayModePayload {
                request_id: "r8".to_string(),
                connection_id: "c".to_string(),
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }),
            ServiceToWorker::AttachVirtualDisplay(AttachVirtualDisplayPayload {
                instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
            }),
            ServiceToWorker::DetachVirtualDisplay,
            ServiceToWorker::RefreshCapabilities,
            ServiceToWorker::AgentRequest(AgentRequestPayload {
                request_id: "r-ai".to_string(),
                connection_id: Some("c".to_string()),
                envelope: sample_agent_envelope(),
            }),
            ServiceToWorker::ExecPlan(ExecPlanPayload {
                request_id: "r-exec".to_string(),
                connection_id: Some("c".to_string()),
                plan: sample_exec_plan(),
                audit_source_request_id: Some("frame-req".to_string()),
            }),
        ];
        for case in &cases {
            let decoded = wincode_round_trip(case);
            // Discriminant equality is enough — per-variant payload
            // assertions live in the variant-specific tests above.
            assert_eq!(
                std::mem::discriminant(case),
                std::mem::discriminant(&decoded),
                "variant {case:?} did not round-trip to the same discriminant"
            );
        }
    }

    /// Exhaustive `WorkerToService` round-trip. See
    /// [`service_to_worker_all_variants_round_trip`] for the rationale.
    #[test]
    fn worker_to_service_all_variants_round_trip() {
        let cases: Vec<WorkerToService> = vec![
            WorkerToService::Ready,
            WorkerToService::Capabilities(MediaCapabilities::default()),
            WorkerToService::SignalingError(SignalingErrorPayload {
                request_id: "r".to_string(),
                connection_id: "c".to_string(),
                signaling_type: SignalingType::Error,
                error_code: 1,
                error_message: None,
            }),
            WorkerToService::Heartbeat(HeartbeatPayload {
                timestamp_ms: 1,
                active_connections: 0,
                cpu_usage: None,
                memory_usage: None,
            }),
            WorkerToService::DesktopChanged(DesktopChangedPayload {
                name: "Default".to_string(),
            }),
            WorkerToService::Error(ErrorPayload {
                code: -1,
                message: "boom".to_string(),
                recoverable: false,
                connection_id: None,
            }),
            WorkerToService::ClipboardRead(ClipboardPayload {
                connection_id: "c".to_string(),
                data: vec![0xDE, 0xAD],
            }),
            WorkerToService::CursorData(CursorDataPayload {
                connection_id: "c".to_string(),
                data: vec![0xBE, 0xEF],
            }),
            WorkerToService::PrivateScreenStateChanged(PrivateScreenStateChangedPayload {
                connection_id: "c".to_string(),
                data: PrivateScreenStateChangedData {
                    visible: true,
                    is_supported: true,
                    error_msg: None,
                },
            }),
            WorkerToService::ManagerSystemInfoResponse(ManagerSystemInfoResponsePayload {
                request_id: "r".to_string(),
                connection_id: Some("c".to_string()),
                info: SystemInfo::default(),
            }),
            WorkerToService::ManagerFileListResponse(ManagerFileListResponsePayload {
                request_id: "r".to_string(),
                connection_id: Some("c".to_string()),
                response: FileListResponse {
                    file_info_list: vec![],
                    total_count: 0,
                },
            }),
            WorkerToService::ManagerFileDeleteResponse(ManagerResponseRefPayload {
                request_id: "r".to_string(),
                connection_id: Some("c".to_string()),
            }),
            WorkerToService::ManagerQuerySettingsResponse(ManagerQuerySettingsResponsePayload {
                request_id: "r".to_string(),
                connection_id: None,
                settings: RemoteSystemSettings::default(),
            }),
            WorkerToService::ManagerUpdateSettingsResponse(ManagerResponseRefPayload {
                request_id: "r".to_string(),
                connection_id: Some("c".to_string()),
            }),
            WorkerToService::TerminalStarted(TerminalStartedPayload {
                request_id: "r".to_string(),
                connection_id: "c".to_string(),
            }),
            WorkerToService::TerminalClosed(TerminalClosedPayload {
                connection_id: "c".to_string(),
            }),
            WorkerToService::ReplyFromTerminal(ReplyFromTerminalPayload {
                connection_id: "c".to_string(),
                data: TerminalOutputData {
                    content: "hi".to_string(),
                },
            }),
            WorkerToService::ListTerminalResponse(ListTerminalResponsePayload {
                request_id: "r".to_string(),
                connection_id: Some("c".to_string()),
                terminals: TerminalList {
                    commands: vec![],
                    current: 0,
                },
            }),
            WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
                request_id: "r".to_string(),
                connection_id: "c".to_string(),
                outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                }),
            }),
            WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
                instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
            }),
            WorkerToService::AgentResponse(AgentResponsePayload {
                request_id: "r-ai".to_string(),
                connection_id: Some("c".to_string()),
                outcome: desk_agent_protocol::AgentOutcome::Err(desk_agent_protocol::AgentError {
                    kind: desk_agent_protocol::AgentErrorKind::Internal,
                    message: "x".to_string(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }),
            }),
            WorkerToService::ExecResult(ExecResultIpcPayload {
                request_id: "r-exec".to_string(),
                connection_id: Some("c".to_string()),
                result: desk_agent_protocol::exec::ExecResultPayload {
                    exec_request_id: desk_agent_protocol::exec::ExecRequestId("e1".to_string()),
                    outcome: desk_agent_protocol::AgentOutcome::Err(
                        desk_agent_protocol::AgentError {
                            kind: desk_agent_protocol::AgentErrorKind::Timeout,
                            message: "x".to_string(),
                            retryable: false,
                            safe_for_model: true,
                            error_code: None,
                        },
                    ),
                },
                audit_source_request_id: Some("frame-req".to_string()),
            }),
        ];
        for case in &cases {
            let decoded = wincode_round_trip(case);
            assert_eq!(
                std::mem::discriminant(case),
                std::mem::discriminant(&decoded),
                "variant {case:?} did not round-trip to the same discriminant"
            );
        }
    }

    // === SignalingErrorPayload full SignalingType coverage ===

    /// `SignalingErrorPayload.signaling_type` rides the wincode tag
    /// on the `SignalingType` enum. Iterate every one of
    /// the 36 variants so a missing `#[wincode(tag = N)]` (or a
    /// wrongly-numbered one) surfaces here instead of as a silent
    /// browser-side mismatch on a SignalingError reply.
    #[test]
    fn signaling_error_round_trips_wincode_for_every_signaling_type() {
        let all_types = [
            SignalingType::Heartbeat,
            SignalingType::FetchConnections,
            SignalingType::ConnectionList,
            SignalingType::ConnectionRemoved,
            SignalingType::RequestRemote,
            SignalingType::Init,
            SignalingType::Offer,
            SignalingType::Answer,
            SignalingType::Canid,
            SignalingType::RequireControl,
            SignalingType::AcceptControl,
            SignalingType::DenyControl,
            SignalingType::CloseControl,
            SignalingType::ChangeDisplaySettings,
            SignalingType::EnablePrivateScreen,
            SignalingType::PrivateScreenStateChanged,
            SignalingType::AudioPlaybackError,
            SignalingType::UpdateDeskSettings,
            SignalingType::ManagerSystemInfo,
            SignalingType::ManagerSystemStatue,
            SignalingType::ManagerFileList,
            SignalingType::ManagerFileDelete,
            SignalingType::StartTerminal,
            SignalingType::SendDataToTerminal,
            SignalingType::ResizeTerminal,
            SignalingType::CloseTerminal,
            SignalingType::ReplyFromTerminal,
            SignalingType::ListTerminal,
            SignalingType::TerminalStarted,
            SignalingType::TerminalClosed,
            SignalingType::ManagerQuerySettings,
            SignalingType::ManagerUpdateSettings,
            SignalingType::DesktopSwitching,
            SignalingType::DesktopReady,
            SignalingType::Error,
            SignalingType::Unknown,
        ];
        assert_eq!(
            all_types.len(),
            36,
            "SignalingType variant count drift — add new variant + tag here"
        );
        for ty in all_types {
            let original = WorkerToService::SignalingError(SignalingErrorPayload {
                request_id: format!("req-{}", ty as i32),
                connection_id: "c".to_string(),
                signaling_type: ty,
                error_code: ty as i32,
                error_message: Some(format!("{ty:?}")),
            });
            match wincode_round_trip(&original) {
                WorkerToService::SignalingError(p) => {
                    assert_eq!(
                        p.signaling_type as i32, ty as i32,
                        "signaling_type discriminant drift for {ty:?}"
                    );
                    assert_eq!(p.error_code, ty as i32);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    // ============== Virtual display variants ==============

    #[test]
    fn set_virtual_display_mode_round_trips_wincode() {
        let msg = ServiceToWorker::SetVirtualDisplayMode(SetVirtualDisplayModePayload {
            request_id: "req-1".to_string(),
            connection_id: "conn-1".to_string(),
            width: 2560,
            height: 1440,
            refresh_hz: 144,
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::SetVirtualDisplayMode(p) => {
                assert_eq!(p.request_id, "req-1");
                assert_eq!(p.connection_id, "conn-1");
                assert_eq!(p.width, 2560);
                assert_eq!(p.height, 1440);
                assert_eq!(p.refresh_hz, 144);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn set_virtual_display_mode_round_trips_serde_json() {
        let msg = ServiceToWorker::SetVirtualDisplayMode(SetVirtualDisplayModePayload {
            request_id: "req-1".to_string(),
            connection_id: "conn-1".to_string(),
            width: 1280,
            height: 720,
            refresh_hz: 60,
        });
        let json = serde_json::to_string(&msg).expect("encode");
        let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
        match back {
            ServiceToWorker::SetVirtualDisplayMode(p) => {
                assert_eq!(p.width, 1280);
                assert_eq!(p.height, 720);
                assert_eq!(p.refresh_hz, 60);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn attach_virtual_display_round_trips_wincode() {
        let msg = ServiceToWorker::AttachVirtualDisplay(AttachVirtualDisplayPayload {
            instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::AttachVirtualDisplay(p) => {
                assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn attach_virtual_display_round_trips_serde_json() {
        let msg = ServiceToWorker::AttachVirtualDisplay(AttachVirtualDisplayPayload {
            instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
        });
        let json = serde_json::to_string(&msg).expect("encode");
        let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
        match back {
            ServiceToWorker::AttachVirtualDisplay(p) => {
                assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn virtual_display_attach_outcome_attached_wincode_roundtrip() {
        let original =
            WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
                instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
            });
        match wincode_round_trip(&original) {
            WorkerToService::VirtualDisplayAttachResult(p) => {
                assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
                assert_eq!(
                    p.outcome,
                    VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
                );
            }
            other => panic!("unexpected variant after wincode round-trip: {other:?}"),
        }
    }

    #[test]
    fn virtual_display_attach_outcome_failed_wincode_roundtrip() {
        let original =
            WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
                instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
                outcome: VirtualDisplayAttachOutcome::Failed(
                    "find_display_name: seen=[] after 6 retries".to_string(),
                ),
            });
        match wincode_round_trip(&original) {
            WorkerToService::VirtualDisplayAttachResult(p) => {
                assert_eq!(p.instance_id, "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay");
                assert!(
                    matches!(p.outcome, VirtualDisplayAttachOutcome::Failed(ref msg) if msg.contains("seen=[]")),
                    "expected Failed with diagnostic message, got {:?}",
                    p.outcome
                );
            }
            other => panic!("unexpected variant after wincode round-trip: {other:?}"),
        }
    }

    #[test]
    fn worker_to_service_virtual_display_attach_result_serde_attached_and_failed() {
        // serde JSON is the on-wire form used by anything that bridges
        // the wincode IPC frames out to text (e.g. log diagnostics or
        // future REST-shaped tooling). Cover both Attached + Failed
        // variants so a future enum tweak (renaming, changing tag
        // attributes) gets flagged here.
        for case in [
            WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
                instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
                outcome: VirtualDisplayAttachOutcome::Attached("\\\\.\\DISPLAY4".to_string()),
            }),
            WorkerToService::VirtualDisplayAttachResult(VirtualDisplayAttachResultPayload {
                instance_id: "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay".to_string(),
                outcome: VirtualDisplayAttachOutcome::Failed("driver pipe IO failed".to_string()),
            }),
        ] {
            let json = serde_json::to_string(&case).expect("encode");
            let back: WorkerToService = serde_json::from_str(&json).expect("decode");
            match (case, back) {
                (
                    WorkerToService::VirtualDisplayAttachResult(a),
                    WorkerToService::VirtualDisplayAttachResult(b),
                ) => {
                    assert_eq!(a.instance_id, b.instance_id);
                    assert_eq!(a.outcome, b.outcome);
                }
                (a, b) => panic!("round-trip variant drift: {a:?} -> {b:?}"),
            }
        }
    }

    #[test]
    fn detach_virtual_display_round_trips_wincode() {
        let msg = ServiceToWorker::DetachVirtualDisplay;
        let back = wincode_round_trip(&msg);
        assert!(matches!(back, ServiceToWorker::DetachVirtualDisplay));
    }

    #[test]
    fn detach_virtual_display_round_trips_serde_json() {
        let msg = ServiceToWorker::DetachVirtualDisplay;
        let json = serde_json::to_string(&msg).expect("encode");
        let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
        assert!(matches!(back, ServiceToWorker::DetachVirtualDisplay));
    }

    /// New in v4: daemon → worker `RefreshCapabilities` is a unit
    /// variant. Both encodings must round-trip cleanly so future
    /// daemon / worker version drift cannot silently corrupt the
    /// virtual-display capabilities refresh path.
    #[test]
    fn refresh_capabilities_round_trips_wincode() {
        let msg = ServiceToWorker::RefreshCapabilities;
        let back = wincode_round_trip(&msg);
        assert!(matches!(back, ServiceToWorker::RefreshCapabilities));
    }

    #[test]
    fn refresh_capabilities_round_trips_serde_json() {
        let msg = ServiceToWorker::RefreshCapabilities;
        let json = serde_json::to_string(&msg).expect("encode");
        let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
        assert!(matches!(back, ServiceToWorker::RefreshCapabilities));
    }

    #[test]
    fn virtual_display_mode_response_applied_round_trips_wincode() {
        let msg = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
            request_id: "req-9".to_string(),
            connection_id: "conn-9".to_string(),
            outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
            }),
        });
        match wincode_round_trip(&msg) {
            WorkerToService::VirtualDisplayMode(p) => {
                assert_eq!(p.request_id, "req-9");
                assert_eq!(p.connection_id, "conn-9");
                match p.outcome {
                    VirtualDisplayModeOutcome::Applied(m) => {
                        assert_eq!(m.width, 1920);
                        assert_eq!(m.height, 1080);
                        assert_eq!(m.refresh_hz, 60);
                    }
                    other => panic!("unexpected outcome: {other:?}"),
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn virtual_display_mode_response_failed_round_trips_wincode() {
        let msg = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
            request_id: "req-10".to_string(),
            connection_id: "conn-10".to_string(),
            outcome: VirtualDisplayModeOutcome::Failed("driver pipe IO failed".to_string()),
        });
        match wincode_round_trip(&msg) {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Failed(reason) => {
                    assert_eq!(reason, "driver pipe IO failed");
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `desired = true` round-trips with a non-trivial `op_id` and
    /// `prompt_duration_ms`. Pins the wire shape (struct layout +
    /// field order) so a future schema-write edit immediately surfaces
    /// in CI.
    #[test]
    fn set_virtual_display_exclusive_enter_round_trips_wincode() {
        let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
            op_id: 42,
            desired: true,
            prompt_duration_ms: 5_000,
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::SetVirtualDisplayExclusive(p) => {
                assert_eq!(p.op_id, 42);
                assert!(p.desired);
                assert_eq!(p.prompt_duration_ms, 5_000);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `desired = false` round-trips. `prompt_duration_ms` is preserved
    /// even though the worker ignores it for leave requests — the
    /// wire format does not get to skip the field.
    #[test]
    fn set_virtual_display_exclusive_leave_round_trips_wincode() {
        let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
            op_id: u64::MAX - 1,
            desired: false,
            prompt_duration_ms: 0,
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::SetVirtualDisplayExclusive(p) => {
                assert_eq!(p.op_id, u64::MAX - 1);
                assert!(!p.desired);
                assert_eq!(p.prompt_duration_ms, 0);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JSON encoding is the secondary wire (used by some test helpers
    /// and the manager's REST surface for debugging). The op_id must
    /// round-trip through JSON too — `serde_json` defaults to a u64
    /// number which is fine up to 2^53 in JSON parsers; tests pin the
    /// representation so a switch to a string encoding (e.g. to avoid
    /// the JS precision cliff) shows up as a failing test.
    #[test]
    fn set_virtual_display_exclusive_round_trips_serde_json() {
        let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
            op_id: 7,
            desired: true,
            prompt_duration_ms: 5_000,
        });
        let json = serde_json::to_string(&msg).expect("encode");
        let back: ServiceToWorker = serde_json::from_str(&json).expect("decode");
        match back {
            ServiceToWorker::SetVirtualDisplayExclusive(p) => {
                assert_eq!(p.op_id, 7);
                assert!(p.desired);
                assert_eq!(p.prompt_duration_ms, 5_000);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Every `ExclusiveOutcome` variant must round-trip. The pipeline
    /// emits exactly these four shapes; a regression that adds or
    /// removes one is a wire break. EnterCancelled is intentionally
    /// absent (removed in design round 6).
    #[test]
    fn exclusive_result_all_four_outcomes_round_trip_wincode() {
        let cases = [
            (
                100u64,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::Entered,
            ),
            (
                101u64,
                ExclusiveDirection::Entering,
                ExclusiveOutcome::EnterFailed("snapshot failed".to_string()),
            ),
            (102u64, ExclusiveDirection::Leaving, ExclusiveOutcome::Left),
            (
                103u64,
                ExclusiveDirection::Leaving,
                ExclusiveOutcome::LeftWithErrors("partial: \\\\.\\DISPLAY2".to_string()),
            ),
        ];
        for (op_id, direction, outcome) in cases {
            let msg = WorkerToService::ExclusiveResult(ExclusiveResultPayload {
                op_id,
                direction,
                outcome: outcome.clone(),
            });
            match wincode_round_trip(&msg) {
                WorkerToService::ExclusiveResult(p) => {
                    assert_eq!(p.op_id, op_id);
                    assert_eq!(p.direction, direction);
                    assert_eq!(p.outcome, outcome);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    /// `op_id` is a u64 — must serialise as 8 bytes little-endian
    /// when used through the wincode `Configuration<true, _>` setup
    /// the IPC pipeline uses (FixInt + LittleEndian). The first 8
    /// bytes after the enum tag + struct framing belong to op_id.
    ///
    /// We don't pin the absolute offset because the enum tag width
    /// is wincode-controlled — but encoding a known op_id value of
    /// `0x_01_02_03_04_05_06_07_08` (i.e. each byte distinct) and
    /// scanning the produced bytes lets us assert the byte sequence
    /// `08 07 06 05 04 03 02 01` appears as a contiguous run — the
    /// LE bit-pattern. A flip to BE or Varint would not produce that
    /// run, so a wire regression fails this test immediately.
    #[test]
    fn op_id_is_serialized_le_8_bytes() {
        let msg = ServiceToWorker::SetVirtualDisplayExclusive(SetVirtualDisplayExclusivePayload {
            op_id: 0x_01_02_03_04_05_06_07_08,
            desired: true,
            prompt_duration_ms: 0,
        });
        let config: WincodeUnbounded = Configuration::new();
        let bytes = wincode::config::serialize(&msg, config).expect("encode");
        let needle = [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
        let found = bytes.windows(needle.len()).any(|w| w == needle);
        assert!(
            found,
            "expected LE u64 bit pattern in encoded bytes; got {bytes:?}"
        );
    }

    /// Build a representative server-stamped `AgentEnvelope` for the
    /// AI-plane IPC round-trip tests.
    fn sample_agent_envelope() -> desk_agent_protocol::AgentEnvelope {
        use desk_agent_protocol::*;
        AgentEnvelope {
            protocol_version: ProtocolVersion::default(),
            request_id: RequestId("req-ai-1".to_string()),
            parent_task_id: Some(TaskId("task-ai-1".to_string())),
            target: TargetRef {
                device_id: "dev-1".to_string(),
                session_id: Some("sess-1".to_string()),
                worker_id: None,
            },
            actor: ActorRef {
                actor_type: ActorType::User,
                actor_id: "user-1".to_string(),
            },
            caller: CallerRef {
                caller_type: CallerType::Human,
                model_provider: None,
                model_name: None,
                adapter: None,
            },
            scope: AgentScope {
                granted: vec![Capability::ProcessList],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            operation: AgentOperation {
                risk_hint: None,
                input: OperationInput::ReadContext(ReadContextInput {
                    kind: ContextKind::ProcessList(ProcessListParams::default()),
                }),
            },
            audit: AuditMeta {
                approval_id: None,
                reason: Some("diagnose".to_string()),
            },
        }
    }

    fn sample_exec_plan() -> desk_agent_protocol::exec::ExecPlan {
        use desk_agent_protocol::RiskLevel;
        use desk_agent_protocol::exec::{ApprovalId, ExecPlan, ExecRequestId, ExecShellKind};
        ExecPlan {
            execution_generation: "gen-1".into(),
            exec_request_id: ExecRequestId("exec-1".to_string()),
            program: "docker".to_string(),
            argv: vec!["restart".to_string(), "web1".to_string()],
            cwd: None,
            shell: ExecShellKind::Native,
            risk: RiskLevel::High,
            template_id: "docker_restart".to_string(),
            approval_id: ApprovalId("appr-1".to_string()),
            fingerprint: "fp".to_string(),
            timeout_ms: 30_000,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        }
    }

    /// `ServiceToWorker::ExecPlan` carries the full sealed plan across the
    /// daemon → worker wire, and `WorkerToService::ExecResult` carries the
    /// `exec_request_id`-tagged result back.
    #[test]
    fn exec_plan_and_result_round_trip_wincode() {
        let plan_msg = ServiceToWorker::ExecPlan(ExecPlanPayload {
            request_id: "r-exec".to_string(),
            connection_id: Some("conn-1".to_string()),
            plan: sample_exec_plan(),
            audit_source_request_id: Some("frame-req-9".to_string()),
        });
        match wincode_round_trip(&plan_msg) {
            ServiceToWorker::ExecPlan(p) => {
                assert_eq!(p.request_id, "r-exec");
                assert_eq!(p.plan, sample_exec_plan());
                assert_eq!(p.audit_source_request_id.as_deref(), Some("frame-req-9"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        let result_msg = WorkerToService::ExecResult(ExecResultIpcPayload {
            request_id: "r-exec".to_string(),
            connection_id: Some("conn-1".to_string()),
            result: desk_agent_protocol::exec::ExecResultPayload {
                exec_request_id: desk_agent_protocol::exec::ExecRequestId("exec-1".to_string()),
                outcome: desk_agent_protocol::AgentOutcome::Ok(
                    desk_agent_protocol::OperationOutput::Exec(desk_agent_protocol::ExecOutput {
                        exit_code: 0,
                        stdout: "ok".to_string(),
                        stderr: String::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                        duration_ms: 5,
                        redactions: vec![],
                    }),
                ),
            },
            audit_source_request_id: Some("frame-req-9".to_string()),
        });
        match wincode_round_trip(&result_msg) {
            WorkerToService::ExecResult(p) => {
                assert_eq!(p.request_id, "r-exec");
                assert_eq!(p.result.exec_request_id.0, "exec-1");
                assert_eq!(p.audit_source_request_id.as_deref(), Some("frame-req-9"));
                assert!(matches!(
                    p.result.outcome,
                    desk_agent_protocol::AgentOutcome::Ok(_)
                ));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `ServiceToWorker::AgentRequest` carries the full embedded
    /// `AgentEnvelope` across the daemon → worker wire byte-for-byte.
    #[test]
    fn agent_request_round_trips_wincode() {
        let msg = ServiceToWorker::AgentRequest(AgentRequestPayload {
            request_id: "req-ai-1".to_string(),
            connection_id: Some("conn-1".to_string()),
            envelope: sample_agent_envelope(),
        });
        match wincode_round_trip(&msg) {
            ServiceToWorker::AgentRequest(p) => {
                assert_eq!(p.request_id, "req-ai-1");
                assert_eq!(p.connection_id.as_deref(), Some("conn-1"));
                assert_eq!(p.envelope, sample_agent_envelope());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// `WorkerToService::AgentResponse` reuses `AgentOutcome` verbatim;
    /// both the `Ok` (output) and `Err` (capability-level error) arms
    /// survive the worker → daemon wire.
    #[test]
    fn agent_response_round_trips_wincode_both_arms() {
        use desk_agent_protocol::*;
        let ok = WorkerToService::AgentResponse(AgentResponsePayload {
            request_id: "req-ai-1".to_string(),
            connection_id: Some("conn-1".to_string()),
            outcome: AgentOutcome::Ok(OperationOutput::ReadContext(
                ReadContextOutput::ProcessList(ProcessListOutput {
                    processes: vec![],
                    truncated: false,
                }),
            )),
        });
        match wincode_round_trip(&ok) {
            WorkerToService::AgentResponse(p) => {
                assert_eq!(p.request_id, "req-ai-1");
                assert!(matches!(p.outcome, AgentOutcome::Ok(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }

        let err = WorkerToService::AgentResponse(AgentResponsePayload {
            request_id: "req-ai-2".to_string(),
            connection_id: None,
            outcome: AgentOutcome::Err(AgentError {
                kind: AgentErrorKind::UnsupportedCapability,
                message: "not implemented yet".to_string(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            }),
        });
        match wincode_round_trip(&err) {
            WorkerToService::AgentResponse(p) => {
                assert_eq!(p.connection_id, None);
                match p.outcome {
                    AgentOutcome::Err(e) => {
                        assert_eq!(e.kind, AgentErrorKind::UnsupportedCapability)
                    }
                    other => panic!("unexpected outcome: {other:?}"),
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
