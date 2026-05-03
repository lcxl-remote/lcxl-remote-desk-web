use super::worker_manager::{WorkerManager, WorkerMessageReceiver};
use crate::model::settings::{SharedSettings, StartupMode};
use actix_web::web;
use awc::{Client, Connector};
use desk_ipc_protocol::message::{ServiceToWorker, SignalingPayload, WorkerToService};
use desk_signal_facade::model::{
    signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
    version::VersionInfo,
};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

pub async fn run_signaling_proxy(
    settings: web::Data<SharedSettings>,
    worker_mgr: WorkerManager,
    mut worker_rx: WorkerMessageReceiver,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Signaling proxy starting");

    let (outbound_tx, _seed_rx) = broadcast::channel::<String>(128);

    let local_handle = {
        let settings = settings.clone();
        let worker_mgr = worker_mgr.clone();
        let outbound_tx = outbound_tx.clone();
        actix_web::rt::spawn(async move {
            loop {
                let (port, enable_ipv6, local_token, startup_mode) = {
                    let s = settings.read().await;
                    (
                        s.system.port,
                        s.system.enable_ipv6,
                        s.system.local_signaling_token.clone().unwrap_or_default(),
                        s.args.startup_mode.clone(),
                    )
                };

                // In Default mode connect to the embedded server port; in
                // ServiceDaemon mode connect to the daemon's own HTTP server.
                // All other modes have no local signaling endpoint.
                let effective_port = match startup_mode {
                    StartupMode::Default => port,
                    StartupMode::ServiceDaemon => crate::daemon::local_api::SERVICE_API_PORT,
                    _ => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        continue;
                    }
                };

                if local_token.is_empty() {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                // The local proxy should keep using loopback even when the daemon
                // API is exposed on all interfaces.
                let local_url = if enable_ipv6 && startup_mode != StartupMode::ServiceDaemon {
                    format!("ws://[::1]:{effective_port}/api/desk/signaling")
                } else {
                    format!("ws://127.0.0.1:{effective_port}/api/desk/signaling")
                };

                let rx = outbound_tx.subscribe();
                let _ = maintain_proxy_connection(
                    settings.clone(),
                    &worker_mgr,
                    local_url,
                    local_token,
                    rx,
                )
                .await;

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    let remote_sig_handle = {
        let settings = settings.clone();
        let worker_mgr = worker_mgr.clone();
        let outbound_tx = outbound_tx.clone();
        actix_web::rt::spawn(async move {
            loop {
                let (signaling_url, signaling_token) = {
                    let s = settings.read().await;
                    (
                        s.system.signaling_url.clone(),
                        s.system.signaling_token.clone(),
                    )
                };

                if let (Some(url), Some(token)) = (signaling_url, signaling_token)
                    && !url.is_empty()
                    && !token.is_empty()
                {
                    let rx = outbound_tx.subscribe();
                    let _ =
                        maintain_proxy_connection(settings.clone(), &worker_mgr, url, token, rx)
                            .await;
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    let remote_mgr_handle = {
        let settings = settings.clone();
        let worker_mgr = worker_mgr.clone();
        let outbound_tx = outbound_tx.clone();
        actix_web::rt::spawn(async move {
            loop {
                let (manager_url, manager_api_token) = {
                    let s = settings.read().await;
                    (
                        s.system.manager_url.clone(),
                        s.system.manager_api_token.clone(),
                    )
                };

                if let (Some(url), Some(token)) = (manager_url, manager_api_token)
                    && !url.is_empty()
                    && !token.is_empty()
                {
                    let rx = outbound_tx.subscribe();
                    let _ =
                        maintain_proxy_connection(settings.clone(), &worker_mgr, url, token, rx)
                            .await;
                }

                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        })
    };

    while let Some(msg) = worker_rx.recv().await {
        // Every IPC message — heartbeat, signaling, desktop change —
        // counts as a sign of life for the watchdog. Updating before
        // the match keeps the bookkeeping in one place and avoids the
        // watchdog firing on a worker that's actively talking but
        // hasn't happened to send a Heartbeat in the last interval.
        worker_mgr.note_heartbeat().await;

        match msg {
            WorkerToService::Ready => {
                info!("[SignalingProxy] Worker is Ready");
            }
            WorkerToService::SignalingMessage(payload) => {
                debug!(
                    "[SignalingProxy] Worker signaling response (len={})",
                    payload.message.len()
                );
                let _ = outbound_tx.send(payload.message);
            }
            WorkerToService::Heartbeat(hb) => {
                log::trace!(
                    "[SignalingProxy] Heartbeat: connections={}, ts={}",
                    hb.active_connections,
                    hb.timestamp_ms
                );
            }
            WorkerToService::DesktopReady => {
                info!("[SignalingProxy] Worker desktop ready after switch");
            }
            WorkerToService::DesktopChanged(payload) => {
                info!(
                    "[SignalingProxy] Worker reported desktop drift -> '{}'; restarting worker",
                    payload.name
                );
                let worker_mgr = worker_mgr.clone();
                let new_desktop = payload.name.clone();
                // Run the switch on a separate task so the message loop
                // keeps draining (notify_desktop_switch awaits worker
                // mailbox flushes; bridge_loop pushes more messages while
                // we wait).
                actix_web::rt::spawn(async move {
                    let preapproved = worker_mgr.notify_desktop_switch().await;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let session_id = crate::daemon::session_monitor::get_active_session_id();
                    if let Err(e) = worker_mgr
                        .start_worker(session_id, Some(new_desktop.clone()), preapproved)
                        .await
                    {
                        error!(
                            "[SignalingProxy] Failed to start worker for desktop '{}': {}",
                            new_desktop, e
                        );
                    }
                });
            }
            WorkerToService::ConnectionAcceptStateChanged {
                connection_id,
                state,
            } => {
                debug!(
                    "[SignalingProxy] Worker reported accept-state for {connection_id}: \
                     control={} clipboard={}",
                    state.accept_control, state.accept_clipboard_sync
                );
                worker_mgr.update_connection_accept(&connection_id, state);
            }
            WorkerToService::ConnectionClosed { connection_id } => {
                debug!("[SignalingProxy] Worker reported connection closed: {connection_id}");
                worker_mgr.remove_connection(&connection_id);
            }
            WorkerToService::Error(err) => {
                error!(
                    "[SignalingProxy] Worker error: code={}, msg={}, recoverable={}",
                    err.code, err.message, err.recoverable
                );
            }
        }
    }

    local_handle.abort();
    remote_sig_handle.abort();
    remote_mgr_handle.abort();

    info!("Signaling proxy stopped");
    Ok(())
}

async fn maintain_proxy_connection(
    settings: web::Data<SharedSettings>,
    worker_mgr: &WorkerManager,
    signaling_url: String,
    auth_token: String,
    mut outbound_rx: broadcast::Receiver<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let display_name = {
        let s = settings.read().await;
        s.desk.display_name.clone()
    };
    let display_name = display_name.or_else(sysinfo::System::host_name);

    let client_id = {
        let s = settings.read().await;
        s.system.get_client_id().map_err(|e| format!("{e}"))?
    };

    let mut version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        crate::version::SERVER_BUILD_NUMBER,
        crate::version::SERVER_COMMIT_HASH.to_string(),
        RemoteDeskTypeEnum::Server,
        display_name,
        Some(client_id),
    );
    version_info.token = Some(auth_token);
    let version_query = serde_urlencoded::to_string(&version_info)
        .map_err(|e| format!("Failed to encode version info: {e}"))?;

    let mut root_store = RootCertStore::empty();
    for cert in load_native_certs().expect("could not load platform certs") {
        root_store.add(cert).unwrap();
    }
    let client = Client::builder()
        .connector(
            Connector::new()
                .timeout(Duration::from_secs(10))
                .rustls_0_23(Arc::new(
                    ClientConfig::builder()
                        .with_root_certificates(Arc::new(root_store))
                        .with_no_client_auth(),
                )),
        )
        .finish();

    let url_clean = signaling_url.trim().trim_matches(|c: char| c.is_control());
    let connect_url = if url_clean.contains('?') {
        format!("{url_clean}&{version_query}")
    } else {
        format!("{url_clean}?{version_query}")
    };

    info!("[Proxy] Connecting to: {signaling_url}");
    debug!("[Proxy] Full URL: {connect_url}");

    let (_resp, framed) = client
        .ws(&connect_url)
        .connect()
        .await
        .map_err(|e| format!("WebSocket connect failed: {e:?}"))?;

    info!("[Proxy] Connected to {signaling_url}");

    let (mut sink, mut stream) = framed.split();

    loop {
        tokio::select! {
            ws_msg = stream.next() => {
                match ws_msg {
                    Some(Ok(frame)) => {
                        match frame {
                            awc::ws::Frame::Text(text) => {
                                let text_str = match std::str::from_utf8(&text) {
                                    Ok(s) => s.to_string(),
                                    Err(e) => {
                                        error!("[Proxy] Invalid UTF-8 from WS: {e}");
                                        continue;
                                    }
                                };
                                if let Ok(parsed) =
                                    serde_json::from_str::<SignalingModel>(&text_str)
                                    && matches!(
                                        parsed.signaling_type,
                                        SignalingType::RequestRemote
                                    )
                                        && let Some(from_id) = parsed.from_connection_id {
                                            worker_mgr.track_browser_connection(from_id);
                                        }
                                let msg = ServiceToWorker::SignalingMessage(SignalingPayload {
                                    message: text_str,
                                    connection_id: None,
                                });
                                if let Err(e) = worker_mgr.send_to_worker(msg).await {
                                    warn!("[Proxy] Failed to forward to worker: {e}");
                                    break;
                                }
                            }
                            awc::ws::Frame::Ping(data) => {
                                let _ = sink.send(awc::ws::Message::Pong(data)).await;
                            }
                            awc::ws::Frame::Close(reason) => {
                                warn!("[Proxy] WS close frame: {reason:?}");
                                break;
                            }
                            _ => {}
                        }
                    }
                    Some(Err(e)) => {
                        error!("[Proxy] WS error: {e}");
                        break;
                    }
                    None => {
                        warn!("[Proxy] WS stream closed");
                        break;
                    }
                }
            }

            outbound = outbound_rx.recv() => {
                match outbound {
                    Ok(msg) => {
                        if let Err(e) = sink.send(awc::ws::Message::Text(msg.into())).await {
                            error!("[Proxy] WS send error: {e}");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("[Proxy] Outbound channel lagged by {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("[Proxy] Outbound broadcast channel closed");
                        break;
                    }
                }
            }
        }
    }

    info!("[Proxy] Connection to {signaling_url} ended");
    Ok(())
}
