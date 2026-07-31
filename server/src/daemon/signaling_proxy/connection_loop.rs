//! Lifecycle of one outbound signaling-proxy connection.

use super::*;

struct OutstandingHeartbeat {
    request_id: String,
    deadline: tokio::time::Instant,
}

pub(super) async fn teardown_manager_members(
    router_ctx: &RouterContext,
    members: &[String],
    reason: &str,
) {
    for connection_id in members {
        if let Some(coordinator) = router_ctx.host_control_hub.remote_access_coordinator() {
            if let Err(error) = coordinator.disconnect_connection(connection_id).await {
                warn!(
                    "[credential-proof] could not disconnect {connection_id} through coordinator: \
                     {error}"
                );
            }
        } else {
            crate::daemon::pc_manager::force_disconnect_connection(
                &router_ctx.pc_registry,
                &router_ctx.worker_mgr,
                router_ctx.virtual_display.as_ref(),
                connection_id,
                reason,
            )
            .await;
            router_ctx
                .host_control_hub
                .cancel_pending_for_connection(connection_id);
        }
    }
}

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
    credential_scopes: &crate::daemon::manager_credential_scope::ManagerCredentialScopeRegistry,
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
    version_info.token = Some(auth_token.clone());
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

    let manager_credential_link = if remote_access_central_link == RemoteAccessCentralLink::Manager
    {
        Some(credential_scopes.begin_link(&auth_token).await)
    } else {
        None
    };
    let mut credential_expiry_rx = credential_scopes.subscribe_expirations();
    let effective_router_ctx = RouterContext {
        admission_origin: match remote_access_central_link {
            RemoteAccessCentralLink::Manager => {
                crate::daemon::pc_manager::AdmissionOrigin::Manager(
                    manager_credential_link
                        .as_ref()
                        .expect("manager link scope")
                        .fingerprint(),
                )
            }
            RemoteAccessCentralLink::RemoteSignal => {
                crate::daemon::pc_manager::AdmissionOrigin::RemoteSignal
            }
            RemoteAccessCentralLink::Local | RemoteAccessCentralLink::None => {
                crate::daemon::pc_manager::AdmissionOrigin::Local
            }
        },
        manager_credential_link: manager_credential_link.clone(),
        ..router_ctx.clone()
    };
    let router_ctx = &effective_router_ctx;

    // A successful (re)connection clears any prior fatal rejection so the host UI
    // stops showing the blocked state once registration goes through.
    if let Some(state) = manager_link_state.as_ref() {
        state.clear().await;
    }

    let (mut sink, mut stream) = framed.split();
    let mut heartbeat_tick = tokio::time::interval(Duration::from_secs(30));
    heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut outstanding_heartbeat: Option<OutstandingHeartbeat> = None;
    let mut accelerated_remaining = 0_u8;
    let mut accelerated_at: Option<tokio::time::Instant> = None;
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

    let mut credential_expired = false;
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
                                if let Some(link) = manager_credential_link.as_ref()
                                    && let Ok(model) =
                                        serde_json::from_str::<SignalingModel>(&text_str)
                                    && outstanding_heartbeat
                                        .as_ref()
                                        .is_some_and(|heartbeat| {
                                            heartbeat.request_id == model.request_id
                                        })
                                {
                                    if model.signaling_type == SignalingType::Heartbeat
                                        && model
                                            .response_state
                                            .as_ref()
                                            .is_some_and(SignalingResponseState::is_success)
                                    {
                                        outstanding_heartbeat = None;
                                        let proof = model
                                            .get_data_with_type::<
                                                desk_signal_facade::model::credential_heartbeat::ManagerCredentialHeartbeatProof,
                                            >()
                                            .ok()
                                            .flatten();
                                        if proof.is_some_and(|proof| proof.is_supported())
                                            && link.accept_proof().await
                                        {
                                            accelerated_remaining = 0;
                                            accelerated_at = None;
                                        } else {
                                            link.proof_unavailable().await;
                                            if accelerated_remaining == 0 {
                                                accelerated_remaining = 3;
                                                accelerated_at = Some(
                                                    tokio::time::Instant::now()
                                                        + accelerated_probe_phase(
                                                            rand::random::<u64>(),
                                                        ),
                                                );
                                            } else if accelerated_remaining > 0 {
                                                accelerated_at = Some(
                                                    tokio::time::Instant::now()
                                                        + Duration::from_secs(10),
                                                );
                                            }
                                        }
                                        continue;
                                    }
                                    if model.signaling_type == SignalingType::Error
                                        && let Some(response) = model.response_state.as_ref()
                                        && (response.error_code
                                            == DeskErrorCode::MANAGER_CREDENTIAL_REVOKED.code()
                                            || response.error_code
                                                == DeskErrorCode::MANAGER_CREDENTIAL_SUSPENDED.code())
                                    {
                                        let suspended = response.error_code
                                            == DeskErrorCode::MANAGER_CREDENTIAL_SUSPENDED.code();
                                        let scope_state = if suspended {
                                            crate::daemon::manager_credential_scope::CredentialScopeState::Suspended
                                        } else {
                                            crate::daemon::manager_credential_scope::CredentialScopeState::Revoked
                                        };
                                        let members = link.invalidate(scope_state).await;
                                        teardown_manager_members(
                                            router_ctx,
                                            &members,
                                            if suspended {
                                                "manager-credential-suspended"
                                            } else {
                                                "manager-credential-revoked"
                                            },
                                        )
                                        .await;
                                        let message = response.message.clone().unwrap_or_else(|| {
                                            if suspended {
                                                "Manager credential is temporarily suspended"
                                            } else {
                                                "Manager credential is no longer valid"
                                            }
                                            .to_string()
                                        });
                                        if let Some(state) = manager_link_state.as_ref() {
                                            state
                                                .record_fatal(response.error_code, message.clone())
                                                .await;
                                        }
                                        let _ = sink.send(awc::ws::Message::Close(None)).await;
                                        return if suspended {
                                            Ok(ProxyConnectionOutcome::CredentialSuspended {
                                                error_code: response.error_code,
                                                message,
                                            })
                                        } else {
                                            Ok(ProxyConnectionOutcome::FatalReject {
                                                error_code: response.error_code,
                                                message,
                                            })
                                        };
                                    }
                                }
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

            _ = heartbeat_tick.tick(), if manager_credential_link.is_some()
                && outstanding_heartbeat.is_none()
                && accelerated_remaining == 0 => {
                let heartbeat = SignalingModel::new_request::<()>(
                    SignalingType::Heartbeat,
                    None,
                    None,
                )
                .map_err(|error| format!("could not build manager heartbeat: {error}"))?;
                let request_id = heartbeat.request_id.clone();
                sink.send(awc::ws::Message::Text(
                    serde_json::to_string(&heartbeat)?.into(),
                ))
                .await?;
                outstanding_heartbeat = Some(OutstandingHeartbeat {
                    request_id,
                    deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                });
            }

            _ = async {
                match accelerated_at {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            }, if manager_credential_link.is_some()
                && outstanding_heartbeat.is_none()
                && accelerated_remaining > 0 => {
                let heartbeat = SignalingModel::new_request::<()>(
                    SignalingType::Heartbeat,
                    None,
                    None,
                )
                .map_err(|error| format!("could not build manager heartbeat: {error}"))?;
                let request_id = heartbeat.request_id.clone();
                sink.send(awc::ws::Message::Text(
                    serde_json::to_string(&heartbeat)?.into(),
                ))
                .await?;
                accelerated_remaining = accelerated_remaining.saturating_sub(1);
                accelerated_at = None;
                outstanding_heartbeat = Some(OutstandingHeartbeat {
                    request_id,
                    deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                });
            }

            _ = async {
                match outstanding_heartbeat.as_ref() {
                    Some(heartbeat) => tokio::time::sleep_until(heartbeat.deadline).await,
                    None => std::future::pending::<()>().await,
                }
            }, if manager_credential_link.is_some() && outstanding_heartbeat.is_some() => {
                outstanding_heartbeat = None;
                if let Some(link) = manager_credential_link.as_ref() {
                    link.proof_unavailable().await;
                    if accelerated_remaining == 0 {
                        accelerated_remaining = 3;
                        accelerated_at = Some(
                            tokio::time::Instant::now()
                                + accelerated_probe_phase(rand::random::<u64>()),
                        );
                    } else if accelerated_remaining > 0 {
                        accelerated_at = Some(
                            tokio::time::Instant::now() + Duration::from_secs(10),
                        );
                    }
                }
            }

            expiry = credential_expiry_rx.recv(), if manager_credential_link.is_some() => {
                if let (Ok(expiry), Some(link)) = (expiry, manager_credential_link.as_ref())
                    && expiry.belongs_to(link)
                {
                    let _ = sink.send(awc::ws::Message::Close(None)).await;
                    credential_expired = true;
                    break;
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
    Ok(if credential_expired {
        ProxyConnectionOutcome::CredentialExpired
    } else {
        ProxyConnectionOutcome::Closed
    })
}
