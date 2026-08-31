use super::*;

/// Handle an inbound `EdgeExecRequest` from a trusted central brain (manager or
/// OSS signal). The frame has already passed the proxy's trusted-central source
/// gate and dedicated authz gate (which
/// unwrapped the inner [`ExecPlan`] and set `ctx.inbound_authz`). This re-
/// validates the plan (PEP) and, on success, dispatches it to the worker
/// correlated as a fleet execution; every exit emits exactly one
/// `EdgeExecResult` so the manager's pending entry always resolves.
pub(super) async fn handle_edge_exec_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request_id = model.request_id.clone();

    // The dedicated authz gate sets `inbound_authz` on success; its absence here
    // is a routing fault. Reject (definitely not executed) rather than dispatch
    // an unauthorized plan.
    let Some(authz) = ctx.inbound_authz.clone() else {
        send_edge_execution_completed(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: agent_error(
                    AgentErrorKind::PermissionDenied,
                    "pep_rejected:missing_authorization",
                    false,
                    true,
                ),
            },
        );
        return Ok(());
    };

    // The frame carries a source-tagged envelope (`Fleet` / `Agentic`), never a
    // bare `ExecPlan`: the two sources need different re-validation (fleet re-renders
    // an operator template with fixed defaults; agentic re-classifies the original
    // input). A missing tag / missing agentic input is a decode error → rejected.
    let payload = match model.get_data::<EdgeExecRequestPayload>() {
        Ok(p) => p,
        Err(e) => {
            send_edge_execution_completed(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    error: agent_error(
                        AgentErrorKind::InvalidInput,
                        &format!("pep_rejected:malformed_plan:{e}"),
                        false,
                        true,
                    ),
                },
            );
            return Ok(());
        }
    };

    let carrier_id = payload.carrier_id().map(str::to_string);
    let carrier_shape_valid = match &payload {
        EdgeExecRequestPayload::Fleet { plan } => !plan.io_mode.is_pty(),
        EdgeExecRequestPayload::Agentic {
            plan, carrier_id, ..
        } => plan.io_mode.is_pty() == carrier_id.as_ref().is_some_and(|id| !id.is_empty()),
    };
    if !carrier_shape_valid {
        send_edge_execution_completed(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: agent_error(
                    AgentErrorKind::InvalidInput,
                    "pep_rejected:interactive_carrier_mismatch",
                    false,
                    true,
                ),
            },
        );
        return Ok(());
    }

    // Bind the plan's identifiers to the authz-validated frame. The authz block was
    // validated against `request_id` (the frame id) by the proxy gate, and the worker
    // is correlated on that same `request_id`; a plan whose own dispatch id names a
    // *different* attempt, or that carries an empty `approval_id`, is malformed — the
    // daemon must not let a plan self-report an id that diverges from the one the
    // authz proof covers, nor dispatch a plan with no approval token. The whole-draft
    // re-render can never catch these fields (they are not on the draft), so gate
    // them here.
    //
    // The frame id is bound to `execution_generation`, the per-dispatch axis, not to
    // `exec_request_id`. The task id is stable across retries by design, so requiring
    // it to equal a per-delivery frame id would force it to change on every retry and
    // collapse the two axes back into one. The task id is still checked, just for
    // presence: a plan that names no task cannot be reconciled with anything.
    {
        let plan = payload.plan();
        if plan.execution_generation != request_id {
            send_edge_execution_completed(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    error: agent_error(
                        AgentErrorKind::InvalidInput,
                        "pep_rejected:execution_generation_mismatch",
                        false,
                        true,
                    ),
                },
            );
            return Ok(());
        }
        if plan.exec_request_id.0.is_empty() {
            send_edge_execution_completed(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    error: agent_error(
                        AgentErrorKind::InvalidInput,
                        "pep_rejected:missing_exec_request_id",
                        false,
                        true,
                    ),
                },
            );
            return Ok(());
        }
        if plan.approval_id.0.is_empty() {
            send_edge_execution_completed(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    error: agent_error(
                        AgentErrorKind::ApprovalRequired,
                        "pep_rejected:missing_approval_id",
                        false,
                        true,
                    ),
                },
            );
            return Ok(());
        }
    }

    // Exec must be runnable in this startup mode. The manager's pre-claim version
    // gate normally prevents dispatch to a daemon that cannot execute, but a PEP
    // must never assume the PDP got it right.
    let execution_available = ctx.exec_supported || ctx.worker_mgr.uses_session_targeting();
    if !execution_available {
        send_edge_execution_completed(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: agent_error(
                    AgentErrorKind::UnsupportedPlatform,
                    "pep_rejected:exec_unsupported_in_mode",
                    false,
                    true,
                ),
            },
        );
        return Ok(());
    }

    // Owner free-form is an interactive-only admission. The central grant and
    // the host's current local execution mode are both authoritative; either can
    // tighten to ReadOnly/SuggestOnly after the preview, which must shut this
    // dispatch down before worker handoff.
    let local_policy = ctx.settings.read().await.ai_policy.clone();
    let effective_mode = { authz.scope.mode.restrict_to(local_policy.execution_mode) };
    if payload.plan().execution_basis
        == desk_agent_protocol::exec::ExecExecutionBasis::OwnerBlocklistOnly
        && !matches!(
            effective_mode,
            ExecutionMode::ConfirmEachAction | ExecutionMode::SessionApproved
        )
    {
        send_edge_execution_completed(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: agent_error(
                    AgentErrorKind::PermissionDenied,
                    "pep_rejected:owner_interactive_mode_disabled",
                    false,
                    true,
                ),
            },
        );
        return Ok(());
    }

    let templates = ctx.command_templates.snapshot();
    let effective_blocklist = ctx.command_blocklist.snapshot();
    let rejection = match &payload {
        EdgeExecRequestPayload::Fleet { plan } => {
            validate_fleet_edge_exec(plan, authz.max_risk, &templates, &effective_blocklist)
        }
        EdgeExecRequestPayload::Agentic {
            plan,
            validation_input,
            ..
        } => {
            let mut validation_input = validation_input.clone();
            desk_diagnose_core::exec_tools::apply_exec_runtime_ceiling(
                &mut validation_input,
                local_policy
                    .max_command_runtime_seconds
                    .saturating_mul(1_000),
            );
            validate_agentic_edge_exec(
                plan,
                &validation_input,
                authz.exec_admission_policy,
                authz.max_risk,
                &templates,
                &effective_blocklist,
            )
        }
    };
    if let Some(reason) = rejection {
        log::warn!("[edge-exec] PEP rejected plan for request {request_id}: {reason}");
        send_edge_execution_completed(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: agent_error(AgentErrorKind::RiskBlocked, &reason, false, true),
            },
        );
        return Ok(());
    }

    // Preserve only the opaque control-connection anchor before dropping the
    // daemon-only validation envelope. The host resolves that connection through
    // its own immutable connection→session binding; no central component sees a
    // platform session key.
    let session_connection_id = payload.session_connection_id().map(str::to_string);
    dispatch_fleet_exec_plan(
        ctx,
        &request_id,
        payload.into_plan(),
        session_connection_id.as_deref(),
        carrier_id,
    )
    .await;
    Ok(())
}

/// Dispatch a PEP-validated fleet [`ExecPlan`] to the worker, correlated so the
/// worker's `WorkerToService::ExecutionCompleted` is relayed back to the trusted central as a
/// `EdgeExecResult(Executed{..})` (see the proxy's `ExecutionCompleted` handler). On a
/// send failure the plan never reached the worker, so the change definitely did
/// not run → `DispatchFailedBeforeWorker`.
pub(super) async fn dispatch_fleet_exec_plan(
    ctx: &RouterContext,
    request_id: &str,
    plan: ExecPlan,
    session_connection_id: Option<&str>,
    carrier_id: Option<String>,
) {
    // A ServiceDaemon can host several independently ready desktop sessions.
    // Resolve the execution anchor before claiming the at-most-once ledger: an
    // ambiguous target is definitely not dispatched and must not consume or
    // poison this execution generation. Portable/DeskServer keep returning
    // `None` here and use their anonymous single-worker adapter.
    let selected_session = match ctx.worker_mgr.resolve_session_target_for_connection(
        crate::daemon::session_target::SessionCapability::Assistant,
        session_connection_id,
    ) {
        Ok(session) => session,
        Err(error) => {
            send_edge_execution_completed(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    error: agent_error(
                        AgentErrorKind::SessionUnavailable,
                        &format!("assistant session target unavailable: {error}"),
                        true,
                        true,
                    ),
                },
            );
            return;
        }
    };

    let pty_capabilities = current_exec_pty_capabilities(&ctx.settings).await;
    if let Some(reason) = pty_dispatch_refusal(&plan, pty_capabilities) {
        send_edge_execution_completed(
            &ctx.outbound_tx,
            request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                error: agent_error(AgentErrorKind::UnsupportedCapability, reason, false, true),
            },
        );
        return;
    }

    // Claim this dispatch in the ledger before the worker can start anything.
    match admit_exec(ctx, &plan).await {
        ExecAdmission::Spawn => {}
        ExecAdmission::Replay(result) => {
            let outcome = serde_json::from_str::<AgentOutcome>(&result).unwrap_or_else(|e| {
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    &format!("stored result could not be read: {e}"),
                    false,
                    true,
                ))
            });
            send_edge_execution_completed(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::Executed { outcome },
            );
            return;
        }
        ExecAdmission::AcceptedOutcomeUnknown(reason) => {
            // `ExecutionStateUnknown` rather than a pre-dispatch variant: only the
            // pre-dispatch ones assert the change did not run, and this one cannot.
            send_edge_execution_completed(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::ExecutionStateUnknown { reason },
            );
            return;
        }
        ExecAdmission::Refused(reason) => {
            send_edge_execution_completed(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    error: agent_error(AgentErrorKind::PermissionDenied, &reason, false, true),
                },
            );
            return;
        }
        ExecAdmission::AtCapacity(reason) => {
            send_edge_execution_completed(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::HostAtCapacity {
                    error: agent_error(AgentErrorKind::HostAtCapacity, &reason, true, true),
                },
            );
            return;
        }
    }

    // Register the in-flight correlation BEFORE sending so a fast worker reply
    // cannot race ahead of the marker.
    if let Ok(mut pending) = ctx.edge_exec_pending.lock() {
        pending.insert(request_id.to_string());
    }
    #[cfg(target_os = "linux")]
    if plan.requires_root_pty_containment() {
        let dispatch = dispatch_root_fleet_pty(
            ctx,
            request_id,
            plan,
            selected_session.as_ref(),
            session_connection_id,
            carrier_id,
            pty_capabilities,
        )
        .await;
        if let Err(error) = dispatch {
            if let Ok(mut pending) = ctx.edge_exec_pending.lock() {
                pending.remove(request_id);
            }
            match ctx
                .exec_ledger
                .mark_terminal(
                    request_id,
                    crate::daemon::exec_ledger::Terminal::SpawnFailed(error.clone()),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => log::error!(
                    "[exec-ledger] root PTY pre-spawn failure could not settle {request_id}"
                ),
                Err(ledger_error) => log::error!(
                    "[exec-ledger] could not record root PTY pre-spawn failure {request_id}: {ledger_error}"
                ),
            }
            ctx.exec_capacity.release(request_id);
            send_edge_execution_completed(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::DispatchFailedBeforeWorker {
                    error: agent_error(
                        AgentErrorKind::SessionUnavailable,
                        &format!("root PTY unavailable: {error}"),
                        true,
                        true,
                    ),
                },
            );
        }
        return;
    }
    let dispatch = if plan.io_mode.is_pty() {
        match (carrier_id, ctx.exec_pty_link.clone()) {
            (Some(stream_id), Some(link)) => match ctx
                .worker_mgr
                .exec_worker_target_for_connection(session_connection_id)
                .await
            {
                Ok(target) => {
                    let payload = desk_ipc_protocol::message::ExecPtyStartPayload {
                        request_id: request_id.to_string(),
                        connection_id: None,
                        exec_pty: pty_capabilities.exec_pty,
                        exec_pty_elevation: pty_capabilities.exec_pty_elevation,
                        stream_id: stream_id.clone(),
                        session_target_id: target.session_target_id,
                        registration_generation: target.registration_generation,
                        worker_incarnation: target.wire_worker_incarnation,
                        plan,
                        audit_source_request_id: Some(request_id.to_string()),
                    };
                    match link.registry.bind(
                        link.link_id,
                        &payload,
                        target.worker_key.clone(),
                        target.source_incarnation,
                        link.outbound,
                    ) {
                        Ok(()) => {
                            let sent = match target.worker_key.as_ref() {
                                Some(key) => {
                                    ctx.worker_mgr
                                        .send_to_session_worker(
                                            &key.session,
                                            ServiceToWorker::ExecPtyStart(payload),
                                        )
                                        .await
                                }
                                None => {
                                    ctx.worker_mgr
                                        .send_to_worker(ServiceToWorker::ExecPtyStart(payload))
                                        .await
                                }
                            };
                            if sent.is_err() {
                                link.registry.remove_stream(
                                    &stream_id,
                                    desk_agent_protocol::exec_pty::PtyCloseReason::SessionStale,
                                );
                            }
                            sent
                        }
                        Err(error) => Err(format!("PTY carrier binding failed: {error}")),
                    }
                }
                Err(error) => Err(error),
            },
            (None, _) => Err("approved PTY execution has no live carrier".to_string()),
            (_, None) => Err("trusted central link has no PTY binary carrier".to_string()),
        }
    } else {
        let payload = ExecPlanPayload {
            request_id: request_id.to_string(),
            // No browser connection: a fleet result is routed by `request_id`, not a
            // control-end connection id.
            connection_id: None,
            plan,
            audit_source_request_id: Some(request_id.to_string()),
        };
        if let Some(session) = selected_session.as_ref() {
            ctx.worker_mgr
                .send_to_session_worker(session, ServiceToWorker::ExecPlan(payload))
                .await
        } else {
            ctx.worker_mgr
                .send_to_worker(ServiceToWorker::ExecPlan(payload))
                .await
        }
    };
    if let Err(e) = dispatch {
        if let Ok(mut pending) = ctx.edge_exec_pending.lock() {
            pending.remove(request_id);
        }
        // Nothing was started, so the slot is free again immediately.
        ctx.exec_capacity.release(request_id);
        send_edge_execution_completed(
            &ctx.outbound_tx,
            request_id,
            EdgeExecDisposition::DispatchFailedBeforeWorker {
                error: agent_error(
                    AgentErrorKind::SessionUnavailable,
                    &format!("worker unavailable: {e}"),
                    true,
                    true,
                ),
            },
        );
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
async fn dispatch_root_fleet_pty(
    ctx: &RouterContext,
    request_id: &str,
    plan: ExecPlan,
    selected_session: Option<&desk_ipc_protocol::message::SessionKey>,
    session_connection_id: Option<&str>,
    carrier_id: Option<String>,
    capabilities: crate::worker::exec_pty::ExecPtyCapabilities,
) -> Result<(), String> {
    if !capabilities.exec_pty_elevation {
        return Err("interactive elevation runtime is not ready".into());
    }
    let session = selected_session.ok_or("root PTY requires a registered Linux session")?;
    let target = ctx
        .worker_mgr
        .exec_worker_target_for_connection(session_connection_id)
        .await?;
    if target.worker_key.as_ref().map(|key| &key.session) != Some(session) {
        return Err("root PTY session target changed before dispatch".into());
    }
    let registration = ctx
        .worker_mgr
        .session_shell_registration(session)
        .ok_or("root PTY session registration is no longer current")?;
    let session_registry = ctx
        .worker_mgr
        .session_shell_registry()
        .ok_or("root PTY session registry is unavailable")?;
    let stream_id = carrier_id
        .filter(|value| !value.is_empty())
        .ok_or("approved root PTY has no live carrier")?;
    let link = ctx
        .exec_pty_link
        .clone()
        .ok_or("trusted central link has no PTY binary carrier")?;
    let payload = desk_ipc_protocol::message::ExecPtyStartPayload {
        request_id: request_id.to_string(),
        connection_id: None,
        exec_pty: capabilities.exec_pty,
        exec_pty_elevation: capabilities.exec_pty_elevation,
        stream_id: stream_id.clone(),
        session_target_id: target.session_target_id,
        registration_generation: target.registration_generation,
        worker_incarnation: target.wire_worker_incarnation,
        plan,
        audit_source_request_id: Some(request_id.to_string()),
    };
    let (control_tx, control_rx) = tokio::sync::mpsc::channel(32);
    link.registry
        .bind_daemon(link.link_id, &payload, control_tx, link.outbound.clone())
        .map_err(|error| format!("PTY carrier binding failed: {error}"))?;

    let task_ctx = ctx.clone();
    let task_request_id = request_id.to_string();
    let carrier = link.registry.clone();
    tokio::spawn(async move {
        let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started_callback = Arc::clone(&started);
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
        let start_outbound = task_ctx.outbound_tx.clone();
        let start_request = task_request_id.clone();
        let result = crate::daemon::linux_exec_pty::run_root_pty(
            payload.clone(),
            registration,
            session_registry,
            carrier.clone(),
            control_rx,
            Arc::clone(&task_ctx.exec_ledger),
            move |containment_identity| {
                started_callback.store(true, std::sync::atomic::Ordering::Release);
                tokio::spawn(async move {
                    crate::daemon::signaling_proxy::inbound::send_exec_lifecycle(
                        &start_outbound,
                        &start_request,
                        None,
                        desk_agent_protocol::exec_lifecycle::ExecLifecycleEvent::Accepted {
                            containment_identity: Some(containment_identity),
                        },
                    );
                    let began = std::time::Instant::now();
                    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(10));
                    heartbeat.tick().await;
                    loop {
                        tokio::select! {
                            _ = heartbeat.tick() => {
                                crate::daemon::signaling_proxy::inbound::send_exec_lifecycle(
                                    &start_outbound,
                                    &start_request,
                                    None,
                                    desk_agent_protocol::exec_lifecycle::ExecLifecycleEvent::Heartbeat {
                                        running_ms: began.elapsed().as_millis().min(u64::MAX as u128) as u64,
                                    },
                                );
                            }
                            changed = done_rx.changed() => {
                                if changed.is_err() || *done_rx.borrow() {
                                    break;
                                }
                            }
                        }
                    }
                });
            },
        )
        .await;
        done_tx.send_replace(true);

        let (outcome, spawn_failure) = match result {
            Ok(execution) => (execution.outcome, None),
            Err(error) => {
                carrier.remove_stream(
                    &stream_id,
                    desk_agent_protocol::exec_pty::PtyCloseReason::InternalError,
                );
                let outcome = AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    &format!("root PTY execution failed: {error}"),
                    false,
                    true,
                ));
                let spawn_failure =
                    (!started.load(std::sync::atomic::Ordering::Acquire)).then_some(error);
                (outcome, spawn_failure)
            }
        };
        let terminal = match spawn_failure {
            Some(reason) => crate::daemon::exec_ledger::Terminal::SpawnFailed(reason),
            None => crate::daemon::exec_ledger::Terminal::Completed(
                serde_json::to_string(&outcome).unwrap_or_else(|_| "null".into()),
            ),
        };
        if let Err(error) = task_ctx
            .exec_ledger
            .mark_terminal(&task_request_id, terminal)
            .await
        {
            log::error!(
                "[exec-ledger] could not record root PTY result {}: {error}",
                task_request_id
            );
        }
        task_ctx.exec_capacity.release(&task_request_id);
        let (success, summary, redactions) = match &outcome {
            AgentOutcome::Ok(desk_agent_protocol::OperationOutput::Exec(output)) => (
                output.exit_code == 0,
                format!("exit {}", output.exit_code),
                output.redactions.len() as i32,
            ),
            AgentOutcome::Ok(_) => (true, "ok".to_string(), 0),
            AgentOutcome::Err(error) => (false, format!("{:?}", error.kind), 0),
        };
        task_ctx
            .audit
            .record(
                desk_agent_protocol::audit::AuditEvent::command_completed(
                    uuid::Uuid::new_v4().to_string(),
                    chrono::Utc::now().to_rfc3339(),
                    &payload.plan.exec_request_id.0,
                    success,
                    summary,
                    redactions,
                    0,
                )
                .with_task_id(Some(&task_request_id)),
            )
            .await;
        if let Ok(mut pending) = task_ctx.edge_exec_pending.lock() {
            pending.remove(&task_request_id);
        }
        send_edge_execution_completed(
            &task_ctx.outbound_tx,
            &task_request_id,
            EdgeExecDisposition::Executed { outcome },
        );
    });
    Ok(())
}

/// Assemble the authoritative [`AgentEnvelope`] from a parsed control-end
/// operation by injecting every trusted field server-side. Pure so the
/// trusted-field-injection invariant is unit-testable.
pub(super) fn build_agent_envelope(
    request_id: &str,
    operation: AgentOperation,
    reason: Option<String>,
    scope: AgentScope,
) -> AgentEnvelope {
    AgentEnvelope {
        protocol_version: ProtocolVersion::default(),
        // Server-owned: the control end's value (if any) is replaced.
        request_id: RequestId(request_id.to_string()),
        parent_task_id: None,
        // Single-machine local target. `device_id` empty until a device
        // registry assigns one; never self-reported by the control end.
        target: TargetRef::default(),
        actor: server_actor(),
        // No model caller yet (no orchestrator); a human operator
        // drove this directly.
        caller: CallerRef {
            caller_type: CallerType::Human,
            model_provider: None,
            model_name: None,
            adapter: None,
        },
        scope,
        operation,
        audit: desk_agent_protocol::AuditMeta {
            approval_id: None,
            reason,
        },
    }
}

/// Route a control-end `InvokeAgentCapability`: two-phase parse → capability
/// derivation → authorization → trusted-field stamp → typed worker IPC.
pub(super) async fn handle_invoke_agent_capability_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    // The AI read collectors expose host data beyond the remote view, so what
    // may leave this host is gated locally by the fail-closed collection policy
    // (`allow_logs` / `allow_screen`) and centrally by the authorization scope
    // below. Provider credentials live on the central brain, so there is no local
    // "gateway configured" gate here: an `InvokeAgentCapability` arrives already
    // authorized from the central link (or, off it, runs under the local read
    // scope).
    let Some(raw) = model.get_raw_data().as_ref() else {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::InvalidInput,
                "missing InvokeAgentCapability body",
                false,
                true,
            ),
        );
        return Ok(());
    };

    // Reject unknown kinds gracefully before typed parsing.
    if let Err(e) = validate_invoke_agent_capability_kinds(raw) {
        emit_agent_error(ctx, model, e);
        return Ok(());
    }

    // Kinds are known → typed parse is safe.
    let request_data = match model.get_data::<AgentRequestData>() {
        Ok(d) => d,
        Err(e) => {
            emit_agent_error(
                ctx,
                model,
                agent_error(
                    AgentErrorKind::InvalidInput,
                    &format!("bad InvokeAgentCapability payload: {e}"),
                    false,
                    true,
                ),
            );
            return Ok(());
        }
    };

    // The `InvokeAgentCapability(600)` plane is **read-only, permanently**. Exec must go
    // through the `ConfirmExec` → `ResolveExec` confirm flow (which classifies,
    // requires explicit approval, and ships a sealed `ExecPlan`); it can never
    // ride the raw capability path, even once execution is wired up. Reject it
    // explicitly here regardless of `execution_mode` or prior approvals.
    if matches!(request_data.operation.input, OperationInput::Exec(_)) {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::UnsupportedCapability,
                "exec is not available on the agent-request plane; use the confirm-execution flow",
                false,
                true,
            ),
        );
        return Ok(());
    }

    // Capability is derived from the input (single source of truth). Exec is
    // already rejected above, so a `None` here is an unexpected non-exec input.
    let Some(capability) = request_data.operation.input.capability() else {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::UnsupportedCapability,
                "unsupported operation",
                false,
                true,
            ),
        );
        return Ok(());
    };

    // Authorize against the server-computed scope. On the manager link the
    // injected policy decision replaces the local default read scope (fleet
    // PDP); without it (single-machine / remote-signaling) the local read scope
    // applies.
    let scope = match &ctx.inbound_authz {
        Some(authz) => authz.scope.clone(),
        None => default_read_scope(),
    };
    if !authorize(capability, &scope.granted) {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::PermissionDenied,
                "capability not granted",
                false,
                false,
            ),
        );
        return Ok(());
    }

    // Stamp trusted fields and forward to the worker.
    let envelope = build_agent_envelope(
        &model.request_id,
        request_data.operation,
        request_data.reason,
        scope,
    );
    let envelope = match envelope.try_into() {
        Ok(envelope) => envelope,
        Err(_) => {
            emit_agent_error(
                ctx,
                model,
                agent_error(
                    AgentErrorKind::InvalidInput,
                    "mutation cannot enter the read-only agent capability lane",
                    false,
                    false,
                ),
            );
            return Ok(());
        }
    };
    let payload = AgentRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: model.from_connection_id.clone(),
        envelope,
    };
    let Some(connection_id) = model.from_connection_id.as_deref() else {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::TargetOffline,
                "AI Assistant request has no selected desktop session",
                false,
                true,
            ),
        );
        return Ok(());
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_connection_worker(
            connection_id,
            ServiceToWorker::InvokeAgentCapability(payload),
        )
        .await
    {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::TargetOffline,
                &format!("worker unavailable: {e}"),
                true,
                true,
            ),
        );
    }
    Ok(())
}

/// Dispatch one immutable Computer Use mutation after re-validating the
/// trusted-central authorization wrapper. This lane is deliberately separate
/// from `InvokeAgentCapability`, whose wire type cannot represent mutation.
pub(super) async fn handle_computer_action_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::computer_use::SealedComputerActionPlan;
    use desk_ipc_protocol::message::ComputerActionPlanPayload;

    let reject = |ctx: &RouterContext, model: &SignalingModel, message: String| {
        emit_computer_action_rejected(ctx, model, &message);
    };
    let Some(authz) = ctx.inbound_authz.as_ref() else {
        reject(
            ctx,
            model,
            "Computer Action authorization is missing".into(),
        );
        return Ok(());
    };
    let plan = match model.get_data::<SealedComputerActionPlan>() {
        Ok(plan) => plan,
        Err(error) => {
            reject(
                ctx,
                model,
                format!("invalid sealed Computer Action plan: {error}"),
            );
            return Ok(());
        }
    };
    if let Err(error) = plan.validate() {
        reject(
            ctx,
            model,
            format!("invalid sealed Computer Action plan: {error}"),
        );
        return Ok(());
    }
    let actor_matches = authz
        .actor
        .user_id
        .is_some_and(|actor| actor.to_string() == plan.approved_actor_id);
    let capabilities_match = plan.actions.iter().all(|step| {
        authz
            .scope
            .granted
            .contains(&step.action.required_capability())
    });
    let expiry_valid = chrono::DateTime::parse_from_rfc3339(&plan.expires_at)
        .is_ok_and(|expiry| expiry > chrono::Utc::now());
    if model.request_id != plan.execution_generation
        || plan.device_id != authz.audience
        || !actor_matches
        || !capabilities_match
        || !expiry_valid
    {
        reject(
            ctx,
            model,
            "sealed Computer Action binding or expiry check failed".into(),
        );
        return Ok(());
    }
    let payload = ComputerActionPlanPayload {
        request_id: model.request_id.clone(),
        connection_id: model.from_connection_id.clone(),
        plan,
    };
    if let Err(error) = ctx
        .worker_mgr
        .send_central_or_connection_worker(
            model.from_connection_id.as_deref(),
            ServiceToWorker::ComputerActionPlan(payload),
        )
        .await
    {
        emit_computer_action_rejected(
            ctx,
            model,
            "Computer Action worker unavailable or desktop session not selected",
        );
        log::debug!("Computer Action worker handoff rejected: {error}");
    }
    Ok(())
}

/// A central stop request cannot grant a new action or infer a completed effect.
/// The worker compares the stamped actor and full identity with its original lease.
pub(super) async fn handle_computer_action_cancel_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::computer_use::ComputerActionCancel;
    use desk_ipc_protocol::message::ComputerActionCancelPayload;

    let reject = || {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::PERMISSION_ERROR,
            "Computer Action stop authorization or binding is invalid",
        )
    };
    let Some(authz) = ctx.inbound_authz.as_ref() else {
        reject();
        return Ok(());
    };
    let Some(actor) = authz.actor.user_id.filter(|actor| *actor > 0) else {
        reject();
        return Ok(());
    };
    if authz.request_id != model.request_id
        || authz
            .validate(
                &model.request_id,
                &authz.audience,
                &chrono::Utc::now().to_rfc3339(),
            )
            .is_err()
    {
        reject();
        return Ok(());
    }
    let Ok(cancel) = model.get_data::<ComputerActionCancel>() else {
        reject();
        return Ok(());
    };
    if [
        &cancel.work_id,
        &cancel.action_request_id,
        &cancel.execution_generation,
    ]
    .iter()
    .any(|value| value.trim().is_empty() || value.len() > 512)
        || cancel.reason.len() > 512
    {
        reject();
        return Ok(());
    }
    let payload = ComputerActionCancelPayload {
        request_id: model.request_id.clone(),
        connection_id: model.from_connection_id.clone(),
        approved_actor_id: actor.to_string(),
        cancel,
    };
    if ctx
        .worker_mgr
        .send_central_or_connection_worker(
            model.from_connection_id.as_deref(),
            ServiceToWorker::ComputerActionCancel(payload),
        )
        .await
        .is_err()
    {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "Computer Action stop target worker is unavailable or ambiguous",
        );
    }
    Ok(())
}

/// A rejection inside the action handler is a request-specific completion,
/// not an uncorrelated protocol Error. No worker has accepted the action.
fn emit_computer_action_rejected(ctx: &RouterContext, model: &SignalingModel, message: &str) {
    use desk_agent_protocol::computer_use::{
        ComputerActionCompleted, ComputerActionResultClass, SealedComputerActionPlan,
    };
    let reply = match model.get_data::<SealedComputerActionPlan>() {
        Ok(plan) => SignalingModel::success_response(
            &model.request_id,
            SignalingType::ComputerActionCompleted,
            None,
            model.from_connection_id.clone(),
            Some(&ComputerActionCompleted {
                work_id: plan.work_id,
                action_request_id: plan.action_request_id,
                execution_generation: plan.execution_generation,
                result: ComputerActionResultClass::DefinitelyNotStarted,
                facts: vec![],
                message: Some(message.to_string()),
                output: None,
            }),
        ),
        Err(_) => SignalingModel::error(
            &model.request_id,
            SignalingType::ComputerActionCompleted,
            None,
            model.from_connection_id.clone(),
            DeskErrorCode::PERMISSION_ERROR,
            "Invalid sealed Computer Action plan",
        ),
    };
    if let Ok(reply) = reply {
        if let Ok(text) = serde_json::to_string(&reply) {
            let _ = ctx.outbound_tx.send(text);
        }
    }
}
