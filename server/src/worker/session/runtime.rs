use super::*;

impl WorkerSession {
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

        let worker_locale = settings
            .system
            .locale
            .as_deref()
            .unwrap_or(crate::locale::DEFAULT_LOCALE);
        if let Err(error) = crate::locale::set_global_locale(worker_locale) {
            warn!(
                "Worker Init contained invalid locale {worker_locale:?}: {error}; using {}",
                crate::locale::DEFAULT_LOCALE
            );
            let _ = crate::locale::set_global_locale(crate::locale::DEFAULT_LOCALE);
        }
        // The worker's copy of the host security policy. The daemon publishes to
        // it; nothing here writes the policy back. The settings this worker also
        // holds are a startup copy and are never consulted for permissions —
        // they do not receive the daemon's updates.
        let policy_mirror = Arc::new(PolicyMirror::new(PolicySnapshot::new(
            settings.security.clone(),
        )));
        let shared_settings = Arc::new(SharedSettings::from(settings));
        let shared_settings_data = web::Data::from(shared_settings.clone());
        let remote_access_locked = Arc::new(AtomicBool::new(init_payload.remote_access_locked));
        let remote_access_state_version =
            Arc::new(AtomicU64::new(init_payload.remote_access_state_version));

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

        // What every permission gate in this worker reads: the mirror the daemon
        // publishes to, with remembered answers travelling back on the same
        // event lane everything else uses.
        let policy = PolicyAccess::mirrored(Arc::clone(&policy_mirror), writer_tx.clone());

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
        // Per-connection capability ceilings, registered by the daemon via
        // `SetConnectionCeiling` for redeemed-grant sessions and consumed by the
        // worker-side `meet(ceiling, global)` permission gates (the dispatchers and
        // DeskSession below each take a clone). Owner / unrestricted connections
        // are never registered (missing entry = global-only gating).
        let connection_ceilings = crate::worker::connection_ceiling::ConnectionCeilingStore::new();
        // Stop switches for the commands currently running, so a cancel arriving
        // on this loop can reach an execution running in a task of its own.
        let exec_registry = crate::worker::exec_registry::ExecRegistry::new();
        let file_transfer_dispatcher = FileTransferDispatcher::new(
            file_sender,
            Arc::clone(&policy),
            Arc::clone(&host_control_hub),
            connection_ceilings.clone(),
            writer_tx.clone(),
        );
        // Whiteboard dispatcher. Spawns a bridge thread to
        // the host_control_hub on construction; reuses the same hub
        // the DeskSession (legacy / portable path) uses so messages
        // flow through a single Tauri overlay manager.
        let whiteboard_dispatcher = WhiteboardDispatcher::new(
            Arc::clone(&host_control_hub),
            Arc::clone(&policy),
            connection_ceilings.clone(),
        );
        if writer_tx
            .send(WorkerToService::Capabilities(capabilities))
            .is_err()
        {
            error!("IPC writer task died before Capabilities could be sent; exiting");
            return Ok(());
        }

        let mut desk_session = DeskSession::new(
            shared_settings_data.clone(),
            session_sender,
            CurrentUser::new_admin("worker_node"),
            host_control_hub,
            connection_ceilings.clone(),
            Arc::clone(&policy),
            writer_tx.clone(),
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
        // WGC capture sessions bound via `CreateForMonitor(HMONITOR)`
        // survive the CDS commit at the
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
        //
        // Security-policy updates are applied here rather than forwarded. The
        // main loop parks on approval prompts — several gates await one inline
        // — and a policy change is often exactly what should resolve the prompt
        // that is blocking it, so routing the change through the same queue
        // would make it wait on its own effect. Applying it on this task keeps
        // the mirror current no matter what the main loop is doing, and it also
        // means a policy change is not subject to the main loop's locked-state
        // filter: an operator revoking a capability while remote access is
        // locked must not have that revocation discarded.
        //
        // The acknowledgement goes onto the writer queue rather than straight
        // onto the `EventSender`: that sender awaits when the bounded event
        // transport is full, and this task must never wait on the outbound
        // direction — a stalled peer would stop it draining the inbound one and
        // strand every message behind it, `Shutdown` included. Sending on the
        // unbounded queue cannot suspend, which is why nothing here is awaited
        // except the receive itself.
        let (service_msg_tx, mut service_msg_rx) =
            mpsc::unbounded_channel::<Option<ServiceToWorker>>();
        {
            let policy_mirror = Arc::clone(&policy_mirror);
            let ack_tx = writer_tx.clone();
            tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Some(ServiceToWorker::UpdateSecurityPolicy(payload)) => {
                            let outcome = policy_mirror.apply(payload.snapshot);
                            // An error means the writer queue is gone, i.e. the
                            // session is tearing down and nobody is waiting for
                            // the acknowledgement any more.
                            let _ = ack_tx.send(WorkerToService::SecurityPolicyApplied(
                                SecurityPolicyAppliedPayload {
                                    operation_id: payload.operation_id,
                                    outcome,
                                },
                            ));
                        }
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
        }

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
            let remote_access_locked = Arc::clone(&remote_access_locked);
            tokio::spawn(async move {
                while let Some(payload) = file_receiver.recv().await {
                    if remote_access_locked.load(Ordering::Acquire) {
                        warn!("Dropping file-lane command while remote access is locked");
                        continue;
                    }
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
                            if remote_access_locked.load(Ordering::Acquire)
                                && !survives_remote_access_lock(&msg)
                            {
                                warn!("Dropping worker command while remote access is locked");
                                continue;
                            }
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
                                ServiceToWorker::UpdateSecurityPolicy(_) => {
                                    // Applied on the transport reader task, ahead of this
                                    // loop, so a policy change is never queued behind the
                                    // approval prompt it would resolve. Arriving here means
                                    // that interception was bypassed and the mirror is stale.
                                    error!(
                                        "Security policy update reached the main loop; \
                                         the policy mirror was not updated"
                                    );
                                }
                                ServiceToWorker::SetRemoteAccessState(payload) => {
                                    let current_version =
                                        remote_access_state_version.load(Ordering::Acquire);
                                    let was_locked =
                                        remote_access_locked.load(Ordering::Acquire);
                                    if payload.state_version > current_version {
                                        remote_access_locked.store(payload.locked, Ordering::Release);
                                        remote_access_state_version
                                            .store(payload.state_version, Ordering::Release);
                                    } else if payload.state_version == current_version
                                        && payload.locked
                                            != remote_access_locked.load(Ordering::Acquire)
                                    {
                                        error!(
                                            "Conflicting remote-access state at version {}; failing closed",
                                            payload.state_version
                                        );
                                        remote_access_locked.store(true, Ordering::Release);
                                    }
                                    let now_locked =
                                        remote_access_locked.load(Ordering::Acquire);
                                    let (cancelled_terminals, cancelled_transfers, cancelled_execs) =
                                        if now_locked && !was_locked {
                                            let cancelled_terminals = desk_session
                                                .cancel_all_remote_activity()
                                                .await;
                                            let cancelled_transfers = file_transfer_dispatcher
                                                .active_transfer_count()
                                                .await;
                                            file_transfer_dispatcher.shutdown().await;
                                            let cancelled_execs = exec_registry.cancel_all();
                                            input_dispatcher.shutdown();
                                            if let Some(dispatcher) = clipboard_dispatcher.as_ref() {
                                                dispatcher.shutdown().await;
                                            }
                                            whiteboard_dispatcher.shutdown().await;
                                            connection_ceilings.clear_all().await;
                                            if let Some(producer) = media_producer.as_ref() {
                                                producer.shutdown();
                                            }
                                            (
                                                cancelled_terminals,
                                                cancelled_transfers,
                                                cancelled_execs,
                                            )
                                        } else {
                                            (0, 0, 0)
                                        };
                                    let _ = writer_tx.send(
                                        WorkerToService::RemoteAccessStateApplied(
                                            RemoteAccessStateAppliedPayload {
                                                operation_id: payload.operation_id,
                                                state_version: remote_access_state_version
                                                    .load(Ordering::Acquire),
                                                cancelled_terminals,
                                                cancelled_transfers,
                                                cancelled_execs,
                                            },
                                        ),
                                    );
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
                                        // Snapshot the layout when capture starts
                                        // so we
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
                                    desk_session.clear_file_permissions(&payload.connection_id);
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
                                // These typed requests replace the legacy
                                // `SignalingMessage` opaque envelope. The worker still
                                // dispatches through `DeskSession::
                                // handle_message` because the actual
                                // handlers in `service::signaling` are
                                // shared with the portable / DeskServer WS
                                // path and shouldn't be duplicated; we
                                // rebuild a lightweight `SignalingModel`
                                // from the typed payload so the existing
                                // arms keep working without duplicating the handlers.
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
                                // Manager-plane typed requests rebuild a
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
                                ServiceToWorker::ManagerFileListRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ManagerFileList,
                                        payload.request_id,
                                        Some(payload.connection_id),
                                        Some(&payload.params),
                                    )
                                    .await;
                                }
                                ServiceToWorker::ManagerFileDeleteRequest(payload) => {
                                    dispatch_typed_signaling_with_request_id(
                                        &mut desk_session,
                                        SignalingType::ManagerFileDelete,
                                        payload.request_id,
                                        Some(payload.connection_id),
                                        Some(&payload.request),
                                    )
                                    .await;
                                }
                                ServiceToWorker::SetLocale(payload) => {
                                    // The daemon has already persisted this; the
                                    // worker only brings its own process and
                                    // settings copy in line. Writing the file here
                                    // would push this worker's startup snapshot of
                                    // everything else back over the daemon's.
                                    let result = async {
                                        let locale = crate::locale::canonicalize(&payload.locale)
                                            .ok_or_else(|| {
                                                format!(
                                                    "unsupported locale {:?}",
                                                    payload.locale
                                                )
                                            })?;
                                        shared_settings_data.write().await.system.locale =
                                            Some(locale.to_string());
                                        crate::locale::set_global_locale(locale)?;
                                        Ok::<_, String>(locale.to_string())
                                    }
                                    .await;
                                    match result {
                                        Ok(locale) => {
                                            let _ = event_tx
                                                .send(WorkerToService::LocaleApplied(
                                                    LocaleAppliedPayload {
                                                        operation_id: payload.operation_id,
                                                        locale,
                                                    },
                                                ))
                                                .await;
                                        }
                                        Err(error) => {
                                            warn!("Worker failed to apply locale: {error}");
                                        }
                                    }
                                }
                                // Terminal-plane typed requests rebuild a
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
                                // The daemon-side `dc.send` failed. The
                                // daemon already classified and logged the
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
                                        // Log the full GDI layout right after a
                                        // new IDD monitor attaches, so it is
                                        // visible whether Windows made it the
                                        // primary by default — the suspected
                                        // reason the virtual display sorts
                                        // first and the Tauri prompt opens on
                                        // it instead of the screen the local
                                        // user is watching.
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
                                    // Snapshot layout the instant the worker is told
                                    // to detach, before any teardown runs.
                                    // Useful for understanding what state
                                    // we were in just before the IDD goes
                                    // away (paired with the post-attach log
                                    // for the next cycle).
                                    desk_virtual_display::log_active_displays_for_diagnostics(
                                        "pre-detach",
                                    );
                                    // By this point the daemon should have already
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
                                    // Register before the execution starts, so a
                                    // cancel racing the spawn still finds it.
                                    let (cancel, registration) =
                                        exec_registry.register(&payload.plan.execution_generation);
                                    tokio::spawn(async move {
                                        // Held for the whole run; dropping it
                                        // deregisters however the command ends.
                                        let _registration = registration;
                                        // Report progress for as long as the
                                        // command runs, so an upstream watching a
                                        // long command never has to infer from
                                        // silence whether it is still alive.
                                        let heartbeat = {
                                            let tx = writer_tx.clone();
                                            let request_id = payload.request_id.clone();
                                            let connection_id = payload.connection_id.clone();
                                            tokio::spawn(async move {
                                                let started = std::time::Instant::now();
                                                let mut ticker = tokio::time::interval(
                                                    EXEC_HEARTBEAT_INTERVAL,
                                                );
                                                // The first tick completes at once;
                                                // the execution has only just begun,
                                                // and the spawn report already said so.
                                                ticker.tick().await;
                                                loop {
                                                    ticker.tick().await;
                                                    let beat = ExecHeartbeatPayload {
                                                        request_id: request_id.clone(),
                                                        connection_id: connection_id.clone(),
                                                        running_ms: started
                                                            .elapsed()
                                                            .as_millis()
                                                            .min(u64::MAX as u128)
                                                            as u64,
                                                    };
                                                    if tx
                                                        .send(WorkerToService::ExecHeartbeat(beat))
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                            })
                                        };
                                        // Report the spawn as soon as it is known,
                                        // ahead of the result: the daemon reserved
                                        // this execution and needs to know it is
                                        // actually running (and how to reclaim it)
                                        // without waiting for the command to end.
                                        let spawn_tx = writer_tx.clone();
                                        let spawn_request_id = payload.request_id.clone();
                                        let spawn_connection_id = payload.connection_id.clone();
                                        let outcome = crate::worker::exec::execute_plan_cancellable(
                                            &payload.plan,
                                            move |report| {
                                                if spawn_tx
                                                    .send(WorkerToService::ExecSpawnReport(
                                                        ExecSpawnReportPayload {
                                                            request_id: spawn_request_id,
                                                            connection_id: spawn_connection_id,
                                                            report,
                                                        },
                                                    ))
                                                    .is_err()
                                                {
                                                    warn!(
                                                        "writer task closed; dropping ExecSpawnReport"
                                                    );
                                                }
                                            },
                                            Some(cancel.subscribe()),
                                        )
                                        .await;
                                        // Nothing is still running to report on.
                                        heartbeat.abort();
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
                                ServiceToWorker::ExecCancel(ExecCancelPayload {
                                    execution_generation,
                                }) => {
                                    // Nothing to stop is the ordinary outcome of a
                                    // cancel that arrived just as the command
                                    // finished; the daemon answers from its ledger.
                                    let stopped = exec_registry.cancel(&execution_generation);
                                    info!(
                                        "Worker received ExecCancel generation={execution_generation} stopped={stopped}"
                                    );
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
                            // type. There is no opaque-envelope
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

                // The exclusive coordinator posts here after each
                // successful enter_exclusive /
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
}
