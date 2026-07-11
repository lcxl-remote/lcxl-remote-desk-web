use crate::worker::agent::LocalDeviceAgent;
use crate::{
    host_control::{HostControlHub, UpstreamForwarder, upstream::spawn_upstream_ws_task},
    model::settings::{Args, Settings, SharedSettings, StartupMode},
    service::signaling::{DeskSession, DeskSessionMessage, DeskSessionSender},
    worker::{
        clipboard_dispatcher::ClipboardDispatcher,
        desktop_monitor,
        file_transfer_dispatcher::FileTransferDispatcher,
        input_dispatcher::InputDispatcher,
        media_producer::MediaProducer,
        shared_capture::CaptureKey,
        virtual_display::{
            RestartStep, VirtualDisplayState, resolve_attach_with_backoff, run_set_mode,
        },
        whiteboard_dispatcher::WhiteboardDispatcher,
    },
};
use actix_web::web;
use desk_agent_protocol::{AgentOutcome, DeviceAgent};
use desk_input_injection::display_watcher;
use desk_ipc_protocol::{
    dual_transport::{EventReceiver, EventSender, MediaSender, framed},
    message::{
        AgentResponsePayload, DesktopChangedPayload, ExecResultIpcPayload, FileTransferPayload,
        HeartbeatPayload, ListTerminalResponsePayload, ManagerFileListResponsePayload,
        ManagerQuerySettingsResponsePayload, ManagerResponseRefPayload,
        ManagerSystemInfoResponsePayload, PrivateScreenStateChangedPayload,
        ReplyFromTerminalPayload, ServiceToWorker, SignalingErrorPayload, StopMediaPayload,
        TerminalClosedPayload, TerminalStartedPayload, VirtualDisplayAttachOutcome,
        VirtualDisplayAttachResultPayload, WorkerInitPayload, WorkerToService,
    },
    transport::{read_message, write_message},
};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::files::FileListResponse;
use desk_signal_facade::model::private_screen::{
    EnablePrivateScreenData, PrivateScreenStateChangedData,
};
use desk_signal_facade::model::signal::{SignalingModel, SignalingType};
use desk_signal_facade::model::system_info::SystemInfo;
use desk_signal_facade::model::system_settings::RemoteSystemSettings;
use desk_signal_facade::model::terminal::{TerminalList, TerminalOutputData};
use desk_virtual_display::VirtualDisplayController;
use log::{error, info, warn};
use std::collections::HashSet;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
};

/// Decide which `HostControlHub` flavour to construct from an Init payload.
/// Returns the hub and, when running in Forwarder mode, the spec needed for the
/// caller to spawn the ws-client task. Split out from `ipc_loop` so the
/// decision can be unit-tested without an actix runtime.
fn build_hub_from_init(
    payload: &WorkerInitPayload,
) -> (
    Arc<HostControlHub>,
    Option<(Arc<UpstreamForwarder>, String, String)>,
) {
    match payload.host_upstream_url.clone() {
        Some(url) => {
            let upstream = UpstreamForwarder::new();
            let token = payload.auth_token.clone().unwrap_or_default();
            let hub = Arc::new(HostControlHub::new_forwarder(Arc::clone(&upstream)));
            (hub, Some((upstream, url, token)))
        }
        None => (Arc::new(HostControlHub::new_local()), None),
    }
}

/// Typed-IPC migration helper: convert a typed `ServiceToWorker`
/// payload back into a `SignalingModel` so the existing
/// `DeskSession::handle_message` dispatch can run unchanged. Used by
/// batch 1 (and subsequent batches) until each `handle_message` arm
/// is fully migrated and the legacy dispatcher can be retired.
///
/// Build / serialise failures are non-fatal: they log + drop, same
/// behaviour the previous `SignalingMessage` JSON-bridge path had on
/// malformed input.
async fn dispatch_typed_signaling<T>(
    desk_session: &mut DeskSession,
    signaling_type: SignalingType,
    connection_id: Option<String>,
    data: &T,
) where
    T: serde::Serialize + ?Sized,
{
    dispatch_typed_signaling_with_request_id(
        desk_session,
        signaling_type,
        // One-way notifications carry no request_id; a placeholder
        // keeps logs scannable.
        "typed-ipc".to_string(),
        connection_id,
        Some(data),
    )
    .await
}

/// Same as [`dispatch_typed_signaling`] but lets the caller pass an
/// explicit `request_id` (manager-plane requests need to echo it
/// back so the worker's `send_response` writes the same id, which
/// the desk_rx outbound classifier then re-uses on the typed
/// `Manager*Response`).
///
/// Also accepts an `Option<&T>` body so empty-body requests
/// (`ManagerSystemInfoRequest` / `ManagerQuerySettingsRequest`) can
/// share this helper without serialising a synthetic placeholder.
///
/// `connection_id` is `Option<String>` because manager-plane and
/// `ListTerminal` requests can be dispatched from a HTTP REST
/// controller (no originating browser PC). The synthetic
/// `SignalingModel` we hand to `DeskSession::handle_message` simply
/// carries a `None` `from_connection_id` in that case; the
/// downstream worker handlers (`handle_manager_file_list`,
/// `handle_list_terminals`, ...) already tolerate `None`, and
/// `send_response` echoes it back into the response model's
/// `to_connection_id`.
async fn dispatch_typed_signaling_with_request_id<T>(
    desk_session: &mut DeskSession,
    signaling_type: SignalingType,
    request_id: String,
    connection_id: Option<String>,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    let signaling_data = match data {
        Some(d) => match serde_json::to_value(d) {
            Ok(v) => Some(v),
            Err(e) => {
                warn!(
                    "Failed to serialise {signaling_type:?} payload for {connection_id:?}: \
                     {e}; dropping",
                );
                return;
            }
        },
        None => None,
    };
    let model = SignalingModel::new(
        &request_id,
        signaling_type,
        connection_id.clone(),
        None,
        signaling_data,
        None,
    );
    if let Err(e) = desk_session.handle_message(&model).await {
        warn!(
            "DeskSession handle_message error for typed {signaling_type:?}: {e}, \
             connection_id={connection_id:?}, request_id={request_id}",
        );
    }
}

/// Classify an outbound signaling text blob produced by `DeskSession`
/// into a typed `WorkerToService` variant. Handles three groups:
///
/// 1. **Error responses** (`response_state.error_code != 0`): packed
///    into [`WorkerToService::SignalingError`] regardless of the
///    originating `SignalingType`. Catches every
///    `service::signaling::DeskSession::send_error` call (terminal
///    permission denied, manager file errors, the fallthrough
///    `_ => UNKNOWN_SIGNALING_TYPE`, ...).
/// 2. **Typed success responses / notifications** for migrated
///    SignalingTypes (PrivateScreenStateChanged, Manager*, Terminal*):
///    routed via [`try_route_typed_outbound`].
/// 3. **Anything else**: log + drop. Returns `None`. After batch 4
///    of the typed-IPC migration there is no `SignalingMessage`
///    fallback bridge — every outbound type the daemon needs to
///    surface to the browser is explicitly typed. A `None` result
///    indicates either a parse failure (malformed JSON the worker
///    never produced under normal operation) or a `SignalingType` no
///    longer expected on the worker → daemon path.
fn build_outbound_payload_from_desk_text(text: String) -> Option<WorkerToService> {
    let model = match serde_json::from_str::<SignalingModel>(&text) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "Worker emitted unparseable signaling JSON; dropping (len={}, err={e})",
                text.len()
            );
            return None;
        }
    };

    // Error responses: route ALL of them through the typed
    // SignalingError catch-all — the original SignalingType is
    // preserved in the payload so the daemon can rebuild the
    // outbound `SignalingModel::error(...)` and the browser keys
    // off `signaling_type` to match the response to its pending
    // request.
    if let Some(state) = model.response_state.as_ref()
        && !state.is_success()
    {
        let connection_id = model.to_connection_id.clone().unwrap_or_default();
        return Some(WorkerToService::SignalingError(SignalingErrorPayload {
            request_id: model.request_id.clone(),
            connection_id,
            signaling_type: model.signaling_type,
            error_code: state.error_code,
            error_message: state.message.clone(),
        }));
    }

    if let Some(typed) = try_route_typed_outbound(&model) {
        return Some(typed);
    }

    warn!(
        "Worker emitted signaling reply with no typed IPC route: type={:?}, request_id={}; \
         dropping (the SignalingMessage bridge no longer exists — every outbound type must \
         have a typed variant)",
        model.signaling_type, model.request_id,
    );
    None
}

/// Return `Some(typed)` when the outbound `SignalingModel` matches a
/// success-response or notification type that has been promoted to a
/// typed `WorkerToService` variant; `None` for unmatched types (the
/// caller logs + drops). Error responses are handled separately by
/// the SignalingError branch in `build_outbound_payload_from_desk_text`.
/// Splits per-type matching out so each batch can append arms without
/// the surrounding function getting unwieldy.
fn try_route_typed_outbound(model: &SignalingModel) -> Option<WorkerToService> {
    match model.signaling_type {
        SignalingType::PrivateScreenStateChanged => {
            let connection_id = model.to_connection_id.clone()?;
            let data = model
                .get_data_with_type::<PrivateScreenStateChangedData>()
                .ok()
                .flatten()?;
            Some(WorkerToService::PrivateScreenStateChanged(
                PrivateScreenStateChangedPayload {
                    connection_id,
                    data,
                },
            ))
        }
        // Batch 2: manager-plane responses. `send_response` echoes
        // `from_connection_id` of the inbound request as
        // `to_connection_id` of the outbound response; HTTP-API-
        // triggered manager requests carry `None`, so the typed
        // payload also carries `Option<String>` and the daemon
        // correlates the response by `request_id` alone.
        SignalingType::ManagerSystemInfo => {
            let info = model.get_data_with_type::<SystemInfo>().ok().flatten()?;
            Some(WorkerToService::ManagerSystemInfoResponse(
                ManagerSystemInfoResponsePayload {
                    request_id: model.request_id.clone(),
                    connection_id: model.to_connection_id.clone(),
                    info,
                },
            ))
        }
        SignalingType::ManagerQuerySettings => {
            let settings = model
                .get_data_with_type::<RemoteSystemSettings>()
                .ok()
                .flatten()?;
            Some(WorkerToService::ManagerQuerySettingsResponse(
                ManagerQuerySettingsResponsePayload {
                    request_id: model.request_id.clone(),
                    connection_id: model.to_connection_id.clone(),
                    settings,
                },
            ))
        }
        SignalingType::ManagerFileList => {
            let response = model
                .get_data_with_type::<FileListResponse>()
                .ok()
                .flatten()?;
            Some(WorkerToService::ManagerFileListResponse(
                ManagerFileListResponsePayload {
                    request_id: model.request_id.clone(),
                    connection_id: model.to_connection_id.clone(),
                    response,
                },
            ))
        }
        // ManagerFileDelete / ManagerUpdateSettings responses carry
        // an empty body (`&()`), so a successful round-trip omits
        // signaling_data; `request_id` alone is enough to correlate
        // back to the originating HTTP REST controller (or browser
        // PC, when `to_connection_id` is `Some`).
        SignalingType::ManagerFileDelete => Some(WorkerToService::ManagerFileDeleteResponse(
            ManagerResponseRefPayload {
                request_id: model.request_id.clone(),
                connection_id: model.to_connection_id.clone(),
            },
        )),
        SignalingType::ManagerUpdateSettings => Some(
            WorkerToService::ManagerUpdateSettingsResponse(ManagerResponseRefPayload {
                request_id: model.request_id.clone(),
                connection_id: model.to_connection_id.clone(),
            }),
        ),
        // Batch 3: terminal-plane responses + notifications. The
        // worker's terminal handlers always set the target browser in
        // `to_connection_id` (either via `success_response` for
        // `TerminalStarted` / `ListTerminal`, or via `new_request` for
        // server-initiated `ReplyFromTerminal` / `TerminalClosed`).
        SignalingType::TerminalStarted => {
            let connection_id = model.to_connection_id.clone()?;
            Some(WorkerToService::TerminalStarted(TerminalStartedPayload {
                request_id: model.request_id.clone(),
                connection_id,
            }))
        }
        SignalingType::TerminalClosed => {
            let connection_id = model.to_connection_id.clone()?;
            Some(WorkerToService::TerminalClosed(TerminalClosedPayload {
                connection_id,
            }))
        }
        SignalingType::ReplyFromTerminal => {
            let connection_id = model.to_connection_id.clone()?;
            let data = model
                .get_data_with_type::<TerminalOutputData>()
                .ok()
                .flatten()?;
            Some(WorkerToService::ReplyFromTerminal(
                ReplyFromTerminalPayload {
                    connection_id,
                    data,
                },
            ))
        }
        SignalingType::ListTerminal => {
            let terminals = model.get_data_with_type::<TerminalList>().ok().flatten()?;
            Some(WorkerToService::ListTerminalResponse(
                ListTerminalResponsePayload {
                    request_id: model.request_id.clone(),
                    connection_id: model.to_connection_id.clone(),
                    terminals,
                },
            ))
        }
        _ => None,
    }
}

/// Returns `true` iff the worker should refresh the input dispatcher's
/// per-connection geometry after sending this `WorkerToService` response
/// to the daemon. Specifically: when the response is a
/// `VirtualDisplayMode` reply carrying `VirtualDisplayModeOutcome::Applied`
/// — i.e. the IDD driver actually committed a new mode. `Failed` /
/// non-VirtualDisplayMode variants return `false`.
///
/// **Scope note**: this gate now only governs the input geometry
/// refresh, NOT the WGC capture restart. The IDD driver's
/// `Departure`+`Arrival` cycle happens at the pipe layer *before* the
/// CDS commit runs, so a `DISP_CHANGE_BADMODE` Failed outcome still
/// invalidates WGC's HMONITOR. The restart path is therefore
/// decoupled from this predicate in the IPC handler.
///
/// Pulled out of the main IPC loop so it can be unit-tested without
/// running the full session.
fn should_refresh_after_set_mode(response: &WorkerToService) -> bool {
    matches!(
        response,
        WorkerToService::VirtualDisplayMode(p)
            if matches!(
                p.outcome,
                desk_ipc_protocol::message::VirtualDisplayModeOutcome::Applied(_)
            )
    )
}

/// Filter the candidate restart steps down to the connections that
/// actually need a forced capture-pipeline rebuild after a
/// `SetVirtualDisplayMode` Applied: only those whose effective
/// `CaptureKey` is WGC backend targeting the currently attached IDD
/// display. DXGI (returns `DXGI_ERROR_ACCESS_LOST` on
/// `AcquireNextFrame`) and GDI (re-`EnumDisplaySettingsW` every frame)
/// self-adapt to a mid-session monitor remount — forcing a Stop+Start
/// on them would only add a needless IDR flicker.
///
/// `key_lookup` is parameterised so the test suite can drive arbitrary
/// connection → key fixtures without spinning up a real producer.
fn select_wgc_restart_steps<F>(
    steps: Vec<RestartStep>,
    attached_display: Option<&str>,
    key_lookup: F,
) -> Vec<RestartStep>
where
    F: Fn(&str) -> Option<CaptureKey>,
{
    let Some(attached) = attached_display else {
        return Vec::new();
    };
    steps
        .into_iter()
        .filter(|s| {
            key_lookup(&s.connection_id)
                .is_some_and(|k| k.backend.eq_ignore_ascii_case("WGC") && k.device_name == attached)
        })
        .collect()
}

/// Distinct `CaptureKey`s covered by the given restart steps, in
/// first-seen order. Ensures each shared-capture registry slot is
/// invalidated exactly once even when several connections share the
/// same `(backend, device_name)` slot (the multi-browser case).
fn dedup_capture_keys<F>(steps: &[RestartStep], key_lookup: F) -> Vec<CaptureKey>
where
    F: Fn(&str) -> Option<CaptureKey>,
{
    let mut seen: HashSet<CaptureKey> = HashSet::new();
    let mut out: Vec<CaptureKey> = Vec::new();
    for step in steps {
        if let Some(k) = key_lookup(&step.connection_id)
            && seen.insert(k.clone())
        {
            out.push(k);
        }
    }
    out
}

/// React to a display-change event by refreshing per-connection cursor
/// geometry. On Wayland this resolves each connection's captured surface
/// against the live `wl_output` geometry (no portal picker); on every
/// other platform it re-queries the attached-display list.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn refresh_geometry_after_display_change(
    input_dispatcher: &InputDispatcher,
    media_producer: Option<&MediaProducer>,
) {
    #[cfg(target_os = "linux")]
    {
        use desk_input_injection::linux_display::{Backend, detect_backend};
        if detect_backend() == Backend::Wayland {
            refresh_wayland_geometry(input_dispatcher, media_producer);
            return;
        }
    }
    input_dispatcher.refresh_geometry(None);
}

/// Wayland display-change refresh. Enumerates the current `wl_output`
/// logical geometry without the portal, then re-anchors each connection
/// on the captured surface's recorded position. Anything unresolved
/// (no producer, empty/failed enumeration, no anchor match) leaves the
/// connection's geometry untouched — it never re-points to a different
/// monitor.
#[cfg(target_os = "linux")]
fn refresh_wayland_geometry(
    input_dispatcher: &InputDispatcher,
    media_producer: Option<&MediaProducer>,
) {
    use desk_capture_engine::image_capture::wayland_output_geometry::{
        enumerate_wayland_outputs, match_output_by_anchor,
    };
    let Some(producer) = media_producer else {
        return;
    };
    let outputs = match enumerate_wayland_outputs() {
        Ok(o) if !o.is_empty() => o,
        Ok(_) => {
            log::debug!(
                "display-change: no Wayland output geometry available; keeping current geometry"
            );
            return;
        }
        Err(e) => {
            log::debug!(
                "display-change: Wayland output enumeration failed: {e}; keeping current geometry"
            );
            return;
        }
    };
    for id in input_dispatcher.connection_ids() {
        let Some(info) = producer.connection_display_info(&id) else {
            continue;
        };
        let anchor = info.desktop_coordinates;
        match match_output_by_anchor(&outputs, anchor) {
            Some(geo) => {
                let r = geo.logical;
                input_dispatcher
                    .set_connection_geometry(&id, (r.left, r.top, r.width(), r.height()));
            }
            None => log::debug!(
                "display-change: no Wayland output matches connection {id} anchor {anchor:?}; \
                 keeping current geometry"
            ),
        }
    }
}

/// Worker-side session. Stateless wrapper — all mutable state lives in the
/// dispatchers / `DeskSession` instances built per-session inside
/// [`Self::run_with_transports`]. The struct exists so the named-pipe
/// entry point ([`Self::run`]) and the in-process portable entry
/// ([`Self::run_with_transports`]) share an inherent-method namespace.
pub struct WorkerSession;

impl Default for WorkerSession {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerSession {
    pub fn new() -> Self {
        WorkerSession
    }

    pub async fn run(args: Args, pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let _ = args; // Reserved for future per-mode toggles; not used today.
        let session = WorkerSession;
        session.connect_and_serve(pipe_name).await
    }

    async fn connect_and_serve(&self, pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        info!("WorkerSession connecting to IPC pipe: {}", pipe_name);

        #[cfg(target_os = "windows")]
        let (reader, writer) = self.connect_windows_pipe(pipe_name).await?;

        #[cfg(not(target_os = "windows"))]
        let (reader, writer) = self.connect_unix_socket(pipe_name).await?;

        self.ipc_loop(reader, writer).await
    }

    /// Named-pipe / Unix-socket entry. Performs the Ready / Init handshake
    /// directly on the byte stream (length-prefixed wincode payload — see
    /// [`desk_ipc_protocol::transport::IPC_CONFIG`]), then wraps the remaining
    /// stream in `framed` event transports and connects the optional media
    /// pipe before delegating to [`Self::run_with_transports`]. The
    /// transport-agnostic main loop is shared with the in-process portable
    /// path — only the way transports are constructed differs.
    async fn ipc_loop<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        write_message(&mut writer, &WorkerToService::Ready).await?;
        info!("Sent Ready message to Service");

        let init_payload = loop {
            let msg: ServiceToWorker = read_message(&mut reader).await?;
            match msg {
                ServiceToWorker::Init(payload) => {
                    info!(
                        "Received Init: session_id={}, os_session_id={}, desktop={:?}",
                        payload.session_id, payload.os_session_id, payload.desktop_name
                    );
                    break payload;
                }
                ServiceToWorker::Shutdown => {
                    info!("Received Shutdown before Init, exiting");
                    return Ok(());
                }
                other => {
                    warn!("Received {:?} before Init, ignoring", other);
                }
            }
        };

        // Wrap the post-handshake bytes in framed event transports. The
        // wire format (`LengthDelimitedCodec` + wincode payload, see
        // `desk_ipc_protocol::transport::IPC_CONFIG`) is binary compatible
        // with the `read_message` / `write_message` calls above — both speak
        // length-prefixed wincode with the same 16 MB cap.
        let event_tx: Arc<dyn EventSender<WorkerToService>> = framed::spawn_event_sender(writer);
        let event_rx: Box<dyn EventReceiver<ServiceToWorker>> = framed::make_event_receiver(reader);

        // Optional media pipe. Connect failure is non-fatal —
        // the worker continues to serve event-pipe traffic (mouse / clipboard
        // / file transfer / ...) and reports `Capabilities` so the daemon can
        // populate `RequestRemote` Init replies even if no frames flow.
        let media_sender = match init_payload.media_pipe_name.as_deref() {
            Some(name) => {
                info!("Worker connecting to media pipe: {name}");
                match connect_media_pipe(name).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        warn!(
                            "Worker failed to connect to media pipe {name}: {e}; \
                             continuing without media transport"
                        );
                        None
                    }
                }
            }
            None => None,
        };

        // File lane: dedicated bidirectional pipe for download
        // chunks / control replies / upload chunks / cancels — split
        // off from the event lane so SCTP backpressure on a slow
        // browser DataChannel does not head-of-line block heartbeats /
        // manager responses. The daemon always provisions this pipe
        // for named-pipe workers, so a missing `file_pipe_name` is a
        // fatal init error: the worker surfaces an `Error` and exits
        // (no fallback, since that would silently put file bytes back
        // on the event lane).
        let file_pipe_name = match init_payload.file_pipe_name.as_deref() {
            Some(name) => name,
            None => {
                let msg = "WorkerInit lacked file_pipe_name in named-pipe mode; \
                           daemon must provision a dedicated file lane";
                error!("{msg}");
                let err = WorkerToService::Error(desk_ipc_protocol::message::ErrorPayload {
                    code: -1,
                    message: msg.to_string(),
                    recoverable: false,
                    connection_id: None,
                });
                let _ = event_tx.send(err).await;
                return Err(msg.into());
            }
        };
        info!("Worker connecting to file pipe: {file_pipe_name}");
        let (file_sender, file_receiver) = connect_file_pipe(file_pipe_name).await?;

        // Named-pipe path: no shared hub — worker constructs its own
        // (Forwarder if `host_upstream_url` is set, Local otherwise).
        self.run_with_transports(
            init_payload,
            event_rx,
            event_tx,
            media_sender,
            file_sender,
            file_receiver,
            None,
        )
        .await
    }

    /// Transport-agnostic main loop. Used by both:
    ///
    /// - the named-pipe / Unix-socket path (after Ready/Init handshake on the
    ///   raw byte stream); and
    /// - the in-process portable path where daemon and worker share
    ///   one process and transports are tokio mpsc channels — no byte
    ///   serialization, no handshake required because the caller just hands
    ///   the [`WorkerInitPayload`] directly.
    ///
    /// All worker-side dispatchers (input / clipboard / file-transfer /
    /// whiteboard / media producer / heartbeat) talk to the daemon through
    /// an internal `mpsc::UnboundedSender<WorkerToService>` — an event
    /// forwarder task drains that mpsc and pushes onto the supplied
    /// [`EventSender`]. This keeps the dispatchers transport-oblivious and
    /// preserves the property that one slow handler (e.g. an awaited
    /// approval prompt) cannot stall heartbeats / IDR write-throughs.
    ///
    /// `shared_hub` is the in-process bypass for the host-control hub. When
    /// `Some`, the supplied hub is used directly (portable mode where
    /// daemon and worker share the same `Arc<HostControlHub>`); when `None`
    /// the worker constructs its own hub from `init_payload.host_upstream_url`
    /// (named-pipe daemon mode — Forwarder bridges via ws back to the
    /// daemon's aggregator).
    pub async fn run_with_transports(
        &self,
        init_payload: WorkerInitPayload,
        mut event_rx: Box<dyn EventReceiver<ServiceToWorker>>,
        event_tx: Arc<dyn EventSender<WorkerToService>>,
        media_sender: Option<Arc<dyn MediaSender>>,
        file_sender: Arc<dyn EventSender<FileTransferPayload>>,
        mut file_receiver: Box<dyn EventReceiver<FileTransferPayload>>,
        shared_hub: Option<Arc<HostControlHub>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let settings = match serde_json::from_str::<Settings>(&init_payload.config_json) {
            Ok(mut s) => {
                // Args is #[serde(skip)] so it defaults to Args::default() after
                // deserialization. SessionWorker always acts as a pure desk server,
                // so set the mode explicitly to satisfy DeskServer-specific checks
                // (e.g. TURN ICE server inclusion in signaling.rs).
                s.args.startup_mode = StartupMode::DeskServer;
                // Restore `config_file_path` from the Init payload so any
                // worker-side `Settings::save()` (e.g. persisting a "remember"
                // choice from a security approval dialog) writes back to the
                // exact file the daemon loaded. Without this the worker's
                // `args.config_file_path` is the empty string and `save()`
                // fails with `FILE_PATH_NOT_FOUND`.
                if let Some(p) = init_payload.config_file_path.as_deref() {
                    s.args.config_file_path = p.to_owned();
                }
                s
            }
            Err(e) => {
                error!("Failed to parse config from Init payload: {}", e);
                let err_msg = WorkerToService::Error(desk_ipc_protocol::message::ErrorPayload {
                    code: -1,
                    message: format!("Failed to parse config: {}", e),
                    recoverable: false,
                    connection_id: None,
                });
                let _ = event_tx.send(err_msg).await;
                return Err(Box::new(e));
            }
        };

        let shared_settings = Arc::new(SharedSettings::from(settings));
        let shared_settings_data = web::Data::from(shared_settings.clone());

        // Telemetry init policy:
        //
        // - Named-pipe SessionWorker mode: the worker is a separate OS process
        //   spawned via `CreateProcessAsUserW`; it must install its own global
        //   tracing subscriber so log events / OTLP spans flow correctly.
        // - In-process portable / DeskServer mode: the host process
        //   (`crate::run`) already called `init_telemetry`, which sets the
        //   single per-process global default subscriber. A second
        //   `init_telemetry` here would panic with `SetGlobalDefaultError`.
        //
        // `shared_hub.is_some()` is the canonical in-process indicator (see
        // the host-control hub branch immediately below), so we reuse it.
        let _guard = if should_init_worker_telemetry(shared_hub.is_some()) {
            crate::telemetry::init_telemetry(shared_settings.clone(), &StartupMode::SessionWorker)
                .await?
        } else {
            info!(
                "In-process worker: skipping telemetry init (host process already installed global subscriber)"
            );
            None
        };

        let (desk_tx, mut desk_rx) = mpsc::unbounded_channel::<DeskSessionMessage>();
        let session_sender = DeskSessionSender {
            sender: desk_tx.clone(),
        };

        // Build the host-control hub. In named-pipe daemon mode the daemon
        // supplied a `host_upstream_url` so we run as a Forwarder and bridge
        // approval / private-screen / whiteboard traffic over ws back to the
        // daemon's aggregator. In portable mode the caller hands us the
        // daemon's hub directly via `shared_hub` — no ws, no extra task,
        // both ends share the same `Arc`. Standalone / test runs (no
        // upstream and no shared hub) fall back to a Local hub whose
        // approvals deny-fast.
        // Portable / in-process mode is identified by the caller passing a
        // pre-built `shared_hub` (mirrors `should_init_worker_telemetry`).
        // We latch the bool here because `shared_hub` is consumed by the
        // match arms below.
        let is_inprocess_worker = shared_hub.is_some();
        let host_control_hub = match shared_hub {
            Some(h) => {
                info!("Using shared host-control hub (in-process portable mode)");
                h
            }
            None => {
                let (hub, upstream_spec) = build_hub_from_init(&init_payload);
                match upstream_spec {
                    Some((upstream, url, token)) => {
                        spawn_upstream_ws_task(upstream, url, token);
                    }
                    None => {
                        warn!(
                            "Init payload missing host_upstream_url and no shared hub; \
                             falling back to Local hub (approvals will deny-fast)."
                        );
                    }
                }
                hub
            }
        };

        // Outbound IPC: dispatchers and the main loop send into an unbounded
        // mpsc; an event-forwarder task drains that mpsc and pushes onto the
        // supplied `EventSender`. Decoupling lets a long-running handler
        // (e.g. `request_approval` awaiting a Tauri dialog) coexist with the
        // heartbeat tick without starving the writer side. The forwarder is
        // joined at shutdown so the in-process transport's mpsc capacity is
        // fully drained before the test/runtime moves on.
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<WorkerToService>();
        let writer_task = spawn_event_forwarder_task(writer_rx, Arc::clone(&event_tx));

        // Build the media producer when the caller supplied a media
        // transport. In named-pipe mode this is the secondary pipe; in
        // in-process mode it's an mpsc-backed `MediaSender`. Either way the
        // producer's policy is identical (drop-on-backpressure for P-frames,
        // 500 ms timeout for I-frames).
        let media_producer: Option<Arc<MediaProducer>> = match media_sender {
            Some(sender) => {
                let desk_settings = shared_settings.read().await.desk.clone();
                Some(Arc::new(MediaProducer::new(
                    desk_settings,
                    sender,
                    writer_tx.clone(),
                )))
            }
            None => None,
        };
        let capabilities = MediaProducer::build_capabilities(
            init_payload.desktop_name.as_deref(),
            init_payload.host_upstream_url.is_some(),
        );
        // Per-connection input handlers. Constructed once per
        // worker; `start_connection` / `stop_connection` keyed off the
        // same `connection_id` the daemon ships in `StartMedia` /
        // `StopMedia`.
        let input_dispatcher = {
            let desk_settings = shared_settings.read().await.desk.clone();
            Arc::new(InputDispatcher::new(desk_settings))
        };
        // Clipboard dispatcher. Construction can fail when
        // the platform host-control helper cannot be initialised
        // (Linux without a clipboard backend, etc.); on failure the
        // worker continues without clipboard sync — the IPC variants
        // log + drop in the main loop instead of dispatching.
        let clipboard_dispatcher: Option<ClipboardDispatcher> = {
            let desk_settings = shared_settings.read().await.desk.clone();
            match ClipboardDispatcher::new(&desk_settings, writer_tx.clone()) {
                Ok(d) => Some(d),
                Err(e) => {
                    warn!("{e}");
                    None
                }
            }
        };
        // File transfer dispatcher. Always constructible —
        // it owns no resource that can fail at init time. Holds the
        // shared settings + host-control hub so it can run the per-
        // connection `allow_file_transfer` gate (which the daemon-side
        // DC router intentionally passes through; see the bug fix
        // notes in `handle_command`). The dispatcher emits download
        // chunks / control replies onto the dedicated file lane via
        // `file_sender`; daemon-bound traffic never goes through the
        // event lane (`writer_tx`) anymore — that path was retired
        // when fix-2026-05-05 demonstrated the head-of-line risk.
        let file_transfer_dispatcher = FileTransferDispatcher::new(
            file_sender,
            shared_settings.clone(),
            Arc::clone(&host_control_hub),
        );
        // Whiteboard dispatcher. Spawns a bridge thread to
        // the host_control_hub on construction; reuses the same hub
        // the DeskSession (legacy / portable path) uses so messages
        // flow through a single Tauri overlay manager.
        let whiteboard_dispatcher = WhiteboardDispatcher::new(Arc::clone(&host_control_hub));
        // Per-connection capability ceilings, registered by the daemon via
        // `SetConnectionCeiling` for redeemed-grant sessions and consumed by the
        // worker-side `meet(ceiling, global)` permission gates. Owner /
        // unrestricted connections are never registered (missing entry =
        // global-only gating).
        let connection_ceilings = crate::worker::connection_ceiling::ConnectionCeilingStore::new();
        if writer_tx
            .send(WorkerToService::Capabilities(capabilities))
            .is_err()
        {
            error!("IPC writer task died before Capabilities could be sent; exiting");
            return Ok(());
        }

        let mut desk_session = DeskSession::new(
            shared_settings_data,
            session_sender,
            CurrentUser::new_admin("worker_node"),
            host_control_hub,
            connection_ceilings.clone(),
        )
        .await
        .map_err(|e| format!("Failed to create DeskSession: {}", e))?;

        info!("DeskSession created successfully, entering main loop");

        // Virtual display: platform controller (Windows IDD impl on
        // Windows; NotSupported stub everywhere else) + per-worker
        // state (attached_display + dual StartMedia cache). Owned by
        // the main loop so all mutations are single-threaded.
        let virtual_display_controller: Arc<dyn VirtualDisplayController> =
            Arc::from(desk_virtual_display::controller_provider());
        let mut vd_state = VirtualDisplayState::new();
        // Coordinator + Drop guard for the exclusive-mode pipeline.
        // The guard owns an Arc clone of the layout slot; on session
        // exit (normal or panic) it drives leave_exclusive so the
        // physical displays come back. TerminateProcess skips Drop —
        // that is the path enter_exclusive's missing CDS_UPDATEREGISTRY
        // covers (the registry replays the physical layout on next
        // logon).
        let mut exclusive_coord = crate::worker::virtual_display::ExclusiveCoordinator::new();
        let _exclusive_guard = crate::worker::virtual_display::ExclusiveGuard::new(Arc::clone(
            &vd_state.exclusive_layout,
        ));
        // E2E fix 2026-05-27: WGC capture sessions bound via
        // `CreateForMonitor(HMONITOR)` survive the CDS commit at the
        // API level but stop emitting fresh frames after exclusive
        // enter/leave because the IDD's framebuffer mapping moves
        // underneath them. The reconciler posts an
        // `ExclusiveCommitEvent` on this channel after each
        // successful enter or leave so the main loop can run the
        // same `invalidate_capture_key + Stop/Start media` cycle
        // `SetVirtualDisplayMode` already uses for the same reason.
        let (exclusive_commit_tx, mut exclusive_commit_rx) =
            mpsc::unbounded_channel::<crate::worker::virtual_display::ExclusiveCommitEvent>();
        exclusive_coord.set_commit_channel(exclusive_commit_tx);

        // Reader task: drain the inbound `EventReceiver<ServiceToWorker>`
        // and forward into an unbounded mpsc the main loop selects on. A
        // `None` from `recv()` means the transport closed (peer disconnected
        // or in-process channel dropped); the main loop sees that as
        // `Some(None)` on the mpsc and breaks cleanly.
        let (service_msg_tx, mut service_msg_rx) =
            mpsc::unbounded_channel::<Option<ServiceToWorker>>();
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Some(msg) => {
                        if service_msg_tx.send(Some(msg)).is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = service_msg_tx.send(None);
                        break;
                    }
                }
            }
        });

        // File-lane drain task: hands inbound `FileTransferPayload`
        // frames straight to the dispatcher. Runs independent of the
        // event main loop so a long `serve_download` / `accept_upload`
        // never head-of-line blocks heartbeats or signaling — and so
        // `dispatcher.handle_command(...).await` (which is internally
        // serial because it `lock`s `inner`) reflects exactly the
        // browser DC arrival order without an extra hop. Exits on
        // `None` from `recv()` (lane closed → daemon vanished);
        // worker shutdown happens through the event lane so this
        // task is allowed to terminate quietly.
        {
            let dispatcher = file_transfer_dispatcher.clone();
            tokio::spawn(async move {
                while let Some(payload) = file_receiver.recv().await {
                    dispatcher.handle_command(payload).await;
                }
                info!("File-lane drain task exiting (peer closed)");
            });
        }

        // Independent heartbeat task: pushes `Heartbeat` to the writer queue
        // every 5 s regardless of what the main loop is doing.
        // active_connections is reported as 0 because the
        // PeerConnections live on the daemon side; the worker has no
        // map to count. The daemon only logs the field at trace level —
        // its watchdog cares about IPC freshness, not the count.
        let heartbeat_task =
            spawn_heartbeat_task(writer_tx.clone(), tokio::time::Duration::from_secs(5));

        // Watch for the user-input desktop drifting away from the one we
        // were launched on (UAC, lock screen, etc.). The watcher emits one
        // notification per *transition* — repeated reads of the same
        // drifted state are suppressed inside the monitor so we don't
        // flood the IPC, and a return to the bound desktop re-arms it for
        // the next drift.
        //
        // Portable / in-process mode (single process under a user token):
        // the daemon side already no-ops `DesktopChanged` because we
        // can't `CreateProcessAsUserW` ourselves out of session 0 — so
        // running the 1 Hz `OpenInputDesktop` poll just costs CPU and
        // produces a confusing "drift detected" log when UAC fires.
        // Skip the spawn; dropping `desktop_change_tx` immediately
        // closes the channel so the corresponding `select!` arm
        // disables itself and never fires.
        let (desktop_change_tx, mut desktop_change_rx) = mpsc::unbounded_channel::<String>();
        if is_inprocess_worker {
            info!(
                "Portable mode: skipping desktop_monitor (single-process worker cannot \
                 cross window stations; daemon-side DesktopChanged is a no-op anyway)"
            );
            drop(desktop_change_tx);
        } else {
            desktop_monitor::spawn(init_payload.desktop_name.clone(), desktop_change_tx);
        }

        // Display-change watcher: an OS listener (Windows
        // `WM_DISPLAYCHANGE`, Linux RandR) that lets us refresh the
        // per-connection mouse geometry without tearing down the
        // connection. Spawn failure is non-fatal — the worker falls back
        // to refreshing only on explicit IPC triggers
        // (SetVirtualDisplayMode / Attach / Detach). See `display_watcher`
        // module doc.
        let (display_watcher_handle, mut display_change_rx) = match display_watcher::spawn() {
            Ok((w, rx)) => (Some(w), rx),
            Err(e) => {
                warn!(
                    "Display change watcher init failed: {e}. Mouse geometry will only \
                     refresh on explicit triggers (IDD SetMode / Attach / Detach); \
                     user-initiated physical-display resolution changes mid-session will \
                     leave the cursor offset until reconnect."
                );
                // Permanently-silent receiver so the `tokio::select!`
                // arm below safely stays parked.
                let (_dummy_tx, dummy_rx) = mpsc::unbounded_channel();
                (None, dummy_rx)
            }
        };

        loop {
            tokio::select! {
                msg_result = service_msg_rx.recv() => {
                    match msg_result {
                        Some(Some(msg)) => {
                            match msg {
                                ServiceToWorker::Shutdown => {
                                    info!("Received Shutdown command");
                                    if let Err(e) = desk_session.shutdown().await {
                                        error!("DeskSession shutdown error: {}", e);
                                    }
                                    break;
                                }
                                ServiceToWorker::Init(_) => {
                                    warn!("Received duplicate Init, ignoring");
                                }
                                // Media-control IPC. Routed
                                // straight to the producer; the producer
                                // returns immediately (start_media spawns a
                                // dedicated capture thread) so the IPC loop
                                // stays responsive to the watchdog and the
                                // daemon's other commands.
                                ServiceToWorker::StartMedia(payload) => {
                                    if let Some(producer) = media_producer.as_ref() {
                                        info!(
                                            "Worker received StartMedia for {}: codec={:?}, fps={}",
                                            payload.connection_id,
                                            payload.video_codec,
                                            payload.fps,
                                        );
                                        // E2E diagnostic 2026-05-27: snapshot
                                        // layout when capture starts so we
                                        // can correlate "which device the
                                        // browser picked" (payload.video_device)
                                        // with "the current OS layout / which
                                        // monitor is primary right now".
                                        desk_virtual_display::log_active_displays_for_diagnostics(
                                            &format!(
                                                "StartMedia conn={} video_device={:?}",
                                                payload.connection_id,
                                                payload.video_device,
                                            ),
                                        );
                                        // Virtual display: cache the
                                        // original (preserves the user's
                                        // preferred physical capture target
                                        // across attach/detach cycles) and
                                        // hand the producer the active
                                        // payload (which may have
                                        // video_device overridden to the
                                        // attached virtual display).
                                        let active = vd_state.record_start(payload);
                                        // Spin up per-connection input
                                        // handlers alongside the encoder so
                                        // mouse / keyboard input is ready as
                                        // soon as the browser opens its DCs.
                                        input_dispatcher.start_connection(&active);
                                        // Subscribe the connection
                                        // to clipboard sync; the dispatcher
                                        // starts its polling loop on the first
                                        // active connection.
                                        if let Some(d) = clipboard_dispatcher.as_ref() {
                                            d.start_connection(&active).await;
                                        }
                                        // Subscribe the connection
                                        // to file transfer commands.
                                        file_transfer_dispatcher.start_connection(&active).await;
                                        // Subscribe the connection
                                        // to whiteboard draw commands.
                                        whiteboard_dispatcher.start_connection(&active).await;
                                        producer.start_media(active);
                                    } else {
                                        warn!(
                                            "Worker received StartMedia but media producer is \
                                             not configured (no media_pipe_name in Init); ignoring"
                                        );
                                    }
                                }
                                ServiceToWorker::SetConnectionCeiling(payload) => {
                                    connection_ceilings
                                        .set(&payload.connection_id, payload.ceiling)
                                        .await;
                                }
                                ServiceToWorker::StopMedia(payload) => {
                                    vd_state.record_stop(&payload.connection_id);
                                    if let Some(producer) = media_producer.as_ref() {
                                        producer.stop_media(&payload);
                                    }
                                    input_dispatcher.stop_connection(&payload);
                                    if let Some(d) = clipboard_dispatcher.as_ref() {
                                        d.stop_connection(&payload).await;
                                    }
                                    file_transfer_dispatcher.stop_connection(&payload).await;
                                    whiteboard_dispatcher.stop_connection(&payload).await;
                                    connection_ceilings.clear(&payload.connection_id).await;
                                }
                                ServiceToWorker::ForceKeyframe(payload) => {
                                    if let Some(producer) = media_producer.as_ref() {
                                        producer.force_keyframe(&payload.connection_id);
                                    }
                                }
                                ServiceToWorker::UpdateMediaSettings(payload) => {
                                    if let Some(producer) = media_producer.as_ref() {
                                        producer.update_settings(payload);
                                    }
                                }
                                // Input IPC. The daemon already
                                // gated on `accept_control` /
                                // `accept_clipboard_sync` before sending,
                                // so the worker injects unconditionally.
                                ServiceToWorker::MouseInput(payload) => {
                                    input_dispatcher.dispatch_mouse(&payload);
                                }
                                ServiceToWorker::MouseMoveInput(payload) => {
                                    input_dispatcher.dispatch_mouse_move(&payload);
                                }
                                ServiceToWorker::KeyboardInput(payload) => {
                                    input_dispatcher.dispatch_keyboard(&payload);
                                }
                                // Clipboard handlers route to
                                // the per-worker clipboard dispatcher when
                                // it was successfully constructed; otherwise
                                // log + drop so a worker without a clipboard
                                // backend stays alive for video / input.
                                ServiceToWorker::ClipboardWrite(payload) => {
                                    if let Some(d) = clipboard_dispatcher.as_ref() {
                                        d.handle_clipboard_write(payload).await;
                                    } else {
                                        warn!(
                                            "ClipboardWrite dropped — no clipboard backend on this worker"
                                        );
                                    }
                                }
                                ServiceToWorker::ClipboardRequest(payload) => {
                                    if let Some(d) = clipboard_dispatcher.as_ref() {
                                        d.handle_clipboard_request(payload).await;
                                    } else {
                                        warn!(
                                            "ClipboardRequest dropped — no clipboard backend on this worker"
                                        );
                                    }
                                }
                                ServiceToWorker::WhiteboardCommand(payload) => {
                                    whiteboard_dispatcher.handle_command(payload).await;
                                }
                                // Typed-IPC migration batch 1: replaces the
                                // legacy `SignalingMessage` opaque envelope
                                // for these two types. The worker still
                                // dispatches through `DeskSession::
                                // handle_message` because the actual
                                // handlers in `service::signaling` are
                                // shared with the portable / DeskServer WS
                                // path and shouldn't be duplicated; we
                                // rebuild a lightweight `SignalingModel`
                                // from the typed payload so the existing
                                // arms keep working unmodified. Subsequent
                                // batches that retire `handle_message`
                                // entirely will inline these calls.
                                ServiceToWorker::EnablePrivateScreen(payload) => {
                                    dispatch_typed_signaling(
                                        &mut desk_session,
                                        SignalingType::EnablePrivateScreen,
                                        Some(payload.connection_id),
                                        &EnablePrivateScreenData {
                                            enable: payload.enable,
                                        },
                                    )
                                    .await;
                                }
                                ServiceToWorker::UpdateDeskSettings(payload) => {
                                    dispatch_typed_signaling(
                                        &mut desk_session,
                                        SignalingType::UpdateDeskSettings,
                                        Some(payload.connection_id),
                                        &payload.settings,
                                    )
                                    .await;
                                }
                                // Typed-IPC migration batch 2: manager
                                // plane requests. Worker rebuilds a
                                // SignalingModel with the original
                                // request_id so DeskSession::handle_message
                                // emits a response carrying that same
                                // request_id, which the desk_rx outbound
                                // classifier turns into the matching
                                // typed `WorkerToService::Manager*Response`.
                                ServiceToWorker::ManagerSystemInfoRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ManagerSystemInfo,
                                        payload.request_id,
                                        payload.connection_id,
                                        Option::<&()>::None,
                                    )
                                    .await;
                                }
                                ServiceToWorker::ManagerQuerySettingsRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ManagerQuerySettings,
                                        payload.request_id,
                                        payload.connection_id,
                                        Option::<&()>::None,
                                    )
                                    .await;
                                }
                                ServiceToWorker::ManagerFileListRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ManagerFileList,
                                        payload.request_id,
                                        payload.connection_id,
                                        Some(&payload.params),
                                    )
                                    .await;
                                }
                                ServiceToWorker::ManagerFileDeleteRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ManagerFileDelete,
                                        payload.request_id,
                                        payload.connection_id,
                                        Some(&payload.request),
                                    )
                                    .await;
                                }
                                ServiceToWorker::ManagerUpdateSettingsRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ManagerUpdateSettings,
                                        payload.request_id,
                                        payload.connection_id,
                                        Some(&payload.settings),
                                    )
                                    .await;
                                }
                                // Typed-IPC migration batch 3: terminal
                                // plane requests. Worker rebuilds a
                                // SignalingModel with the original
                                // request_id (where applicable) so
                                // DeskSession::handle_message produces
                                // a response carrying that same id,
                                // which the desk_rx outbound classifier
                                // turns into the matching typed
                                // `WorkerToService::TerminalStarted` /
                                // `ListTerminalResponse`. The body-less
                                // request types (`SendDataToTerminal`,
                                // `ResizeTerminal`, `CloseTerminal`)
                                // ride `dispatch_typed_signaling`
                                // because the worker emits no response.
                                ServiceToWorker::StartTerminalRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::StartTerminal,
                                        payload.request_id,
                                        Some(payload.connection_id),
                                        Some(&payload.session),
                                    )
                                    .await;
                                }
                                ServiceToWorker::SendDataToTerminalRequest(payload) => {
                                    dispatch_typed_signaling(
                                        &mut desk_session,
                                        SignalingType::SendDataToTerminal,
                                        Some(payload.connection_id),
                                        &payload.data,
                                    )
                                    .await;
                                }
                                ServiceToWorker::ResizeTerminalRequest(payload) => {
                                    dispatch_typed_signaling(
                                        &mut desk_session,
                                        SignalingType::ResizeTerminal,
                                        Some(payload.connection_id),
                                        &payload.data,
                                    )
                                    .await;
                                }
                                ServiceToWorker::CloseTerminalRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::CloseTerminal,
                                        // CloseTerminal has no response body
                                        // but the legacy handler still calls
                                        // `check_and_get_from_connection_id`
                                        // for logging — `dispatch_typed_*`
                                        // both feed it; we use the explicit
                                        // form so a future test can pin the
                                        // request_id surface in trace logs.
                                        "typed-ipc".to_string(),
                                        Some(payload.connection_id),
                                        Option::<&()>::None,
                                    )
                                    .await;
                                }
                                ServiceToWorker::ListTerminalRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ListTerminal,
                                        payload.request_id,
                                        payload.connection_id,
                                        Option::<&()>::None,
                                    )
                                    .await;
                                }
                                // F1: daemon-side `dc.send` failed. The
                                // daemon already classified + logged the
                                // wire error; the worker owns transfer
                                // state and the browser-facing
                                // `TransferError` JSON shape, so it
                                // aborts the matching transfer and emits
                                // a `TransferError` over its file lane.
                                ServiceToWorker::FileTransferSendFailed(payload) => {
                                    file_transfer_dispatcher
                                        .handle_send_failed(payload)
                                        .await;
                                }
                                // Virtual display: the daemon owns the
                                // SwDevice handle; the worker owns
                                // attached_display tracking + the
                                // controller (driver pipe + CDS). The
                                // controller is the real Windows IDD
                                // implementation on Windows and a
                                // NotSupported stub on other platforms,
                                // so these arms run against an inert
                                // backend off-Windows.
                                ServiceToWorker::SetVirtualDisplayMode(payload) => {
                                    let controller = Arc::clone(&virtual_display_controller);
                                    let attached = vd_state.attached_display.clone();
                                    info!(
                                        "Worker received SetVirtualDisplayMode: \
                                         conn={}, req={}, target={}x{}@{}, attached={:?}",
                                        payload.connection_id,
                                        payload.request_id,
                                        payload.width,
                                        payload.height,
                                        payload.refresh_hz,
                                        attached.as_deref(),
                                    );
                                    let response = run_set_mode(controller, attached.clone(), payload).await;
                                    // Inspect the outcome *before* moving
                                    // `response` into `writer_tx.send` so we
                                    // can decide whether to refresh input
                                    // geometry after the reply has been
                                    // queued. Refresh after send keeps the
                                    // browser-facing ACK on the critical
                                    // path and the (best-effort, infallible)
                                    // refresh off it.
                                    let should_refresh = should_refresh_after_set_mode(&response);
                                    // Collect WGC-specific restart candidates BEFORE moving
                                    // `response` into the writer channel. WGC restart is NOT
                                    // gated by `should_refresh` (i.e. the IPC outcome variant):
                                    // `pipe_client::send_set_mode` triggers the IDD
                                    // Departure+Arrival cycle at the driver layer *before*
                                    // `apply_cds` runs, so a `DISP_CHANGE_BADMODE` returned by
                                    // `ChangeDisplaySettingsExW` still leaves WGC bound to a
                                    // dead HMONITOR even though the IPC outcome surfaces as
                                    // Failed. Refreshing input geometry, on the other hand,
                                    // is correctly gated on Applied: it would re-read the
                                    // unchanged display rect anyway. DXGI / GDI self-adapt
                                    // natively so we still filter them out.
                                    let restart_steps: Vec<RestartStep> =
                                        if let Some(producer) = media_producer.as_ref() {
                                            let producer_for_lookup = Arc::clone(producer);
                                            select_wgc_restart_steps(
                                                vd_state.restart_steps_for_attached(),
                                                attached.as_deref(),
                                                |id| producer_for_lookup.connection_capture_key(id),
                                            )
                                        } else {
                                            Vec::new()
                                        };
                                    info!(
                                        "SetVirtualDisplayMode processed: \
                                         applied={}, wgc_restart_steps={}",
                                        should_refresh,
                                        restart_steps.len(),
                                    );
                                    if writer_tx.send(response).is_err() {
                                        warn!(
                                            "writer task closed; dropping VirtualDisplayMode \
                                             response"
                                        );
                                    }
                                    if should_refresh {
                                        // The connections matching the
                                        // attached display were already
                                        // retargeted onto it by the Attach
                                        // path; refresh_geometry rewrites
                                        // their stale rect to the
                                        // freshly-applied resolution.
                                        input_dispatcher.refresh_geometry(attached.as_deref());
                                    }
                                    if !restart_steps.is_empty()
                                        && let Some(producer) = media_producer.as_ref()
                                    {
                                        // Dedup so two WGC connections sharing the same
                                        // (backend, device_name) slot only trigger one
                                        // registry eviction.
                                        let keys_to_invalidate = dedup_capture_keys(
                                            &restart_steps,
                                            |id| producer.connection_capture_key(id),
                                        );
                                        for key in &keys_to_invalidate {
                                            let evicted = producer.invalidate_capture_key(key);
                                            info!(
                                                "SetVirtualDisplayMode: invalidated capture key \
                                                 backend={} device={} evicted={}",
                                                key.backend, key.device_name, evicted,
                                            );
                                        }
                                        for step in restart_steps {
                                            producer.stop_media(&StopMediaPayload {
                                                connection_id: step.connection_id.clone(),
                                            });
                                            producer.start_media(step.active);
                                        }
                                    }
                                }
                                ServiceToWorker::AttachVirtualDisplay(payload) => {
                                    info!(
                                        "Worker received AttachVirtualDisplay: instance_id={}",
                                        payload.instance_id,
                                    );
                                    // The daemon (Session 0) cannot resolve
                                    // the GDI display name; we do it here in
                                    // the user session via
                                    // `desk_virtual_display::resolve_display_name`,
                                    // with bounded backoff retries to cover
                                    // the IDD bring-up window. The supervisor
                                    // uses our reply to decide whether to
                                    // promote its state machine to Attached.
                                    let instance_id = payload.instance_id;
                                    let outcome = resolve_attach_with_backoff(
                                        &instance_id,
                                        desk_virtual_display::resolve_display_name,
                                        tokio::time::sleep,
                                    )
                                    .await;
                                    if let VirtualDisplayAttachOutcome::Attached(ref display_name) =
                                        outcome
                                    {
                                        info!(
                                            "Resolved virtual display instance_id {} -> {}",
                                            instance_id, display_name,
                                        );
                                        // E2E diagnostic 2026-05-27: log full
                                        // GDI layout right after a new IDD
                                        // monitor attaches, so we can see if
                                        // Windows put it at primary by
                                        // default (the suspected cause of
                                        // "虚拟屏排第一 / Tauri 弹窗跑到虚拟屏").
                                        desk_virtual_display::log_active_displays_for_diagnostics(
                                            &format!("post-attach virtual={display_name}")
                                        );
                                        let steps = vd_state
                                            .rebuild_active_for_attach(Some(display_name.clone()));
                                        for step in steps {
                                            if let Some(producer) = media_producer.as_ref() {
                                                producer.stop_media(&StopMediaPayload {
                                                    connection_id: step.connection_id.clone(),
                                                });
                                                producer.start_media(step.active.clone());
                                            }
                                            // Mirror the producer Stop+Start
                                            // on the input side: retarget
                                            // updates `video_device` and
                                            // rewrites the SharedMonitorGeometry
                                            // so the very next mouse event
                                            // lands on the new capture
                                            // surface. Without this the
                                            // virtual display would render
                                            // but mouse would still target
                                            // the previous physical screen.
                                            input_dispatcher.retarget_connection(&step.active);
                                        }
                                    } else {
                                        warn!(
                                            "Failed to resolve virtual display instance_id {}; \
                                             not updating attached_display",
                                            instance_id,
                                        );
                                    }
                                    let result_msg = WorkerToService::VirtualDisplayAttachResult(
                                        VirtualDisplayAttachResultPayload {
                                            instance_id,
                                            outcome,
                                        },
                                    );
                                    if writer_tx.send(result_msg).is_err() {
                                        warn!(
                                            "writer task closed; dropping \
                                             VirtualDisplayAttachResult"
                                        );
                                    }
                                }
                                ServiceToWorker::DetachVirtualDisplay => {
                                    info!("Worker received DetachVirtualDisplay");
                                    // E2E diagnostic 2026-05-27: snapshot
                                    // layout the instant the worker is told
                                    // to detach, before any teardown runs.
                                    // Useful for understanding what state
                                    // we were in just before the IDD goes
                                    // away (paired with the post-attach log
                                    // for the next cycle).
                                    desk_virtual_display::log_active_displays_for_diagnostics(
                                        "pre-detach",
                                    );
                                    // codex round 2 #5 + round 7 #5: by this
                                    // point the daemon should have already
                                    // sent SetVirtualDisplayExclusive(false)
                                    // and awaited idle. But if a protocol
                                    // violation lands a Detach while
                                    // exclusive_layout is still Some, run
                                    // leave_exclusive on a spawned task so
                                    // the IPC loop is never blocked by CDS.
                                    let leftover = vd_state
                                        .exclusive_layout
                                        .lock()
                                        .ok()
                                        .and_then(|mut g| g.take());
                                    if let Some(layout) = leftover {
                                        error!(
                                            "daemon protocol violation: \
                                             DetachVirtualDisplay arrived while exclusive_layout \
                                             is still Some; spawning fire-and-forget leave"
                                        );
                                        tokio::spawn(async move {
                                            let res = tokio::task::spawn_blocking(move || {
                                                desk_virtual_display::leave_exclusive(&layout)
                                            })
                                            .await;
                                            match res {
                                                Ok(Ok(())) => {}
                                                Ok(Err(e)) => warn!(
                                                    "[virtual-display] fire-and-forget \
                                                     leave_exclusive failed: {e:?}"
                                                ),
                                                Err(je) => warn!(
                                                    "[virtual-display] fire-and-forget leave \
                                                     join: {je}"
                                                ),
                                            }
                                        });
                                    }
                                    let steps = vd_state.rebuild_active_for_attach(None);
                                    for step in steps {
                                        if let Some(producer) = media_producer.as_ref() {
                                            producer.stop_media(&StopMediaPayload {
                                                connection_id: step.connection_id.clone(),
                                            });
                                            producer.start_media(step.active.clone());
                                        }
                                        // Detach restores the original
                                        // physical capture target; retarget
                                        // walks the input state back so
                                        // the cursor lands on the
                                        // physical display again.
                                        input_dispatcher.retarget_connection(&step.active);
                                    }
                                }
                                ServiceToWorker::RefreshCapabilities => {
                                    // Daemon (typically the
                                    // `VirtualDisplaySupervisor` on the
                                    // Attaching -> Attached or
                                    // Attached -> Disabled edge) asked
                                    // us to re-enumerate displays and
                                    // re-publish capabilities. We
                                    // re-use the `desktop_name` /
                                    // `host_upstream_url` context from
                                    // the cached `init_payload` so the
                                    // daemon's snapshot stays
                                    // consistent with the initial
                                    // Capabilities the worker sent at
                                    // startup.
                                    info!("Worker received RefreshCapabilities");
                                    let capabilities = MediaProducer::build_capabilities(
                                        init_payload.desktop_name.as_deref(),
                                        init_payload.host_upstream_url.is_some(),
                                    );
                                    if writer_tx
                                        .send(WorkerToService::Capabilities(capabilities))
                                        .is_err()
                                    {
                                        warn!(
                                            "writer task closed; dropping refreshed \
                                             Capabilities"
                                        );
                                    }
                                }
                                ServiceToWorker::SetVirtualDisplayExclusive(payload) => {
                                    info!(
                                        "Worker received SetVirtualDisplayExclusive \
                                         op_id={} desired={} prompt_ms={}",
                                        payload.op_id,
                                        payload.desired,
                                        payload.prompt_duration_ms
                                    );
                                    // Hand off to the coordinator; the
                                    // IPC loop does NOT await here so
                                    // a multi-second prompt + CDS does
                                    // not stall heartbeats or block
                                    // subsequent Detach commands.
                                    exclusive_coord.request(
                                        payload.op_id,
                                        payload.desired,
                                        payload.prompt_duration_ms,
                                        vd_state.attached_display.clone(),
                                        Arc::clone(&vd_state.exclusive_layout),
                                        writer_tx.clone(),
                                    );
                                }
                                ServiceToWorker::AgentRequest(payload) => {
                                    info!(
                                        "Worker received AgentRequest req={} conn={:?}",
                                        payload.request_id, payload.connection_id,
                                    );
                                    // Run the collector off the IPC loop so a
                                    // slow read (process enumeration, screen
                                    // capture, docker probe, ...) never stalls
                                    // heartbeats or other commands. The reply
                                    // rides the same `writer_tx` every other
                                    // worker → daemon message uses; capability-
                                    // level errors travel inside the
                                    // `AgentOutcome`, not the transport state.
                                    let writer_tx = writer_tx.clone();
                                    let agent_settings = shared_settings.clone();
                                    tokio::spawn(async move {
                                        let agent =
                                            LocalDeviceAgent::with_settings(agent_settings)
                                                .with_audit(std::sync::Arc::new(
                                                    crate::worker::agent::audit_sink::LogAuditSink,
                                                ));
                                        let outcome = match agent.invoke(payload.envelope).await {
                                            Ok(output) => AgentOutcome::Ok(output),
                                            Err(error) => AgentOutcome::Err(error),
                                        };
                                        if writer_tx
                                            .send(WorkerToService::AgentResponse(
                                                AgentResponsePayload {
                                                    request_id: payload.request_id,
                                                    connection_id: payload.connection_id,
                                                    outcome,
                                                },
                                            ))
                                            .is_err()
                                        {
                                            warn!(
                                                "writer task closed; dropping AgentResponse"
                                            );
                                        }
                                    });
                                }
                                ServiceToWorker::ExecPlan(payload) => {
                                    info!(
                                        "Worker received ExecPlan req={} template={} conn={:?}",
                                        payload.request_id,
                                        payload.plan.template_id,
                                        payload.connection_id,
                                    );
                                    // Execute off the IPC loop so a slow command
                                    // (up to its timeout) never stalls heartbeats
                                    // or other commands. The result rides the same
                                    // `writer_tx`; execution failures travel inside
                                    // the `AgentOutcome`, not the transport.
                                    let writer_tx = writer_tx.clone();
                                    tokio::spawn(async move {
                                        let outcome =
                                            crate::worker::exec::execute_plan(&payload.plan).await;
                                        let result = desk_agent_protocol::exec::ExecResultPayload {
                                            exec_request_id: payload.plan.exec_request_id,
                                            outcome,
                                        };
                                        if writer_tx
                                            .send(WorkerToService::ExecResult(
                                                ExecResultIpcPayload {
                                                    request_id: payload.request_id,
                                                    connection_id: payload.connection_id,
                                                    result,
                                                    // Echo the ledger key back so
                                                    // the daemon can attribute the
                                                    // completion to the operator.
                                                    audit_source_request_id: payload
                                                        .audit_source_request_id,
                                                },
                                            ))
                                            .is_err()
                                        {
                                            warn!("writer task closed; dropping ExecResult");
                                        }
                                    });
                                }
                            }
                        }
                        Some(None) => {
                            info!("IPC event transport closed by Service");
                            break;
                        }
                        None => {
                            info!("IPC reader task stopped");
                            break;
                        }
                    }
                }

                desk_msg = desk_rx.recv() => {
                    match desk_msg {
                        Some(DeskSessionMessage::Text(text)) => {
                            // Worker-emitted signaling reply (terminal
                            // output, manager queries, file/system info
                            // responses, error responses, ...). Every
                            // SignalingType the daemon needs to surface
                            // to the browser is shipped via a dedicated
                            // typed `WorkerToService::*` variant — error
                            // responses go through the SignalingError
                            // catch-all regardless of their original
                            // type. After batch 4 of the typed-IPC
                            // migration there is no opaque-envelope
                            // bridge fallback; unrouted text is logged
                            // + dropped inside the helper.
                            if let Some(payload) =
                                build_outbound_payload_from_desk_text(text.to_string())
                                && writer_tx.send(payload).is_err()
                            {
                                error!("IPC writer task died; exiting main loop");
                                break;
                            }
                        }
                        Some(DeskSessionMessage::Binary(_bin)) => {
                            warn!("DeskSession sent binary message, skipping IPC forward");
                        }
                        Some(DeskSessionMessage::Close) => {
                            info!("DeskSession requested close");
                            break;
                        }
                        Some(DeskSessionMessage::Ping(_)) | Some(DeskSessionMessage::Pong(_)) => {}
                        None => {
                            info!("DeskSession channel closed");
                            break;
                        }
                    }
                }

                Some(new_desktop) = desktop_change_rx.recv() => {
                    info!("Reporting desktop drift to daemon: '{}'", new_desktop);
                    let payload = WorkerToService::DesktopChanged(DesktopChangedPayload {
                        name: new_desktop,
                    });
                    if writer_tx.send(payload).is_err() {
                        error!("IPC writer task died; exiting main loop");
                        break;
                    }
                    // Stay in the loop. If the daemon decides to switch
                    // workers it will send `DesktopSwitching` back, which
                    // is handled by the service_msg_rx arm above.
                    // For Winlogon (UAC) the daemon currently keeps us
                    // alive — see signaling_proxy::run_signaling_proxy.
                }

                // OS-driven display reconfiguration (resolution change,
                // monitor add/remove, primary swap). The broadcast does
                // not tell us *which* display changed, so refresh every
                // active connection's geometry — the read-side cost is
                // a single RwLock write per connection.
                Some(evt) = display_change_rx.recv() => {
                    info!(
                        "Display configuration change received (seq={}); refreshing input \
                         geometry for all connections",
                        evt.seq
                    );
                    // The OS display-change notification fires after every
                    // layout transition (exclusive enter/leave, virtual
                    // display attach, a user manually changing display
                    // settings, etc.). Logging the resulting layout here
                    // gives a per-event snapshot of what the OS reports
                    // right after each transition.
                    desk_virtual_display::log_active_displays_for_diagnostics(
                        &format!("display change seq={}", evt.seq),
                    );
                    refresh_geometry_after_display_change(
                        &input_dispatcher,
                        media_producer.as_deref(),
                    );
                }

                // E2E fix 2026-05-27: the exclusive coordinator posts
                // here after each successful enter_exclusive /
                // leave_exclusive CDS batch. We run the same
                // `invalidate_capture_key + Stop/Start media` cycle
                // SetVirtualDisplayMode already does — WGC bound to
                // the now-stale HMONITOR keeps emitting frozen frames
                // otherwise. Modelled after the SetVirtualDisplayMode
                // arm above; the dedup + restart steps logic is the
                // same.
                Some(commit_evt) = exclusive_commit_rx.recv() => {
                    info!(
                        "ExclusiveCommit received ({:?}); restarting WGC capture for \
                         attached virtual display",
                        commit_evt,
                    );
                    let attached = vd_state.attached_display.clone();
                    if let Some(producer) = media_producer.as_ref() {
                        let producer_for_lookup = Arc::clone(producer);
                        let restart_steps: Vec<RestartStep> = select_wgc_restart_steps(
                            vd_state.restart_steps_for_attached(),
                            attached.as_deref(),
                            |id| producer_for_lookup.connection_capture_key(id),
                        );
                        if restart_steps.is_empty() {
                            info!(
                                "ExclusiveCommit({:?}): no WGC restart candidates \
                                 (attached={:?}, restart_steps=0)",
                                commit_evt, attached,
                            );
                        } else {
                            let keys_to_invalidate = dedup_capture_keys(
                                &restart_steps,
                                |id| producer.connection_capture_key(id),
                            );
                            for key in &keys_to_invalidate {
                                let evicted = producer.invalidate_capture_key(key);
                                info!(
                                    "ExclusiveCommit({:?}): invalidated capture key \
                                     backend={} device={} evicted={}",
                                    commit_evt, key.backend, key.device_name, evicted,
                                );
                            }
                            for step in restart_steps {
                                producer.stop_media(&StopMediaPayload {
                                    connection_id: step.connection_id.clone(),
                                });
                                producer.start_media(step.active);
                            }
                        }
                    }
                }
            }
        }

        // Order matters: stop the heartbeat task first so it doesn't keep
        // pushing into writer_tx, then shut down media-producer pipeline
        // threads (each one observes its `stop_flag` within one frame
        // tick and drops its `MediaSender`, which in turn lets the framed
        // writer task on the media pipe drain and exit). Finally drop our
        // own writer_tx so the event-pipe writer task observes "all
        // senders gone" and exits cleanly.
        heartbeat_task.abort();
        // Drop the display watcher early so its message-pump thread
        // unblocks before the rest of the shutdown chain runs. The
        // Drop impl posts `WM_CLOSE` and joins the thread; absent a
        // working watcher (Err path during init) this is a cheap
        // no-op.
        drop(display_watcher_handle);
        if let Some(producer) = media_producer.as_ref() {
            producer.shutdown();
        }
        input_dispatcher.shutdown();
        if let Some(d) = clipboard_dispatcher.as_ref() {
            d.shutdown().await;
        }
        file_transfer_dispatcher.shutdown().await;
        whiteboard_dispatcher.shutdown().await;
        drop(writer_tx);
        let _ = writer_task.await;

        info!("WorkerSession IPC loop exiting");
        Ok(())
    }

    #[cfg(target_os = "windows")]
    async fn connect_windows_pipe(
        &self,
        pipe_name: &str,
    ) -> Result<
        (
            impl AsyncRead + Unpin + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
        Box<dyn std::error::Error>,
    > {
        use tokio::net::windows::named_pipe::ClientOptions;

        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        info!("Connecting to Named Pipe: {}", pipe_path);

        let client = {
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(&pipe_path) {
                    Ok(client) => break client,
                    Err(e) if attempts < 10 => {
                        attempts += 1;
                        warn!(
                            "Pipe not ready (attempt {}), retrying in 500ms: {}",
                            attempts, e
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to pipe after {} attempts: {}",
                            attempts, e
                        );
                        return Err(Box::new(e));
                    }
                }
            }
        };

        let (reader, writer) = tokio::io::split(client);
        Ok((reader, writer))
    }

    #[cfg(not(target_os = "windows"))]
    async fn connect_unix_socket(
        &self,
        socket_path: &str,
    ) -> Result<
        (
            impl AsyncRead + Unpin + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
        Box<dyn std::error::Error>,
    > {
        use tokio::net::UnixStream;

        info!("Connecting to Unix socket: {}", socket_path);
        let stream = UnixStream::connect(socket_path).await?;
        let (reader, writer) = tokio::io::split(stream);
        Ok((reader, writer))
    }
}

/// Open the daemon-side media pipe (Windows: named pipe; Unix: domain
/// socket) and wrap the writer half in a [`MediaSender`] that flushes
/// onto it via the framed transport from `desk-ipc-protocol`.
///
/// Reader half is dropped because the media transport is uni-
/// directional (worker → daemon). The daemon does not push
/// commands on this pipe — it uses the event pipe for that.
async fn connect_media_pipe(
    pipe_name: &str,
) -> Result<Arc<dyn MediaSender>, Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        // Same retry loop as the event pipe — the daemon creates the
        // pipe as part of `run_pipe_server` but a fast worker may dial
        // before that point.
        let client = {
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(&pipe_path) {
                    Ok(c) => break c,
                    Err(e) if attempts < 10 => {
                        attempts += 1;
                        warn!(
                            "Media pipe not ready (attempt {}), retrying in 200ms: {}",
                            attempts, e
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
        };
        let (_reader, writer) = tokio::io::split(client);
        Ok(framed::spawn_media_sender(writer))
    }
    #[cfg(not(target_os = "windows"))]
    {
        use tokio::net::UnixStream;
        let stream = UnixStream::connect(pipe_name).await?;
        let (_reader, writer) = tokio::io::split(stream);
        Ok(framed::spawn_media_sender(writer))
    }
}

/// Open the daemon-side **file-transfer** pipe (Windows: named pipe;
/// Unix: domain socket). Unlike [`connect_media_pipe`] this transport
/// is **bidirectional**: the worker emits download chunks / control
/// replies on the writer half, and consumes upload chunks / control
/// commands on the reader half. The framed sender uses
/// `FILE_QUEUE_CAP = 32` so backpressure surfaces at the worker as a
/// parked `send().await` inside the dispatcher's `emit_*` helpers.
async fn connect_file_pipe(
    pipe_name: &str,
) -> Result<
    (
        Arc<dyn EventSender<FileTransferPayload>>,
        Box<dyn EventReceiver<FileTransferPayload>>,
    ),
    Box<dyn std::error::Error>,
> {
    #[cfg(target_os = "windows")]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        let client = {
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(&pipe_path) {
                    Ok(c) => break c,
                    Err(e) if attempts < 10 => {
                        attempts += 1;
                        warn!(
                            "File pipe not ready (attempt {}), retrying in 200ms: {}",
                            attempts, e
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
        };
        let (reader, writer) = tokio::io::split(client);
        let sender = framed::spawn_file_sender::<_, FileTransferPayload>(writer);
        let receiver = framed::make_event_receiver::<_, FileTransferPayload>(reader);
        Ok((sender, receiver))
    }
    #[cfg(not(target_os = "windows"))]
    {
        use tokio::net::UnixStream;
        let stream = UnixStream::connect(pipe_name).await?;
        let (reader, writer) = tokio::io::split(stream);
        let sender = framed::spawn_file_sender::<_, FileTransferPayload>(writer);
        let receiver = framed::make_event_receiver::<_, FileTransferPayload>(reader);
        Ok((sender, receiver))
    }
}

/// Whether [`WorkerSession::run_with_transports`] should call
/// [`crate::telemetry::init_telemetry`] for itself.
///
/// `shared_hub_is_some` is `true` whenever the worker runs in-process inside
/// the host (portable / DeskServer modes); `false` for the named-pipe
/// SessionWorker path that runs in a dedicated OS process.
///
/// Telemetry init installs the **global default** tracing subscriber, which
/// can only be set once per process. Calling it again from an in-process
/// worker panics with `SetGlobalDefaultError`. Conversely, the named-pipe
/// worker is a separate process whose subscriber slot is empty, so it must
/// init.
fn should_init_worker_telemetry(shared_hub_is_some: bool) -> bool {
    !shared_hub_is_some
}

/// Spawn a task that drains the dispatcher-facing mpsc and forwards each
/// message onto the supplied [`EventSender`]. Replaces the old byte-stream
/// writer task so the same forwarder works for the named-pipe path (where
/// the sender is `framed::FramedEventSender`) and the in-process path
/// (where the sender is `inprocess::InProcessEventSender`). Decoupling the
/// forwarder from the main `select!` preserves the property that a slow
/// handler cannot stall heartbeats or other queued outbound messages. The
/// task exits when all dispatcher senders drop (clean shutdown) or when
/// the underlying transport returns `Closed`.
fn spawn_event_forwarder_task(
    mut rx: mpsc::UnboundedReceiver<WorkerToService>,
    sender: Arc<dyn EventSender<WorkerToService>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = sender.send(msg).await {
                warn!("Failed to forward IPC message: {}", e);
                break;
            }
        }
    })
}

/// Spawn an independent heartbeat task that pushes `Heartbeat` to the writer
/// queue every `interval`. Runs in its own task so it stays alive even when
/// the main `select!` is blocked awaiting a long handler. The task exits when
/// the writer queue is closed (writer task gone) or it is aborted.
fn spawn_heartbeat_task(
    writer_tx: mpsc::UnboundedSender<WorkerToService>,
    interval: tokio::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(interval);
        loop {
            timer.tick().await;
            let hb = WorkerToService::Heartbeat(HeartbeatPayload {
                timestamp_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                active_connections: 0,
                cpu_usage: None,
                memory_usage: None,
            });
            if writer_tx.send(hb).is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_control::HubMode;
    use desk_utils::error::DeskErrorCode;

    fn payload_with(
        host_upstream_url: Option<String>,
        auth_token: Option<String>,
    ) -> WorkerInitPayload {
        WorkerInitPayload {
            session_id: "session-1".into(),
            os_session_id: 1,
            desktop_name: None,
            config_json: "{}".into(),
            signaling_url: None,
            auth_token,
            host_upstream_url,
            media_pipe_name: None,
            file_pipe_name: None,
            config_file_path: None,
        }
    }

    /// When the daemon supplies a host_upstream_url the worker constructs a
    /// Forwarder hub and emits a spec the caller can spawn the ws task with.
    #[tokio::test]
    async fn build_hub_forwarder_when_url_present() {
        let payload = payload_with(
            Some("ws://127.0.0.1:8082/ws/host_upstream".into()),
            Some("ipc-token".into()),
        );
        let (hub, spec) = build_hub_from_init(&payload);
        assert_eq!(hub.mode(), HubMode::Forwarder);
        let (upstream, url, token) = spec.expect("Forwarder must yield an upstream spec");
        assert_eq!(url, "ws://127.0.0.1:8082/ws/host_upstream");
        assert_eq!(token, "ipc-token");
        // Upstream starts disconnected; hub should mirror that until the ws
        // task connects (which the test doesn't exercise).
        assert!(!upstream.is_connected());
    }

    /// Missing host_upstream_url falls back to a Local hub and yields no spec.
    #[test]
    fn build_hub_local_when_url_absent() {
        let payload = payload_with(None, None);
        let (hub, spec) = build_hub_from_init(&payload);
        assert_eq!(hub.mode(), HubMode::Local);
        assert!(spec.is_none());
    }

    /// Telemetry must initialize for the named-pipe SessionWorker path
    /// (`shared_hub == None`, the worker is its own OS process) and must NOT
    /// initialize for the in-process portable / DeskServer path
    /// (`shared_hub == Some(_)`, the host process already installed the
    /// global tracing subscriber). A double-init in the in-process path
    /// would panic with `SetGlobalDefaultError`, which is exactly the bug
    /// surfaced when portable mode tried to spawn an in-process worker.
    #[test]
    fn telemetry_init_skipped_when_shared_hub_present() {
        // In-process worker: host already inited → must skip.
        assert!(!should_init_worker_telemetry(true));
        // Named-pipe worker: separate process → must init.
        assert!(should_init_worker_telemetry(false));
    }

    /// Forwarder hub built without an auth token still works (passes empty
    /// string to ws task — daemon will reject the handshake, which is the
    /// intended fail-fast behaviour).
    #[tokio::test]
    async fn build_hub_forwarder_empty_token_when_auth_token_none() {
        let payload = payload_with(Some("ws://127.0.0.1:8082/ws/host_upstream".into()), None);
        let (_hub, spec) = build_hub_from_init(&payload);
        let (_, _, token) = spec.expect("spec must be present");
        assert_eq!(token, "");
    }

    /// Heartbeat task fires on every interval tick and stops when the writer
    /// queue is closed. Uses a 50 ms real interval to keep the test fast while
    /// still exercising the timing path (`tokio::time::advance` would require
    /// the test-util feature which isn't enabled in regular dependencies).
    #[tokio::test]
    async fn heartbeat_task_emits_on_interval_until_queue_closed() {
        let (tx, mut rx) = mpsc::unbounded_channel::<WorkerToService>();
        let interval = tokio::time::Duration::from_millis(50);
        let task = spawn_heartbeat_task(tx, interval);

        // First two ticks must arrive within ~3 intervals worth of slack.
        let first = tokio::time::timeout(interval * 3, rx.recv())
            .await
            .expect("first heartbeat must arrive")
            .expect("queue closed unexpectedly");
        assert!(matches!(first, WorkerToService::Heartbeat(_)));

        let second = tokio::time::timeout(interval * 3, rx.recv())
            .await
            .expect("second heartbeat must arrive")
            .expect("queue closed unexpectedly");
        assert!(matches!(second, WorkerToService::Heartbeat(_)));

        // Closing the receiver causes the task to detect Err on send and exit.
        drop(rx);
        tokio::time::timeout(interval * 5, task)
            .await
            .expect("heartbeat task must exit after queue closes")
            .expect("task panicked");
    }

    /// Forwarder task drains the dispatcher-facing mpsc and pushes onto the
    /// supplied [`EventSender`] in order, then exits when all senders are
    /// dropped. Uses the in-process transport so the test stays fully sync
    /// (no IO scheduling); the framed-transport path is exercised by the
    /// `inproc_event_round_trips` / `framed_event_round_trips_through_duplex`
    /// tests in `desk_ipc_protocol::dual_transport`.
    #[tokio::test]
    async fn event_forwarder_drains_queue_and_exits_when_senders_dropped() {
        use desk_ipc_protocol::dual_transport::inprocess;

        let (sender, mut receiver) = inprocess::make_event::<WorkerToService>();
        let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
        let task = spawn_event_forwarder_task(rx, sender);

        tx.send(WorkerToService::Ready).expect("send Ready");
        tx.send(WorkerToService::Heartbeat(HeartbeatPayload {
            timestamp_ms: 1,
            active_connections: 0,
            cpu_usage: None,
            memory_usage: None,
        }))
        .expect("send Heartbeat");
        drop(tx);

        let m1 = receiver.recv().await.expect("recv first message");
        assert!(matches!(m1, WorkerToService::Ready));
        let m2 = receiver.recv().await.expect("recv second message");
        assert!(matches!(m2, WorkerToService::Heartbeat(_)));

        tokio::time::timeout(tokio::time::Duration::from_secs(1), task)
            .await
            .expect("forwarder task must exit after senders drop")
            .expect("task panicked");
    }

    /// Typed-IPC migration batch 1: a `PrivateScreenStateChanged`
    /// blob produced by `DeskSession`'s host-control-hub bridge is
    /// classified into the typed `WorkerToService::PrivateScreenStateChanged`
    /// variant, carrying the inner `PrivateScreenStateChangedData`
    /// verbatim. This guards the rendering decision in
    /// `build_outbound_payload_from_desk_text`.
    #[test]
    fn outbound_dispatch_routes_private_screen_state_changed_to_typed_variant() {
        let data = PrivateScreenStateChangedData {
            visible: true,
            is_supported: true,
            error_msg: None,
        };
        let model = SignalingModel::new_request(
            SignalingType::PrivateScreenStateChanged,
            Some("conn-pss".to_string()),
            Some(&data),
        )
        .expect("build PrivateScreenStateChanged model");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed route") {
            WorkerToService::PrivateScreenStateChanged(p) => {
                assert_eq!(p.connection_id, "conn-pss");
                assert!(p.data.visible);
                assert!(p.data.is_supported);
                assert!(p.data.error_msg.is_none());
            }
            other => panic!("PrivateScreenStateChanged must take the typed path, got {other:?}",),
        }
    }

    /// Batch 4: error responses (any SignalingType, response_state
    /// with non-zero error_code) all flow through the typed
    /// `WorkerToService::SignalingError` catch-all. The daemon
    /// rebuilds a `SignalingModel::error(...)` from this payload so
    /// the browser sees the error response on its pending request.
    #[test]
    fn outbound_dispatch_routes_error_responses_to_typed_signaling_error() {
        // SignalingModel::error builds the canonical wire shape.
        let model = SignalingModel::error(
            "req-bad",
            SignalingType::StartTerminal,
            None,
            Some("conn-term".to_string()),
            DeskErrorCode::PERMISSION_ERROR,
            "Permission denied",
        )
        .expect("build error response");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed error route") {
            WorkerToService::SignalingError(p) => {
                assert_eq!(p.request_id, "req-bad");
                assert_eq!(p.connection_id, "conn-term");
                assert_eq!(p.signaling_type, SignalingType::StartTerminal);
                assert_eq!(p.error_code, DeskErrorCode::PERMISSION_ERROR.code());
                assert_eq!(p.error_message.as_deref(), Some("Permission denied"));
            }
            other => panic!("expected SignalingError, got {other:?}"),
        }
    }

    /// Batch 4: malformed JSON (a `service::signaling` bug or a wire
    /// corruption) is logged + dropped now — there is no
    /// SignalingMessage bridge to ferry it through. Returns `None`.
    #[test]
    fn outbound_dispatch_drops_malformed_signaling_text() {
        let raw = "not-a-signaling-model".to_string();
        assert!(
            build_outbound_payload_from_desk_text(raw).is_none(),
            "malformed JSON must drop, not surface as a typed variant",
        );
    }

    /// Batch 4: an unrecognised `SignalingType` (e.g. `Error`,
    /// `Unknown`, or a brand-new variant the worker emitted before
    /// the daemon learned about) is logged + dropped. Returns `None`.
    /// This is a tightening of the previous SignalingMessage fallback.
    #[test]
    fn outbound_dispatch_drops_unrecognised_signaling_types() {
        let model = SignalingModel::new(
            "stray",
            SignalingType::Error,
            Some("conn-x".to_string()),
            None,
            Some(serde_json::json!({"code": -1, "message": "boom"})),
            None,
        );
        let text = serde_json::to_string(&model).expect("serialise");
        assert!(build_outbound_payload_from_desk_text(text).is_none());
    }

    /// Batch 2: `ManagerSystemInfo` response (built by the worker's
    /// `send_response`) gets routed onto
    /// `WorkerToService::ManagerSystemInfoResponse` carrying the
    /// `request_id`, `connection_id`, and the `SystemInfo` body
    /// verbatim. This guards the typed-routing decision on the
    /// happy path.
    #[test]
    fn outbound_dispatch_routes_manager_system_info_response_to_typed_variant() {
        let info = SystemInfo {
            name: Some("alice-pc".to_string()),
            is_admin: Some(true),
            ..SystemInfo::default()
        };
        let model = SignalingModel::success_response(
            "req-info-1",
            SignalingType::ManagerSystemInfo,
            None,
            Some("conn-info".to_string()),
            Some(&info),
        )
        .expect("build response");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed route") {
            WorkerToService::ManagerSystemInfoResponse(p) => {
                assert_eq!(p.request_id, "req-info-1");
                assert_eq!(p.connection_id.as_deref(), Some("conn-info"));
                assert_eq!(p.info.name.as_deref(), Some("alice-pc"));
                assert_eq!(p.info.is_admin, Some(true));
            }
            other => panic!("expected ManagerSystemInfoResponse, got {other:?}"),
        }
    }

    /// Batch 2: empty-body responses (`ManagerFileDelete`,
    /// `ManagerUpdateSettings`) ride
    /// `WorkerToService::ManagerResponseRefPayload` — only the
    /// `request_id` + `connection_id` matter. Verify both variants
    /// route to the right enum tag.
    #[test]
    fn outbound_dispatch_routes_empty_body_manager_responses_to_typed_variants() {
        for (signaling_type, expected_variant) in [
            (
                SignalingType::ManagerFileDelete,
                "ManagerFileDeleteResponse",
            ),
            (
                SignalingType::ManagerUpdateSettings,
                "ManagerUpdateSettingsResponse",
            ),
        ] {
            let model = SignalingModel::success_response(
                "req-empty",
                signaling_type,
                None,
                Some("conn-empty".to_string()),
                Some(&()),
            )
            .expect("build response");
            let text = serde_json::to_string(&model).expect("serialise");
            let routed = build_outbound_payload_from_desk_text(text).expect("typed route");
            match (expected_variant, routed) {
                ("ManagerFileDeleteResponse", WorkerToService::ManagerFileDeleteResponse(p))
                | (
                    "ManagerUpdateSettingsResponse",
                    WorkerToService::ManagerUpdateSettingsResponse(p),
                ) => {
                    assert_eq!(p.request_id, "req-empty");
                    assert_eq!(p.connection_id.as_deref(), Some("conn-empty"));
                }
                (expected, other) => {
                    panic!("expected {expected}, got {other:?}");
                }
            }
        }
    }

    /// Batch 3: a `TerminalStarted` blob built by the worker's
    /// `handle_manager_terminal_start` (success_response with the
    /// original request_id) gets routed onto
    /// `WorkerToService::TerminalStarted` carrying the request_id +
    /// connection_id; daemon's send_manager_response rebuilds it
    /// back to the browser as a SignalingType::TerminalStarted
    /// response with that same id.
    #[test]
    fn outbound_dispatch_routes_terminal_started_to_typed_variant() {
        let model = SignalingModel::success_response::<()>(
            "req-start",
            SignalingType::TerminalStarted,
            None,
            Some("conn-term".to_string()),
            None,
        )
        .expect("build response");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed route") {
            WorkerToService::TerminalStarted(p) => {
                assert_eq!(p.request_id, "req-start");
                assert_eq!(p.connection_id, "conn-term");
            }
            other => panic!("expected TerminalStarted, got {other:?}"),
        }
    }

    /// Batch 3: `TerminalClosed` is a server-initiated notification
    /// (`new_request`) — `request_id` is auto-minted, no correlation
    /// needed; the typed payload carries only `connection_id`.
    #[test]
    fn outbound_dispatch_routes_terminal_closed_to_typed_variant() {
        let model = SignalingModel::new_request::<()>(
            SignalingType::TerminalClosed,
            Some("conn-term".to_string()),
            None,
        )
        .expect("build new_request");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed route") {
            WorkerToService::TerminalClosed(p) => {
                assert_eq!(p.connection_id, "conn-term");
            }
            other => panic!("expected TerminalClosed, got {other:?}"),
        }
    }

    /// Batch 3: `ReplyFromTerminal` is the high-frequency PTY-output
    /// path. The PTY reader thread builds it via `new_request` with a
    /// `TerminalOutputData` body; verify the body survives the typed
    /// route + the connection_id is read from `to_connection_id`
    /// (server-initiated request, target browser is the destination).
    #[test]
    fn outbound_dispatch_routes_reply_from_terminal_to_typed_variant() {
        let body = TerminalOutputData {
            content: "hello\r\nworld\r\n".to_string(),
        };
        let model = SignalingModel::new_request(
            SignalingType::ReplyFromTerminal,
            Some("conn-term".to_string()),
            Some(&body),
        )
        .expect("build new_request");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed route") {
            WorkerToService::ReplyFromTerminal(p) => {
                assert_eq!(p.connection_id, "conn-term");
                assert_eq!(p.data.content, "hello\r\nworld\r\n");
            }
            other => panic!("expected ReplyFromTerminal, got {other:?}"),
        }
    }

    /// Batch 3: `ListTerminal` response carries the `TerminalList` in
    /// the body. `handle_list_terminals` uses `send_response` which
    /// writes `to_connection_id` + the original request_id.
    #[test]
    fn outbound_dispatch_routes_list_terminal_to_typed_variant() {
        let terminals = TerminalList {
            commands: vec![vec!["C:\\Windows\\System32\\cmd.exe".to_string()]],
            current: 0,
        };
        let model = SignalingModel::success_response(
            "req-list",
            SignalingType::ListTerminal,
            None,
            Some("conn-list".to_string()),
            Some(&terminals),
        )
        .expect("build response");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed route") {
            WorkerToService::ListTerminalResponse(p) => {
                assert_eq!(p.request_id, "req-list");
                assert_eq!(p.connection_id.as_deref(), Some("conn-list"));
                assert_eq!(p.terminals.commands.len(), 1);
                assert_eq!(p.terminals.current, 0);
            }
            other => panic!("expected ListTerminalResponse, got {other:?}"),
        }
    }

    /// Batch B (HTTP-API correlation): a `ManagerSystemInfo` response
    /// produced by `handle_manager_system_info` for a HTTP-REST-
    /// triggered request carries `to_connection_id == None` because
    /// the original request from `signal-facade::request_peer_with_callback`
    /// had no `from_connection_id`. The typed dispatcher must still
    /// route it (the daemon's signal/manager bus correlates the
    /// response by `request_id` alone in that case); a previous
    /// `model.to_connection_id.clone()?` short-circuit silently
    /// dropped these and broke `GET /api/desk/files/...`.
    #[test]
    fn outbound_dispatch_manager_response_without_to_connection_routes_with_none() {
        let info = SystemInfo::default();
        let model = SignalingModel::success_response(
            "req-info-noid",
            SignalingType::ManagerSystemInfo,
            None,
            None, // HTTP-API trigger: no originating browser PC
            Some(&info),
        )
        .expect("build response");
        let text = serde_json::to_string(&model).expect("serialise");
        match build_outbound_payload_from_desk_text(text).expect("typed route") {
            WorkerToService::ManagerSystemInfoResponse(p) => {
                assert_eq!(p.request_id, "req-info-noid");
                assert!(p.connection_id.is_none());
            }
            other => panic!("expected ManagerSystemInfoResponse, got {other:?}"),
        }
    }

    /// Forwarder task exits immediately if the underlying transport returns
    /// `Closed` on the first send. Built by dropping the in-process
    /// receiver before any forwarder send happens — the next `send` then
    /// surfaces `TransportError::Closed`.
    #[tokio::test]
    async fn event_forwarder_exits_when_transport_closed() {
        use desk_ipc_protocol::dual_transport::inprocess;

        let (sender, receiver) = inprocess::make_event::<WorkerToService>();
        drop(receiver);
        let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
        let task = spawn_event_forwarder_task(rx, sender);

        // Push one message; forwarder will observe `Closed` and exit.
        tx.send(WorkerToService::Ready).expect("send Ready");

        tokio::time::timeout(tokio::time::Duration::from_secs(1), task)
            .await
            .expect("forwarder task must exit after transport closes")
            .expect("task panicked");
    }

    /// `SetVirtualDisplayMode` Applied → the worker should refresh
    /// per-connection input geometry on the attached display.
    /// This predicate gates that branch, so unit-test it directly.
    #[test]
    fn should_refresh_after_set_mode_returns_true_on_applied() {
        use desk_ipc_protocol::message::{
            VirtualDisplayModeData, VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload,
        };
        let response = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
            request_id: "r".into(),
            connection_id: "c".into(),
            outcome: VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
                width: 2560,
                height: 1440,
                refresh_hz: 60,
            }),
        });
        assert!(should_refresh_after_set_mode(&response));
    }

    /// `SetVirtualDisplayMode` Failed → no geometry refresh; the IDD
    /// mode did not actually change, so the existing rect is still
    /// authoritative.
    #[test]
    fn should_refresh_after_set_mode_returns_false_on_failed() {
        use desk_ipc_protocol::message::{
            VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload,
        };
        let response = WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
            request_id: "r".into(),
            connection_id: "c".into(),
            outcome: VirtualDisplayModeOutcome::Failed("invalid mode".into()),
        });
        assert!(!should_refresh_after_set_mode(&response));
    }

    /// Non-VirtualDisplayMode responses must never trigger a refresh.
    /// Guards against a typo that would catch the wrong variant via the
    /// `matches!` macro.
    #[test]
    fn should_refresh_after_set_mode_returns_false_on_other_variants() {
        assert!(!should_refresh_after_set_mode(&WorkerToService::Ready));
    }

    // -----------------------------------------------------------------
    // WGC mid-session restart helpers (see select_wgc_restart_steps +
    // dedup_capture_keys). The SetVirtualDisplayMode handler funnels
    // through these so the unit tests can drive arbitrary
    // (connection, CaptureKey) fixtures without spinning up real
    // capture backends.
    // -----------------------------------------------------------------

    use desk_ipc_protocol::message::{MediaCodec, StartMediaPayload};
    use std::collections::HashMap;

    fn make_step(connection_id: &str, device: &str) -> RestartStep {
        RestartStep {
            connection_id: connection_id.to_string(),
            active: StartMediaPayload {
                connection_id: connection_id.to_string(),
                video_codec: MediaCodec::H264,
                audio_codec: MediaCodec::Opus,
                video_device: Some(device.to_string()),
                audio_device: None,
                fps: 60,
                bitrate_kbps: 4_000,
                quality: 0,
                start_video: true,
                start_audio: true,
                image_capture: None,
                enable_dirty_rect: None,
            },
        }
    }

    fn make_key(backend: &str, device: &str) -> CaptureKey {
        CaptureKey {
            backend: backend.to_string(),
            device_name: device.to_string(),
        }
    }

    /// The core gating contract: only WGC connections that target the
    /// currently attached IDD display are eligible for forced rebuild.
    /// WGC connections on a different display, and DXGI / GDI
    /// connections on the same display, are filtered out.
    #[test]
    fn select_wgc_restart_steps_picks_only_wgc_on_attached() {
        let attached = r"\\.\DISPLAY51";
        let other_display = r"\\.\DISPLAY1";
        let steps = vec![
            make_step("c-wgc-attached", attached),
            make_step("c-wgc-other", other_display),
            make_step("c-dxgi-attached", attached),
        ];
        let mut keys: HashMap<String, CaptureKey> = HashMap::new();
        keys.insert("c-wgc-attached".into(), make_key("WGC", attached));
        keys.insert("c-wgc-other".into(), make_key("WGC", other_display));
        keys.insert("c-dxgi-attached".into(), make_key("DXGI", attached));

        let picked = select_wgc_restart_steps(steps, Some(attached), |id| keys.get(id).cloned());
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].connection_id, "c-wgc-attached");
    }

    /// No attached display ⇒ nothing to rebuild. Guards the handler
    /// from invoking the WGC-only branch on a Detach race.
    #[test]
    fn select_wgc_restart_steps_empty_when_no_attached() {
        let steps = vec![make_step("c-1", r"\\.\DISPLAY1")];
        let keys: HashMap<String, CaptureKey> = HashMap::new();
        let picked = select_wgc_restart_steps(steps, None, |id| keys.get(id).cloned());
        assert!(picked.is_empty());
    }

    /// A connection whose key_lookup returns None (e.g. it never
    /// reached the post-subscribe checkpoint) must be skipped, not
    /// panic. Defensive against transient producer state.
    #[test]
    fn select_wgc_restart_steps_empty_when_lookup_returns_none() {
        let attached = r"\\.\DISPLAY51";
        let steps = vec![make_step("c-1", attached)];
        let picked = select_wgc_restart_steps(steps, Some(attached), |_| None);
        assert!(picked.is_empty());
    }

    /// `ImageCaptureType::WGC` round-trips through `<&'static str>::from`
    /// as exactly "WGC", but `eq_ignore_ascii_case` is a cheap belt to
    /// avoid future-mode drift if someone renames the variant in
    /// lowercase / mixed case downstream.
    #[test]
    fn select_wgc_restart_steps_case_insensitive_backend() {
        let attached = r"\\.\DISPLAY51";
        let steps = vec![make_step("c-1", attached)];
        let mut keys: HashMap<String, CaptureKey> = HashMap::new();
        keys.insert("c-1".into(), make_key("wgc", attached));
        let picked = select_wgc_restart_steps(steps, Some(attached), |id| keys.get(id).cloned());
        assert_eq!(picked.len(), 1);
    }

    /// Two connections sharing the same (backend, device_name) slot
    /// must yield exactly one CaptureKey so the registry is
    /// invalidated once, not twice.
    #[test]
    fn dedup_capture_keys_collapses_duplicates() {
        let attached = r"\\.\DISPLAY51";
        let steps = vec![
            make_step("c-1", attached),
            make_step("c-2", attached),
            make_step("c-3", attached),
            make_step("c-4", r"\\.\DISPLAY1"),
        ];
        let mut keys: HashMap<String, CaptureKey> = HashMap::new();
        let shared = make_key("WGC", attached);
        keys.insert("c-1".into(), shared.clone());
        keys.insert("c-2".into(), shared.clone());
        keys.insert("c-3".into(), shared.clone());
        keys.insert("c-4".into(), make_key("WGC", r"\\.\DISPLAY1"));

        let distinct = dedup_capture_keys(&steps, |id| keys.get(id).cloned());
        assert_eq!(distinct.len(), 2);
    }

    /// Regression test for the WGC mid-session resize blackscreen
    /// **second iteration** (2026-05-24): a CDS `DISP_CHANGE_BADMODE`
    /// (typical on browser-driven shrink to a non-standard size) makes
    /// `run_set_mode` return Failed even though
    /// `pipe_client::send_set_mode` already triggered the IDD
    /// Departure+Arrival cycle. The previous gate
    /// `if should_refresh { compute restart_steps }` would silently
    /// skip the WGC rebuild, leaving the capture loop bound to the
    /// dead HMONITOR (the symptom: persistent
    /// `post-rebuild heartbeat tick produced 0 NALs` in
    /// `desk-worker.log`). The fix decouples WGC restart from the
    /// outcome variant — every successful IPC reception goes through
    /// `select_wgc_restart_steps`, with `should_refresh` retained only
    /// for the input-geometry refresh.
    ///
    /// Concretely this test pins the contract:
    ///   1. `should_refresh_after_set_mode` returns `false` for Failed
    ///      (input geometry must NOT refresh).
    ///   2. `select_wgc_restart_steps` is independently computable —
    ///      it does not consult the response variant at all — so the
    ///      handler can still pick out the WGC connection on the
    ///      attached display when the outcome is Failed.
    /// Tested together so a future refactor that re-couples the two
    /// fails this test rather than silently regressing the fix.
    #[test]
    fn wgc_restart_decoupled_from_failed_outcome() {
        use desk_ipc_protocol::message::{
            VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload,
        };
        // Failed outcome (the CDS BADMODE case).
        let failed_response =
            WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
                request_id: "r".into(),
                connection_id: "c".into(),
                outcome: VirtualDisplayModeOutcome::Failed(
                    "BADMODE for \\\\.\\DISPLAY9 @ 850x770@60; \
                     driver did not advertise this mode"
                        .into(),
                ),
            });
        assert!(
            !should_refresh_after_set_mode(&failed_response),
            "Failed must keep gating input geometry refresh \
             (no actual mode change to reflect)"
        );

        // ...but the WGC restart selector still produces non-empty
        // candidates because it is independent of the response.
        let attached = r"\\.\DISPLAY51";
        let steps = vec![make_step("c-wgc-attached", attached)];
        let mut keys: HashMap<String, CaptureKey> = HashMap::new();
        keys.insert("c-wgc-attached".into(), make_key("WGC", attached));
        let picked = select_wgc_restart_steps(steps, Some(attached), |id| keys.get(id).cloned());
        assert_eq!(
            picked.len(),
            1,
            "select_wgc_restart_steps must NOT consult the IPC outcome — \
             the IDD pipe replug already invalidated HMONITOR, so WGC \
             needs rebuilding even on CDS Failed"
        );
        assert_eq!(picked[0].connection_id, "c-wgc-attached");
    }

    /// Order preserved: callers iterate this list in order to drive
    /// `invalidate_capture_key`, so deterministic FIFO behaviour
    /// keeps log output and integration traces reproducible.
    #[test]
    fn dedup_capture_keys_preserves_order() {
        let attached = r"\\.\DISPLAY51";
        let other = r"\\.\DISPLAY9";
        let steps = vec![
            make_step("c-A", attached),
            make_step("c-B", other),
            make_step("c-C", attached), // dup of A
        ];
        let mut keys: HashMap<String, CaptureKey> = HashMap::new();
        let key_attached = make_key("WGC", attached);
        let key_other = make_key("WGC", other);
        keys.insert("c-A".into(), key_attached.clone());
        keys.insert("c-B".into(), key_other.clone());
        keys.insert("c-C".into(), key_attached.clone());

        let distinct = dedup_capture_keys(&steps, |id| keys.get(id).cloned());
        assert_eq!(distinct, vec![key_attached, key_other]);
    }
}
