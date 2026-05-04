use super::pc_manager::PcRegistry;
use super::signaling_router::{self, RouteOutcome, RouterContext};
use super::worker_manager::{WorkerManager, WorkerMessageReceiver};
use crate::host_control::HostControlHub;
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
    host_control_hub: Arc<HostControlHub>,
    mut worker_rx: WorkerMessageReceiver,
    pc_registry: PcRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Signaling proxy starting");

    let (outbound_tx, _seed_rx) = broadcast::channel::<String>(128);

    // The daemon constructs `pc_registry` once in `daemon::mod` and shares
    // it with both `WorkerManager` (for the media-pipe receiver) and the
    // signaling proxy (for inbound SDP/ICE handlers). Using a single
    // registry across all signaling endpoints (local / remote signaling /
    // remote manager) means the same PC handles inbound messages
    // regardless of which WS surfaced them.
    let router_ctx = RouterContext {
        pc_registry: pc_registry.clone(),
        outbound_tx: outbound_tx.clone(),
        settings: settings.clone(),
        host_control_hub: host_control_hub.clone(),
        worker_mgr: worker_mgr.clone(),
    };

    let local_handle = {
        let settings = settings.clone();
        let worker_mgr = worker_mgr.clone();
        let outbound_tx = outbound_tx.clone();
        let router_ctx = router_ctx.clone();
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
                    &router_ctx,
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
        let router_ctx = router_ctx.clone();
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
                    let _ = maintain_proxy_connection(
                        settings.clone(),
                        &worker_mgr,
                        &router_ctx,
                        url,
                        token,
                        rx,
                    )
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
        let router_ctx = router_ctx.clone();
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
                    let _ = maintain_proxy_connection(
                        settings.clone(),
                        &worker_mgr,
                        &router_ctx,
                        url,
                        token,
                        rx,
                    )
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
            WorkerToService::Capabilities(caps) => {
                info!(
                    "[SignalingProxy] Worker reported capabilities: video={:?}, audio={:?}, \
                     desktop={:?}, has_tauri={}, is_admin={}",
                    caps.video_codecs,
                    caps.audio_codecs,
                    caps.desktop_name,
                    caps.has_tauri,
                    caps.is_admin,
                );
                worker_mgr.set_worker_capabilities(caps);
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
            // PR 3 cursor sync: worker emits CursorData when its
            // capture loop sees a cursor shape / position update;
            // daemon writes the JSON bytes to the matching browser's
            // `cursor_sync_event` DC. Lookup happens in
            // `pc_manager::write_cursor_data` (silent-drop on
            // unknown connection / no DC).
            WorkerToService::CursorData(payload) => {
                crate::daemon::pc_manager::write_cursor_data(&pc_registry, payload).await;
            }
            // Arch IV variants — daemon's PR 2 event-pipe handler will
            // own these. The current Arch III signaling_proxy never sees
            // them because no Arch III worker emits them.
            other => {
                debug!("[SignalingProxy] Ignoring Arch IV variant in Arch III proxy: {other:?}");
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
    router_ctx: &RouterContext,
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
                                handle_inbound_signaling_text(
                                    text_str,
                                    &worker_mgr,
                                    router_ctx,
                                )
                                .await;
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

/// Inbound-text dispatcher pulled out of `maintain_proxy_connection`
/// so the parse / route / forward sequence is reusable for tests and
/// the per-frame logic stays out of the WS select loop.
///
/// Cut 3c hygiene:
///
/// - Parse the inbound text **once** (the WS loop used to parse twice
///   — a partial parse for `RequestRemote` tracking, then the router
///   parsed a second time inside `route()`).
/// - Reject worker-bound types missing `from_connection_id`. Until cut
///   3c every legacy-forwarded message carried `connection_id: None`
///   and the worker had to re-parse the JSON to find out who it came
///   from; now the daemon refuses to forward something the worker
///   would not be able to dispatch on, and surfaces the rejection to
///   the operator via a warn-level log.
/// - When forwarding, populate `SignalingPayload.connection_id` from
///   `from_connection_id` so the worker can dispatch without a
///   re-parse.
async fn handle_inbound_signaling_text(
    text_str: String,
    worker_mgr: &WorkerManager,
    router_ctx: &RouterContext,
) {
    let parsed = match serde_json::from_str::<SignalingModel>(&text_str) {
        Ok(m) => m,
        Err(e) => {
            warn!("[Proxy] Dropping malformed signaling text: {e}");
            return;
        }
    };

    if matches!(parsed.signaling_type, SignalingType::RequestRemote)
        && let Some(from_id) = parsed.from_connection_id.as_ref()
    {
        worker_mgr.track_browser_connection(from_id.clone());
    }

    let outcome = match signaling_router::route(&parsed, router_ctx).await {
        Ok(o) => o,
        Err(e) => {
            warn!(
                "[Proxy] router handler failed for {:?}: {e}; falling back to worker forward",
                parsed.signaling_type,
            );
            RouteOutcome::ForwardToWorker
        }
    };

    if outcome == RouteOutcome::HandledByDaemon {
        return;
    }

    let from_connection_id = match parsed.from_connection_id.as_ref() {
        Some(id) => id.clone(),
        None => {
            warn!(
                "[Proxy] Dropping worker-bound signaling without from_connection_id: {:?} \
                 (cut 3c contract: worker-routed types must carry the connection id so \
                 the worker can dispatch without re-parsing the JSON)",
                parsed.signaling_type,
            );
            return;
        }
    };

    let msg = ServiceToWorker::SignalingMessage(SignalingPayload {
        message: text_str,
        connection_id: Some(from_connection_id),
    });
    if let Err(e) = worker_mgr.send_to_worker(msg).await {
        warn!("[Proxy] Failed to forward to worker: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::pc_manager::PcRegistry;
    use crate::host_control::HostControlHub;
    use crate::model::settings::{Settings, SharedSettings};
    use desk_signal_facade::model::signal::{SignalingModel, SignalingType};

    fn make_router_ctx_and_mgr() -> (RouterContext, broadcast::Sender<String>, WorkerManager) {
        let (outbound_tx, _) = broadcast::channel::<String>(16);
        let shared = SharedSettings::from(Settings::default());
        let settings = web::Data::new(shared);
        let pc_registry = PcRegistry::new();
        let (worker_mgr, _rx) = WorkerManager::new(settings.clone(), pc_registry.clone());
        let ctx = RouterContext {
            pc_registry,
            outbound_tx: outbound_tx.clone(),
            settings,
            host_control_hub: Arc::new(HostControlHub::new_local()),
            worker_mgr: worker_mgr.clone(),
        };
        (ctx, outbound_tx, worker_mgr)
    }

    /// Worker-bound signaling without `from_connection_id` is dropped
    /// at the daemon (rather than forwarded as `connection_id: None`
    /// like Arch III did) — the worker would not be able to dispatch
    /// without a re-parse otherwise.
    #[tokio::test]
    async fn drops_worker_bound_message_without_from_connection_id() {
        let (router_ctx, _out_tx, worker_mgr) = make_router_ctx_and_mgr();

        // RequireControl is worker-owned (router returns ForwardToWorker)
        let model = SignalingModel::new(
            "req-1",
            SignalingType::RequireControl,
            None, // missing from_connection_id
            None,
            None,
            None,
        );
        let text = serde_json::to_string(&model).unwrap();

        // Should NOT panic, should NOT forward (no worker exists).
        // Just verifies the function returns cleanly.
        handle_inbound_signaling_text(text, &worker_mgr, &router_ctx).await;
    }

    /// Malformed JSON arriving on the WS is dropped with a warning
    /// rather than crashing the proxy loop (cut 3c collapses the
    /// previous lossy `from_str(...).ok()` two-step into a single
    /// validated parse).
    #[tokio::test]
    async fn drops_malformed_json() {
        let (router_ctx, _out_tx, worker_mgr) = make_router_ctx_and_mgr();

        handle_inbound_signaling_text(
            "{ this is not valid json".to_string(),
            &worker_mgr,
            &router_ctx,
        )
        .await;
    }

    /// Daemon-owned RequestRemote without `from_connection_id` does
    /// not crash the dispatcher — the router's `handle_request_remote`
    /// returns the per-handler error which we already log; the
    /// dispatcher should not promote that into a forward attempt.
    #[tokio::test]
    async fn handles_router_error_without_forwarding() {
        let (router_ctx, _out_tx, worker_mgr) = make_router_ctx_and_mgr();

        let model = SignalingModel::new(
            "req-2",
            SignalingType::RequestRemote,
            None, // missing from_connection_id triggers handler error
            None,
            None,
            None,
        );
        let text = serde_json::to_string(&model).unwrap();

        // Router returns Err -> dispatcher logs and falls through to
        // the worker-forward path; that path then drops because
        // from_connection_id is missing. Net: no panic, no forward.
        handle_inbound_signaling_text(text, &worker_mgr, &router_ctx).await;
    }
}
