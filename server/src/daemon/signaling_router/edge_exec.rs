use super::*;

/// Handle an inbound `EdgeExecRequest` from the manager. The frame has already
/// passed the proxy's source gate (Manager-only) and dedicated authz gate (which
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
        send_edge_exec_result(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                reason: "pep_rejected:missing_authorization".to_string(),
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
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: format!("pep_rejected:malformed_plan:{e}"),
                },
            );
            return Ok(());
        }
    };

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
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: "pep_rejected:execution_generation_mismatch".to_string(),
                },
            );
            return Ok(());
        }
        if plan.exec_request_id.0.is_empty() {
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: "pep_rejected:missing_exec_request_id".to_string(),
                },
            );
            return Ok(());
        }
        if plan.approval_id.0.is_empty() {
            send_edge_exec_result(
                &ctx.outbound_tx,
                &request_id,
                EdgeExecDisposition::RejectedBeforeDispatch {
                    reason: "pep_rejected:missing_approval_id".to_string(),
                },
            );
            return Ok(());
        }
    }

    // Exec must be runnable in this startup mode. The manager's pre-claim version
    // gate normally prevents dispatch to a daemon that cannot execute, but a PEP
    // must never assume the PDP got it right.
    if !ctx.exec_supported {
        send_edge_exec_result(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch {
                reason: "pep_rejected:exec_unsupported_in_mode".to_string(),
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
        } => validate_agentic_edge_exec(
            plan,
            validation_input,
            authz.max_risk,
            &templates,
            &effective_blocklist,
        ),
    };
    if let Some(reason) = rejection {
        log::warn!("[edge-exec] PEP rejected plan for request {request_id}: {reason}");
        send_edge_exec_result(
            &ctx.outbound_tx,
            &request_id,
            EdgeExecDisposition::RejectedBeforeDispatch { reason },
        );
        return Ok(());
    }

    // Drop the daemon-only `validation_input`: only the frozen `ExecPlan` argv
    // reaches the worker (the "worker never sees the command string" invariant).
    dispatch_fleet_exec_plan(ctx, &request_id, payload.into_plan()).await;
    Ok(())
}

/// Dispatch a PEP-validated fleet [`ExecPlan`] to the worker, correlated so the
/// worker's `WorkerToService::ExecResult` is relayed back to the manager as a
/// `EdgeExecResult(Executed{..})` (see the proxy's `ExecResult` handler). On a
/// send failure the plan never reached the worker, so the change definitely did
/// not run → `DispatchFailedBeforeWorker`.
pub(super) async fn dispatch_fleet_exec_plan(
    ctx: &RouterContext,
    request_id: &str,
    plan: ExecPlan,
) {
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
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::Executed { outcome },
            );
            return;
        }
        ExecAdmission::AcceptedOutcomeUnknown(reason) => {
            // `ExecutionStateUnknown` rather than a pre-dispatch variant: only the
            // pre-dispatch ones assert the change did not run, and this one cannot.
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::ExecutionStateUnknown { reason },
            );
            return;
        }
        ExecAdmission::Refused(reason) => {
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::RejectedBeforeDispatch { reason },
            );
            return;
        }
        ExecAdmission::AtCapacity(reason) => {
            send_edge_exec_result(
                &ctx.outbound_tx,
                request_id,
                EdgeExecDisposition::HostAtCapacity { reason },
            );
            return;
        }
    }

    // Register the in-flight correlation BEFORE sending so a fast worker reply
    // cannot race ahead of the marker.
    if let Ok(mut pending) = ctx.edge_exec_pending.lock() {
        pending.insert(request_id.to_string());
    }
    let payload = ExecPlanPayload {
        request_id: request_id.to_string(),
        // No browser connection: a fleet result is routed by `request_id`, not a
        // control-end connection id.
        connection_id: None,
        plan,
        audit_source_request_id: Some(request_id.to_string()),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ExecPlan(payload))
        .await
    {
        if let Ok(mut pending) = ctx.edge_exec_pending.lock() {
            pending.remove(request_id);
        }
        // Nothing was started, so the slot is free again immediately.
        ctx.exec_capacity.release(request_id);
        send_edge_exec_result(
            &ctx.outbound_tx,
            request_id,
            EdgeExecDisposition::DispatchFailedBeforeWorker {
                reason: format!("worker unavailable: {e}"),
            },
        );
    }
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

/// Route a control-end `AgentRequest`: two-phase parse → capability
/// derivation → authorization → trusted-field stamp → typed worker IPC.
pub(super) async fn handle_agent_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    // The AI read collectors expose host data beyond the remote view, so what
    // may leave this host is gated locally by the fail-closed collection policy
    // (`allow_logs` / `allow_screen`) and centrally by the authorization scope
    // below. Provider credentials live on the central brain, so there is no local
    // "gateway configured" gate here: an `AgentRequest` arrives already
    // authorized from the central link (or, off it, runs under the local read
    // scope).
    let Some(raw) = model.get_raw_data().as_ref() else {
        emit_agent_error(
            ctx,
            model,
            agent_error(
                AgentErrorKind::InvalidInput,
                "missing AgentRequest body",
                false,
                true,
            ),
        );
        return Ok(());
    };

    // Reject unknown kinds gracefully before typed parsing.
    if let Err(e) = validate_agent_request_kinds(raw) {
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
                    &format!("bad AgentRequest payload: {e}"),
                    false,
                    true,
                ),
            );
            return Ok(());
        }
    };

    // The `AgentRequest(600)` plane is **read-only, permanently**. Exec must go
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
    let payload = AgentRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: model.from_connection_id.clone(),
        envelope,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::AgentRequest(payload))
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
