//! Lifecycle of one outbound signaling-proxy connection.

use super::*;

pub(super) async fn maintain_proxy_connection(
    settings: web::Data<SharedSettings>,
    router_ctx: &RouterContext,
    signaling_url: String,
    auth_token: String,
    mut outbound_rx: broadcast::Receiver<String>,
    source: InboundSignalingSource,
    // True only on the manager link (`remote_mgr_handle`); a fatal device-quota
    // `Error` then stops auto-reconnect. Never set for the local loopback (which is
    // also `TrustedCentral` in Default mode) or a bare remote-signaling relay.
    fatal_quota_reject_enabled: bool,
    // Records the fatal rejection for the host UI when the link is the manager link.
    manager_link_state: Option<Arc<ManagerLinkState>>,
    // Set only for the manager and support upstreams. When present, the current
    // connection is torn down the moment the shared `ManagerLinkGate` flips to
    // `false` (the host disabling the manager connection at runtime). `None` for
    // the local loopback and bare remote-signaling relays, which the manager
    // toggle does not govern.
    mut manager_link_enabled_rx: Option<watch::Receiver<bool>>,
    // Candidate kind used to elect exactly one authoritative mirror dynamically:
    // manager > configured remote signal > embedded local signal.
    remote_access_central_link: RemoteAccessCentralLink,
) -> Result<ProxyConnectionOutcome, Box<dyn std::error::Error>> {
    let display_name = {
        let s = settings.read().await;
        s.desk.display_name.clone()
    };
    let display_name = display_name.or_else(sysinfo::System::host_name);

    let client_id = {
        let s = settings.read().await;
        s.system.get_client_id().map_err(|e| format!("{e}"))?
    };

    // Whether the host refuses a plaintext dial to a *public* signaling / manager
    // address. Loopback / private / LAN targets (the local loopback link, a
    // self-hosted server on a LAN) stay reachable over plaintext regardless; only
    // an internet-routable plaintext target is refused when this is on.
    let require_secure_signaling = {
        let s = settings.read().await;
        s.system.require_secure_signaling
    };

    // Every upstream link registers as a normal `Server` connection. Temporary
    // support no longer opens a dedicated restricted upstream: a support code is
    // requested over this same `Server` link and redeemed into a capability-scoped
    // grant, so the restriction is enforced per session rather than per link.
    let remote_desk_type = RemoteDeskTypeEnum::Server;

    let mut version_info = VersionInfo::new(
        desk_server_version::SERVER_API_VERSION,
        crate::version::SERVER_BUILD_NUMBER,
        crate::version::SERVER_COMMIT_HASH.to_string(),
        remote_desk_type,
        display_name,
        Some(client_id),
    );
    version_info.token = Some(auth_token);
    version_info.set_available_exec_shells(&crate::exec_shells::available_exec_shells());
    let advertised_ai_command_runtime_ms = {
        let settings = settings.read().await;
        settings
            .ai_policy
            .max_command_runtime_seconds
            .saturating_mul(1_000)
    };
    version_info.max_ai_command_runtime_ms = Some(advertised_ai_command_runtime_ms);
    log::info!(
        "[agent-exec] verified available shells: {:?}",
        version_info.available_exec_shell_list()
    );
    if !crate::version::SERVER_REPOSITORY_URL.is_empty() {
        version_info.repository_url = Some(crate::version::SERVER_REPOSITORY_URL.to_string());
    }
    let version_query = serde_urlencoded::to_string(&version_info)
        .map_err(|e| format!("Failed to encode version info: {e}"))?;

    let mut root_store = RootCertStore::empty();
    for cert in load_native_certs().expect("could not load platform certs") {
        root_store.add(cert).unwrap();
    }
    // Guard the outbound dial at connect time: the metadata floor is always
    // blocked, and a plaintext (`ws://`) scheme to a public address is refused when
    // `require_secure_signaling` is on. The scheme is fixed for this dial, so bake
    // it into the resolver — no second lookup that could rebind. `allow_private` is
    // always true here: signaling legitimately reaches LAN / loopback targets.
    // Normalize the URL exactly as the dial does and guard the (possibly literal)
    // target before connecting. Returns the cleaned URL that will be dialed.
    let url_clean = guard_and_clean_signaling_url(&signaling_url, require_secure_signaling)?;
    let scheme_is_tls = signaling_scheme_is_tls(&url_clean);
    let guard = crate::transport_guard::TransportGuardResolver::system(
        crate::transport_guard::TransportPolicy {
            allow_private: true,
            scheme_is_tls,
            enforce_public_tls: require_secure_signaling,
        },
    );
    let tcp =
        actix_tls::connect::Connector::new(actix_tls::connect::Resolver::custom(guard)).service();
    let client = Client::builder()
        .connector(
            Connector::new()
                .connector(tcp)
                .timeout(Duration::from_secs(10))
                .rustls_0_23(Arc::new(
                    ClientConfig::builder()
                        .with_root_certificates(Arc::new(root_store))
                        .with_no_client_auth(),
                )),
        )
        .finish();

    let connect_url = if url_clean.contains('?') {
        format!("{url_clean}&{version_query}")
    } else {
        format!("{url_clean}?{version_query}")
    };

    info!(
        "[Proxy] Connecting to: {}",
        redact_token_in_url(&signaling_url)
    );
    debug!("[Proxy] Full URL: {}", redact_token_in_url(&connect_url));

    let (_resp, framed) = client
        .ws(&connect_url)
        .connect()
        .await
        .map_err(|e| format!("WebSocket connect failed: {e:?}"))?;

    info!(
        "[Proxy] Connected to {}",
        redact_token_in_url(&signaling_url)
    );

    // A successful (re)connection clears any prior fatal rejection so the host UI
    // stops showing the blocked state once registration goes through.
    if let Some(state) = manager_link_state.as_ref() {
        state.clear().await;
    }

    let (mut sink, mut stream) = framed.split();
    let mut remote_access_reconcile = tokio::time::interval(Duration::from_secs(2));
    remote_access_reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut agent_capability_reconcile = tokio::time::interval(Duration::from_secs(2));
    agent_capability_reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut remote_access_commands = if remote_access_central_link != RemoteAccessCentralLink::None
    {
        router_ctx
            .host_control_hub
            .remote_access_coordinator()
            .map(|coordinator| coordinator.subscribe_central_commands())
    } else {
        None
    };

    // Close a race where the manager link is disabled after `connect()` but
    // before this read loop parks on the gate: read the current value first and
    // bail out immediately if the link should no longer be up.
    if let Some(rx) = manager_link_enabled_rx.as_ref()
        && !*rx.borrow()
    {
        info!(
            "[Proxy] Manager link disabled; closing {}",
            redact_token_in_url(&signaling_url)
        );
        let _ = sink.send(awc::ws::Message::Close(None)).await;
        return Ok(ProxyConnectionOutcome::Closed);
    }

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
                                if remote_access_link_is_primary(
                                    &settings,
                                    remote_access_central_link,
                                )
                                .await
                                    && consume_remote_access_ack(
                                        &text_str,
                                        &router_ctx.host_control_hub,
                                    )
                                    .await
                                {
                                    continue;
                                }
                                match handle_inbound_signaling_text(
                                    text_str,
                                    router_ctx,
                                    source,
                                    fatal_quota_reject_enabled,
                                )
                                .await
                                {
                                    InboundOutcome::Continue => {}
                                    InboundOutcome::FatalReject { error_code, message } => {
                                        warn!(
                                            "[Proxy] Manager rejected registration (code {error_code}): \
                                             {message}; stopping auto-reconnect until manual retry"
                                        );
                                        if let Some(state) = manager_link_state.as_ref() {
                                            state.record_fatal(error_code, message.clone()).await;
                                        }
                                        let _ = sink.send(awc::ws::Message::Close(None)).await;
                                        return Ok(ProxyConnectionOutcome::FatalReject {
                                            error_code,
                                            message,
                                        });
                                    }
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

            _ = remote_access_reconcile.tick(), if remote_access_central_link != RemoteAccessCentralLink::None => {
                if remote_access_link_is_primary(&settings, remote_access_central_link).await
                    && let Some(frame) = pending_remote_access_frame(&router_ctx.host_control_hub)
                    && let Err(error) = sink.send(awc::ws::Message::Text(frame.into())).await
                {
                    error!("[remote-access] central mirror send failed: {error}");
                    break;
                }
            }

            command = receive_remote_access_command(&mut remote_access_commands), if remote_access_central_link != RemoteAccessCentralLink::None => {
                if remote_access_link_is_primary(&settings, remote_access_central_link).await
                    && let Some(command) = command
                    && let Err(error) = sink.send(awc::ws::Message::Text(command.into())).await
                {
                    error!("[remote-access] peer eviction send failed: {error}");
                    break;
                }
            }

            // The runtime ceiling is registration metadata used by the central
            // model schema and plan sealer. Reconnect all upstreams when the
            // locally authoritative value changes so a newly started diagnosis
            // observes the saved setting instead of a stale connection-time cap.
            _ = agent_capability_reconcile.tick() => {
                let current = {
                    let settings = settings.read().await;
                    settings
                        .ai_policy
                        .max_command_runtime_seconds
                        .saturating_mul(1_000)
                };
                if current != advertised_ai_command_runtime_ms {
                    info!(
                        "[agent-exec] command runtime ceiling changed from {}ms to {}ms; reconnecting {}",
                        advertised_ai_command_runtime_ms,
                        current,
                        redact_token_in_url(&signaling_url)
                    );
                    let _ = sink.send(awc::ws::Message::Close(None)).await;
                    break;
                }
            }

            // Manager / support upstreams only: tear the connection down when the
            // host disables the manager link at runtime. `None` links resolve a
            // never-completing future, so this branch is inert for them.
            _ = wait_manager_link_disabled(&mut manager_link_enabled_rx) => {
                info!(
            "[Proxy] Manager link disabled; closing {}",
            redact_token_in_url(&signaling_url)
        );
                let _ = sink.send(awc::ws::Message::Close(None)).await;
                break;
            }
        }
    }

    info!(
        "[Proxy] Connection to {} ended",
        redact_token_in_url(&signaling_url)
    );
    Ok(ProxyConnectionOutcome::Closed)
}
