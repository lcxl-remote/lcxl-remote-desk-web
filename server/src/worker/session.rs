use crate::{
    host_control::{HostControlHub, UpstreamForwarder, upstream::spawn_upstream_ws_task},
    model::settings::{Args, Settings, SharedSettings, StartupMode},
    service::signaling::{DeskSession, DeskSessionMessage, DeskSessionSender},
    worker::{
        clipboard_dispatcher::ClipboardDispatcher, desktop_monitor,
        file_transfer_dispatcher::FileTransferDispatcher, input_dispatcher::InputDispatcher,
        media_producer::MediaProducer, whiteboard_dispatcher::WhiteboardDispatcher,
    },
};
use actix_web::web;
use desk_ipc_protocol::{
    dual_transport::{EventReceiver, EventSender, MediaSender, framed},
    message::{
        DesktopChangedPayload, HeartbeatPayload, ServiceToWorker, WorkerInitPayload,
        WorkerToService,
    },
    transport::{read_message, write_message},
};
use desk_server_user::model::CurrentUser;
use log::{error, info, warn};
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
    /// directly on the byte stream (so the pre-handshake protocol stays
    /// length-prefix bincode v2 — same as Arch III), then wraps the remaining
    /// stream in `framed` event transports and connects the optional media
    /// pipe before delegating to [`Self::run_with_transports`]. The
    /// transport-agnostic main loop is shared with the in-process portable
    /// path (PR 5) — only the way transports are constructed differs.
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
        // wire format (`LengthDelimitedCodec` + bincode v2) is binary
        // compatible with the `read_message` / `write_message` calls above
        // — both speak length-prefixed bincode-v2 with a 16 MB cap.
        let event_tx: Arc<dyn EventSender<WorkerToService>> = framed::spawn_event_sender(writer);
        let event_rx: Box<dyn EventReceiver<ServiceToWorker>> = framed::make_event_receiver(reader);

        // Arch IV cut 4: optional media pipe. Connect failure is non-fatal —
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

        // Named-pipe path: no shared hub — worker constructs its own
        // (Forwarder if `host_upstream_url` is set, Local otherwise).
        self.run_with_transports(init_payload, event_rx, event_tx, media_sender, None)
            .await
    }

    /// Transport-agnostic main loop. Used by both:
    ///
    /// - the named-pipe / Unix-socket path (after Ready/Init handshake on the
    ///   raw byte stream); and
    /// - the in-process portable path (PR 5) where daemon and worker share
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
    /// `Some`, the supplied hub is used directly (PR 5 portable mode where
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
        shared_hub: Option<Arc<HostControlHub>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let settings = match serde_json::from_str::<Settings>(&init_payload.config_json) {
            Ok(mut s) => {
                // Args is #[serde(skip)] so it defaults to Args::default() after
                // deserialization. SessionWorker always acts as a pure desk server,
                // so set the mode explicitly to satisfy DeskServer-specific checks
                // (e.g. TURN ICE server inclusion in signaling.rs).
                s.args.startup_mode = StartupMode::DeskServer;
                s
            }
            Err(e) => {
                error!("Failed to parse config from Init payload: {}", e);
                let err_msg = WorkerToService::Error(desk_ipc_protocol::message::ErrorPayload {
                    code: -1,
                    message: format!("Failed to parse config: {}", e),
                    recoverable: false,
                });
                let _ = event_tx.send(err_msg).await;
                return Err(Box::new(e));
            }
        };

        let shared_settings = Arc::new(SharedSettings::from(settings));
        let shared_settings_data = web::Data::from(shared_settings.clone());

        // Initialize telemetry for SessionWorker mode (using actual startup mode enum variable instead of args to ensure correct log naming)
        let _guard =
            crate::telemetry::init_telemetry(shared_settings.clone(), &StartupMode::SessionWorker)
                .await?;

        let (desk_tx, mut desk_rx) = mpsc::unbounded_channel::<DeskSessionMessage>();
        let session_sender = DeskSessionSender {
            sender: desk_tx.clone(),
        };

        // Build the host-control hub. In named-pipe daemon mode the daemon
        // supplied a `host_upstream_url` so we run as a Forwarder and bridge
        // approval / private-screen / whiteboard traffic over ws back to the
        // daemon's aggregator. In PR 5 portable mode the caller hands us the
        // daemon's hub directly via `shared_hub` — no ws, no extra task,
        // both ends share the same `Arc`. Standalone / test runs (no
        // upstream and no shared hub) fall back to a Local hub whose
        // approvals deny-fast.
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

        // Arch IV cut 4: build the media producer when the caller supplied a
        // media transport. In named-pipe mode this is the secondary pipe; in
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
        // Cut 5: per-connection input handlers. Constructed once per
        // worker; `start_connection` / `stop_connection` keyed off the
        // same `connection_id` the daemon ships in `StartMedia` /
        // `StopMedia`.
        let input_dispatcher = {
            let desk_settings = shared_settings.read().await.desk.clone();
            Arc::new(InputDispatcher::new(desk_settings))
        };
        // PR 4 cut 1: clipboard dispatcher. Construction can fail when
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
        // PR 4 cut 2: file transfer dispatcher. Always constructible —
        // it owns no resource that can fail at init time.
        let file_transfer_dispatcher = FileTransferDispatcher::new(writer_tx.clone());
        // PR 4 cut 3: whiteboard dispatcher. Spawns a bridge thread to
        // the host_control_hub on construction; reuses the same hub
        // the DeskSession (legacy / portable path) uses so messages
        // flow through a single Tauri overlay manager.
        let whiteboard_dispatcher = WhiteboardDispatcher::new(Arc::clone(&host_control_hub));
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
        )
        .await
        .map_err(|e| format!("Failed to create DeskSession: {}", e))?;

        info!("DeskSession created successfully, entering main loop");

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

        // Independent heartbeat task: pushes `Heartbeat` to the writer queue
        // every 5 s regardless of what the main loop is doing.
        // active_connections is reported as 0 here because the count lives in
        // `desk_session.rtc_peer_connection_map` which the main loop owns;
        // surfacing it to this task would require an Arc<AtomicU32> updated
        // at every map mutation. The daemon only logs the field at trace
        // level — its watchdog cares about IPC freshness, not the count.
        let heartbeat_task =
            spawn_heartbeat_task(writer_tx.clone(), tokio::time::Duration::from_secs(5));

        // Watch for the user-input desktop drifting away from the one we
        // were launched on (UAC, lock screen, etc.). The watcher emits one
        // notification per *transition* — repeated reads of the same
        // drifted state are suppressed inside the monitor so we don't
        // flood the IPC, and a return to the bound desktop re-arms it for
        // the next drift.
        let (desktop_change_tx, mut desktop_change_rx) = mpsc::unbounded_channel::<String>();
        desktop_monitor::spawn(init_payload.desktop_name.clone(), desktop_change_tx);

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
                                // Arch IV cut 4: media-control IPC. Routed
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
                                        // Cut 5: spin up per-connection input
                                        // handlers alongside the encoder so
                                        // mouse / keyboard input is ready as
                                        // soon as the browser opens its DCs.
                                        input_dispatcher.start_connection(&payload);
                                        // PR 4 cut 1: subscribe the connection
                                        // to clipboard sync; the dispatcher
                                        // starts its polling loop on the first
                                        // active connection.
                                        if let Some(d) = clipboard_dispatcher.as_ref() {
                                            d.start_connection(&payload).await;
                                        }
                                        // PR 4 cut 2: subscribe the connection
                                        // to file transfer commands.
                                        file_transfer_dispatcher.start_connection(&payload).await;
                                        // PR 4 cut 3: subscribe the connection
                                        // to whiteboard draw commands.
                                        whiteboard_dispatcher.start_connection(&payload).await;
                                        producer.start_media(payload);
                                    } else {
                                        warn!(
                                            "Worker received StartMedia but media producer is \
                                             not configured (no media_pipe_name in Init); ignoring"
                                        );
                                    }
                                }
                                ServiceToWorker::StopMedia(payload) => {
                                    if let Some(producer) = media_producer.as_ref() {
                                        producer.stop_media(&payload);
                                    }
                                    input_dispatcher.stop_connection(&payload);
                                    if let Some(d) = clipboard_dispatcher.as_ref() {
                                        d.stop_connection(&payload).await;
                                    }
                                    file_transfer_dispatcher.stop_connection(&payload).await;
                                    whiteboard_dispatcher.stop_connection(&payload).await;
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
                                // Cut 5: input IPC. The daemon already
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
                                // PR 4 cut 1: clipboard handlers route to
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
                                ServiceToWorker::FileTransferCommand(payload) => {
                                    file_transfer_dispatcher.handle_command(payload).await;
                                }
                                ServiceToWorker::WhiteboardCommand(payload) => {
                                    whiteboard_dispatcher.handle_command(payload).await;
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
                        // Arch IV: DeskSession in the worker no longer
                        // produces signaling text — the daemon owns the PC
                        // and writes directly to the browser. Drop instead
                        // of forwarding (the SignalingMessage IPC variant
                        // is gone in Arch IV / PR 7).
                        Some(DeskSessionMessage::Text(_text)) => {}
                        Some(DeskSessionMessage::Binary(_bin)) => {
                            warn!("DeskSession sent binary message, skipping IPC forward");
                        }
                        Some(DeskSessionMessage::Close) => {
                            info!("DeskSession requested close");
                            break;
                        }
                        Some(DeskSessionMessage::WebRTCDropped(connection_id)) => {
                            info!(
                                "WebRTC dropped for connection {}, shutting down peer connection",
                                connection_id
                            );
                            if let Some(peer_connection) =
                                desk_session.rtc_peer_connection_map.remove(&connection_id)
                            {
                                let peer_connection = peer_connection.read().await;
                                if let Err(e) = peer_connection.shutdown().await {
                                    error!("Failed to shutdown peer connection: {}", e);
                                }
                            }
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
/// directional in Arch IV (worker → daemon). The daemon does not push
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
}
