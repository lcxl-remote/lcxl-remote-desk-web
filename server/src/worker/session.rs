use crate::{
    host_control::{HostControlHub, UpstreamForwarder, upstream::spawn_upstream_ws_task},
    model::settings::{Args, Settings, SharedSettings, StartupMode},
    service::signaling::{DeskSession, DeskSessionMessage, DeskSessionSender},
    worker::desktop_monitor,
};
use actix_web::web;
use desk_ipc_protocol::{
    message::{
        DesktopChangedPayload, HeartbeatPayload, ServiceToWorker, SignalingPayload,
        WorkerInitPayload, WorkerToService,
    },
    transport::{read_message, write_message},
};
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::signal::SignalingModel;
use log::{error, info, warn};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
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

pub struct WorkerSession {
    args: Args,
}

impl WorkerSession {
    pub async fn run(args: Args, pipe_name: &str) -> Result<(), Box<dyn std::error::Error>> {
        let session = WorkerSession { args };
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
                write_message(&mut writer, &err_msg).await?;
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

        // Build the host-control hub. When the daemon supplied a host_upstream_url
        // we run as a Forwarder and bridge approval / private-screen / whiteboard
        // traffic over ws to the daemon's aggregator. Without an upstream URL
        // (standalone or test runs) fall back to a Local hub — its approvals
        // deny-fast because nothing connects to the worker's own ws endpoint.
        let (host_control_hub, upstream_spec) = build_hub_from_init(&init_payload);
        match upstream_spec {
            Some((upstream, url, token)) => {
                spawn_upstream_ws_task(upstream, url, token);
            }
            None => {
                warn!(
                    "Init payload missing host_upstream_url; falling back to Local hub \
                     (approvals will deny-fast)."
                );
            }
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

        let (service_msg_tx, mut service_msg_rx) =
            mpsc::unbounded_channel::<io::Result<ServiceToWorker>>();

        tokio::spawn(async move {
            loop {
                let result = read_message::<_, ServiceToWorker>(&mut reader).await;
                let should_stop = result.is_err();
                if service_msg_tx.send(result).is_err() || should_stop {
                    break;
                }
            }
        });

        // Outbound IPC: a dedicated writer task owns `writer` and drains an
        // unbounded mpsc. Decoupling the writer from the main `select!` loop
        // means heartbeats (and other queued messages) keep flowing even when
        // the main loop is blocked awaiting a long-running handler — e.g.
        // `request_approval` waiting for the user to click the Tauri dialog.
        // Without this split the heartbeat-timer arm of `select!` would never
        // be polled while a handler `await`ed, causing the daemon's watchdog
        // to declare the worker stuck and kill it after 30 s.
        let (writer_tx, writer_rx) = mpsc::unbounded_channel::<WorkerToService>();
        let writer_task = spawn_ipc_writer_task(writer, writer_rx);

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
                        Some(Ok(msg)) => {
                            match msg {
                                ServiceToWorker::SignalingMessage(payload) => {
                                    info!("Worker received SignalingMessage: {}", payload.message);
                                    match serde_json::from_str::<SignalingModel>(&payload.message) {
                                        Ok(signaling_model) => {
                                            if let Err(e) = desk_session.handle_message(&signaling_model).await {
                                                warn!(
                                                    "DeskSession handle_message error: {}, type={}, request_id={}",
                                                    e, signaling_model.signaling_type, signaling_model.request_id
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            error!("Failed to parse signaling message: {}", e);
                                        }
                                    }
                                }
                                ServiceToWorker::DesktopSwitching => {
                                    info!("Desktop switching - preparing for shutdown");
                                    if let Err(e) = desk_session.shutdown().await {
                                        error!("DeskSession shutdown error: {}", e);
                                    }
                                    break;
                                }
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
                            }
                        }
                        Some(Err(e)) => {
                            if e.kind() == io::ErrorKind::UnexpectedEof {
                                info!("IPC connection closed by Service");
                            } else {
                                error!("IPC read error: {}", e);
                            }
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
                            let payload = WorkerToService::SignalingMessage(SignalingPayload {
                                message: text.to_string(),
                                connection_id: None,
                            });
                            if writer_tx.send(payload).is_err() {
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
        // pushing into writer_tx, then drop our own writer_tx so the writer
        // task observes "all senders gone" and drains + exits cleanly.
        heartbeat_task.abort();
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

/// Spawn a task that owns `writer` and drains `rx`, writing each message to
/// the IPC. Decoupled from the main `select!` so a long-running handler can't
/// block outbound traffic. The task exits when all senders are dropped or a
/// write fails.
fn spawn_ipc_writer_task<W>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<WorkerToService>,
) -> tokio::task::JoinHandle<()>
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = write_message(&mut writer, &msg).await {
                warn!("Failed to write IPC message: {}", e);
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

    /// Writer task drains the queue in order and exits when all senders are
    /// dropped. Uses an in-memory duplex stream so we can read back the bytes.
    #[tokio::test]
    async fn writer_task_drains_queue_and_exits_when_senders_dropped() {
        let (server_side, mut client_side) = tokio::io::duplex(64 * 1024);
        let (tx, rx) = mpsc::unbounded_channel::<WorkerToService>();
        let task = spawn_ipc_writer_task(server_side, rx);

        tx.send(WorkerToService::Ready).expect("send Ready");
        tx.send(WorkerToService::DesktopReady)
            .expect("send DesktopReady");
        drop(tx);

        // Both messages must have been written and decodable in order.
        let m1: WorkerToService = read_message(&mut client_side)
            .await
            .expect("read first message");
        assert!(matches!(m1, WorkerToService::Ready));
        let m2: WorkerToService = read_message(&mut client_side)
            .await
            .expect("read second message");
        assert!(matches!(m2, WorkerToService::DesktopReady));

        tokio::time::timeout(tokio::time::Duration::from_secs(1), task)
            .await
            .expect("writer task must exit after senders drop")
            .expect("task panicked");
    }
}
