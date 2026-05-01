use crate::{
    ExternalChannels,
    host_control::HostControlHub,
    model::settings::{Args, Settings, SharedSettings, StartupMode},
    service::signaling::{DeskSession, DeskSessionMessage, DeskSessionSender},
};
use desk_ipc_protocol::{
    message::{HeartbeatPayload, ServiceToWorker, SignalingPayload, WorkerToService},
    transport::{read_message, write_message},
};

use actix_web::web;
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

        let mut channels = ExternalChannels {
            private_screen_cmd_sender: None,
            private_screen_state_receiver: None,
            tauri_login_token: None,
            whiteboard_cmd_sender: None,
            security_approval_sender: None,
            service_op_sender: None,
        };

        // Step 3 stop-gap: a Local hub means approvals deny-fast (no Tauri client
        // is ever connected to the worker's own ws endpoint). Step 5 swaps this for
        // a Forwarder hub that talks to the daemon's `/ws/host_upstream`.
        let host_control_hub = Arc::new(HostControlHub::new_local());

        let mut desk_session = DeskSession::new(
            shared_settings_data,
            session_sender,
            CurrentUser::new_admin("worker_node"),
            &mut channels,
            host_control_hub,
        )
        .await
        .map_err(|e| format!("Failed to create DeskSession: {}", e))?;

        info!("DeskSession created successfully, entering main loop");

        let heartbeat_interval = tokio::time::Duration::from_secs(5);
        let mut heartbeat_timer = tokio::time::interval(heartbeat_interval);
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
                            if let Err(e) = write_message(&mut writer, &payload).await {
                                error!("Failed to forward signaling to Service: {}", e);
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

                _ = heartbeat_timer.tick() => {
                    let heartbeat = WorkerToService::Heartbeat(HeartbeatPayload {
                        timestamp_ms: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                        active_connections: desk_session.rtc_peer_connection_map.len() as u32,
                        cpu_usage: None,
                        memory_usage: None,
                    });
                    if let Err(e) = write_message(&mut writer, &heartbeat).await {
                        warn!("Failed to send heartbeat: {}", e);
                        break;
                    }
                }
            }
        }

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
