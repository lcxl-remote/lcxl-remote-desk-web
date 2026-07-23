use super::*;

/// Route a control-end `ConfirmExec`: gate → classify → (on an executable
/// classification permitted by the current mode) park an immutable plan draft
/// and stream an `ExecPreview` with the minted `exec_request_id`; otherwise
/// stream a non-executable preview. Never executes — that needs an explicit
/// `ResolveExec(Approve)`.
pub(super) async fn handle_confirm_exec_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let to = model.from_connection_id.clone();
    let request_id = model.request_id.clone();

    let data = match model.get_data::<ConfirmExecData>() {
        Ok(d) => d,
        Err(e) => {
            // A malformed payload still has to leave a trace. The manager records
            // its own authorization of this frame, so a rejection that reported
            // nothing would read as a dispatch the host never acknowledged. This
            // is a protocol error rather than a capability decision — no capability
            // has been determined yet — so it is a task failure. The parser's
            // message may echo payload fragments and is deliberately not stored;
            // only the error kind is.
            ctx.audit
                .record(AuditEvent::task_failed_for_request(
                    new_audit_event_id(),
                    audit_now(),
                    &request_id,
                    &agent_error(
                        AgentErrorKind::InvalidInput,
                        "bad ConfirmExec payload",
                        false,
                        true,
                    ),
                ))
                .await;
            send_exec_preview(
                &ctx.outbound_tx,
                &request_id,
                to,
                non_executable_preview(
                    String::new(),
                    String::new(),
                    None,
                    0,
                    desk_agent_protocol::RiskLevel::High,
                    "Invalid request".to_string(),
                    Some(format!("bad ConfirmExec payload: {e}")),
                    None,
                ),
            );
            return Ok(());
        }
    };

    // The operation must be an exec; a read operation is a protocol error.
    let OperationInput::Exec(exec_input) = data.operation.input else {
        // Recorded for the same reason as the parse failure above: a protocol
        // error, not a capability decision.
        ctx.audit
            .record(AuditEvent::task_failed_for_request(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                &agent_error(
                    AgentErrorKind::InvalidInput,
                    "ConfirmExec requires an exec operation",
                    false,
                    true,
                ),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                String::new(),
                String::new(),
                None,
                0,
                desk_agent_protocol::RiskLevel::High,
                "Invalid request".to_string(),
                Some("ConfirmExec requires an exec operation".to_string()),
                None,
            ),
        );
        return Ok(());
    };

    let shell = exec_shell_label(&exec_input);
    let command = exec_input.command.clone();
    let cwd = exec_input.cwd.clone();
    let limits = crate::exec::ExecLimits::clamped(&exec_input);

    // Gate: confirmed execution is unavailable in ServiceDaemon mode.
    if !ctx.exec_supported {
        // Unlike the two protocol errors above, this is a genuine capability
        // refusal: the request was well-formed and the host is declining the
        // capability outright. The risk is unknown here — classification has not
        // run — so the ceiling is recorded rather than a computed value.
        ctx.audit
            .record(AuditEvent::capability_denied(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                risk_str(desk_agent_protocol::RiskLevel::High),
                "exec unsupported in this startup mode".to_string(),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                shell,
                command,
                cwd,
                limits.timeout_ms,
                desk_agent_protocol::RiskLevel::High,
                "AI command execution is not available in this mode".to_string(),
                Some("unsupported in this startup mode".to_string()),
                None,
            ),
        );
        return Ok(());
    }

    // The execution mode is the device owner's local ceiling on AI action.
    // Provider credentials live on the central brain, so there is no local
    // "gateway configured" gate here: confirmed execution is gated by worker
    // support (above) and central authorization (the PDP checks below). On a
    // central link the policy decision's mode applies but the local mode is an
    // upper bound — the local setting can narrow a centrally issued authorization,
    // never widen it (a SuggestOnly / ReadOnly local config caps a broad central
    // grant). Off that link the local mode applies directly.
    let execution_mode = {
        let s = ctx.settings.read().await;
        match &ctx.inbound_authz {
            Some(authz) => authz.scope.mode.restrict_to(s.ai_policy.execution_mode),
            None => s.ai_policy.execution_mode,
        }
    };

    // Classify against the built-in baseline unioned with the operator
    // templates synced from the manager (empty on single-machine links), using
    // the effective blocklist (built-in floor on single-machine / unsynced links,
    // the manager's built-in-minus-disabled ∪ custom set on a fleet link).
    let operator_templates = ctx.command_templates.snapshot();
    let effective_blocklist = ctx.command_blocklist.snapshot();
    let outcome = crate::exec::classify_command_with_all(
        &exec_input,
        &operator_templates,
        &effective_blocklist,
    );
    let classification = outcome.classification;

    // Fleet PDP risk ceiling (manager link): refuse a command whose classified
    // risk exceeds the policy's `max_risk`, regardless of execution mode.
    if let Some(authz) = &ctx.inbound_authz
        && classification.risk > authz.max_risk
    {
        ctx.audit
            .record(AuditEvent::capability_denied(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                risk_str(classification.risk),
                classification.impact.clone(),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                shell,
                command,
                cwd,
                limits.timeout_ms,
                classification.risk,
                "command exceeds the policy risk ceiling".to_string(),
                Some("blocked by policy max_risk".to_string()),
                None,
            ),
        );
        return Ok(());
    }

    // Fleet PDP capability gate (manager link): the command's required exec
    // capability — the `shell.exec.readonly` vs `shell.exec.confirmed` split
    // decided by the server-side classification — must be in the policy-granted
    // scope. This mirrors the AgentRequest read path: a policy that grants only
    // `shell.exec.readonly` must not run a mutating command even when the mode
    // and `max_risk` would otherwise allow it. Without a manager authorization
    // (single-machine / remote-signaling) the local mode / template gating is
    // the authority, so the check is skipped.
    if let Some(authz) = &ctx.inbound_authz
        && let Some(required) = OperationInput::required_capability(&classification)
        && !authorize(required, &authz.scope.granted)
    {
        ctx.audit
            .record(AuditEvent::capability_denied(
                new_audit_event_id(),
                audit_now(),
                &request_id,
                risk_str(classification.risk),
                classification.impact.clone(),
            ))
            .await;
        send_exec_preview(
            &ctx.outbound_tx,
            &request_id,
            to,
            non_executable_preview(
                shell,
                command,
                cwd,
                limits.timeout_ms,
                classification.risk,
                "command requires a capability the policy does not grant".to_string(),
                Some("blocked by policy scope".to_string()),
                None,
            ),
        );
        return Ok(());
    }

    // Decide executability from the classification + the active execution mode.
    let mode_note = match (
        classification.decision,
        classification.effect,
        execution_mode,
    ) {
        (ExecDecision::Blocked, _, _) => {
            ctx.audit
                .record(AuditEvent::capability_denied(
                    new_audit_event_id(),
                    audit_now(),
                    &request_id,
                    risk_str(classification.risk),
                    classification.impact.clone(),
                ))
                .await;
            send_exec_preview(
                &ctx.outbound_tx,
                &request_id,
                to,
                non_executable_preview(
                    shell,
                    command,
                    cwd,
                    limits.timeout_ms,
                    classification.risk,
                    classification.impact.clone(),
                    None,
                    Some(classification.impact),
                ),
            );
            return Ok(());
        }
        (ExecDecision::NotExecutable, _, _) => {
            Some("command does not match a safe template; run it manually instead".to_string())
        }
        (ExecDecision::ConfirmRequired, _, ExecutionMode::SuggestOnly) => {
            Some("AI command execution is disabled (suggest-only mode)".to_string())
        }
        (ExecDecision::ConfirmRequired, Some(ExecEffect::Mutating), ExecutionMode::ReadOnly) => {
            Some("read-only mode does not permit state-changing commands".to_string())
        }
        // SessionApproved executes like ConfirmEachAction, except the first
        // confirmation of a given template grants it for the rest of the
        // session (handled below). Automated (run without any confirmation)
        // is not implemented.
        (ExecDecision::ConfirmRequired, _, ExecutionMode::Automated) => {
            Some("execution mode not available".to_string())
        }
        (ExecDecision::ConfirmRequired, _, _) => None, // executable
    };

    // Executable iff the classification is ConfirmRequired and the mode allows
    // it (no `mode_note` was produced) and a draft was rendered.
    if mode_note.is_none()
        && classification.decision == ExecDecision::ConfirmRequired
        && let Some(draft) = outcome.draft
    {
        let capability = OperationInput::required_capability(&classification).map(|c| c.as_str());
        let risk = classification.risk;

        // On a manager link the ConfirmExec frame request_id is the PDP's
        // authorization-ledger key; carry it through the whole exec lifecycle so
        // every audit event (here and on the later ResolveExec / worker-result
        // paths) can be attributed to the real operator. Single-machine /
        // remote-signaling links have no ledger, so this stays None and the
        // audit `task_id` is unchanged.
        let audit_source = ctx.inbound_authz.as_ref().map(|_| request_id.clone());

        // SessionApproved grant eligibility: the active mode is SessionApproved,
        // the command matched a template (intersect with the whitelist — only
        // an already-executable template is ever granted), and the request came
        // over a connection we can key the grant to.
        let session_template = (execution_mode == ExecutionMode::SessionApproved)
            .then(|| classification.matched_template.clone())
            .flatten();
        let connection_id = model.from_connection_id.clone();

        // Already granted this session → auto-execute without re-prompting.
        if let (Some(template_id), Some(conn)) = (session_template.as_ref(), connection_id.as_ref())
            && ctx.session_approvals.is_granted(conn, template_id)
        {
            let exec_request_id = crate::daemon::exec_approval::mint_exec_request_id();
            // The frame that triggers execution is this ConfirmExec itself: the
            // session grant means no separate approval round happens.
            let (approval_id, plan) = crate::daemon::exec_approval::seal_plan(
                exec_request_id.clone(),
                &request_id,
                draft,
            );
            // No new approval prompt; the prior session grant authorizes it.
            ctx.audit
                .record(
                    AuditEvent::capability_allowed(
                        new_audit_event_id(),
                        audit_now(),
                        &exec_request_id.0,
                        capability,
                        risk_str(risk),
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            ctx.audit
                .record(
                    AuditEvent::command_executed(
                        new_audit_event_id(),
                        audit_now(),
                        &exec_request_id.0,
                        &approval_id.0,
                        capability,
                        risk_str(risk),
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            // Informational preview (no confirmation) so the control end can
            // show what ran; the result follows as an `ExecResult`.
            let preview = ExecPreview {
                exec_request_id: Some(exec_request_id),
                shell,
                command,
                cwd,
                timeout_ms: limits.timeout_ms,
                risk: classification.risk,
                impact: classification.impact,
                policy_note: classification
                    .matched_template
                    .map(|t| format!("session-approved template {t}")),
                requires_confirmation: false,
                executable: true,
                blocked_reason: None,
            };
            send_exec_preview(&ctx.outbound_tx, &request_id, to.clone(), preview);
            dispatch_exec_plan(ctx, &request_id, to, plan, audit_source).await;
            return Ok(());
        }

        // Not yet granted (or not session-approved mode) → park and prompt.
        // `session_template` (when present) is carried so that approving this
        // preview grants the template for the rest of the session.
        let exec_request_id = ctx.exec_approvals.insert(
            draft,
            classification.clone(),
            connection_id,
            session_template,
            audit_source.clone(),
        );
        ctx.audit
            .record(
                AuditEvent::capability_requested(
                    new_audit_event_id(),
                    audit_now(),
                    &exec_request_id.0,
                    capability,
                    risk_str(risk),
                    classification.impact.clone(),
                )
                .with_task_id(audit_source.as_deref()),
            )
            .await;
        let preview = ExecPreview {
            exec_request_id: Some(exec_request_id),
            shell,
            command,
            cwd,
            timeout_ms: limits.timeout_ms,
            risk: classification.risk,
            impact: classification.impact,
            policy_note: classification
                .matched_template
                .map(|t| format!("matched template {t}")),
            requires_confirmation: true,
            executable: true,
            blocked_reason: None,
        };
        send_exec_preview(&ctx.outbound_tx, &request_id, to, preview);
        return Ok(());
    }

    // Not executable under the current mode / classification.
    ctx.audit
        .record(AuditEvent::capability_denied(
            new_audit_event_id(),
            audit_now(),
            &request_id,
            risk_str(classification.risk),
            mode_note
                .clone()
                .unwrap_or_else(|| classification.impact.clone()),
        ))
        .await;
    send_exec_preview(
        &ctx.outbound_tx,
        &request_id,
        to,
        non_executable_preview(
            shell,
            command,
            cwd,
            limits.timeout_ms,
            classification.risk,
            classification.impact,
            mode_note,
            None,
        ),
    );
    Ok(())
}

/// Route a control-end `ResolveExec`: consume the pending approval (once) and,
/// on approve, seal the stored draft into an `ExecPlan` and dispatch it. Reject
/// just consumes the pending and ends. A missing / expired / already-consumed id
/// on approve returns an error `ExecResult`.
/// Handle an `ExecControl(623)`: stop an execution, or report on one.
///
/// Both actions answer with the same `ExecStateReply(624)` built from the durable
/// ledger. The ledger is asked *after* a cancel has been passed to the worker so
/// the reply reflects the request, and a generation the worker is not running is
/// not an error — it has very likely just finished, and the ledger says so.
pub(super) async fn handle_exec_control_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request_id = model.request_id.clone();
    let to = model.from_connection_id.clone();

    let payload = match model.get_data::<ExecControlPayload>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[router] bad ExecControl payload: {e} (request_id={request_id})");
            return Ok(());
        }
    };
    let generation = payload.execution_generation.clone();

    if let ExecControlAction::Cancel { requested_by } = &payload.action {
        log::info!(
            "[router] exec cancel requested: generation={generation} by={requested_by} \
             (request_id={request_id})"
        );
        // Best-effort by design: the worker may be gone, or the command may have
        // just finished. Either way the ledger below reports what is actually
        // true, rather than this send's success standing in for it.
        if let Err(e) = ctx
            .worker_mgr
            .send_to_worker(ServiceToWorker::ExecCancel(ExecCancelPayload {
                execution_generation: generation.clone(),
            }))
            .await
        {
            log::warn!("[router] could not pass the cancel to the worker: {e}");
        }
        // `requested_by` is a wire hint only; the audit pipeline stamps the
        // authenticated actor, so a control end cannot name someone else as the
        // one who stopped a command.
        ctx.audit
            .record(AuditEvent::command_cancel_requested(
                new_audit_event_id(),
                audit_now(),
                &generation,
            ))
            .await;
    }

    let reply = match ctx.exec_ledger.describe(&generation).await {
        Ok(reply) => reply,
        Err(e) => {
            log::warn!("[router] could not read the exec ledger: {e} (generation={generation})");
            return Ok(());
        }
    };

    send_notification(
        &ctx.outbound_tx,
        &request_id,
        SignalingType::ExecStateReply,
        to,
        &reply,
        "ExecStateReply",
    );
    Ok(())
}

pub(super) async fn handle_resolve_exec_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request_id = model.request_id.clone();
    let to = model.from_connection_id.clone();

    let data = match model.get_data::<ResolveExecData>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("[router] bad ResolveExec payload: {e} (request_id={request_id})");
            return Ok(());
        }
    };

    use crate::daemon::exec_approval::TakeOutcome;
    use desk_agent_protocol::exec::ApprovalDecision;

    // Agentic (model-initiated) exec: the loop is awaiting this decision through the
    // coordinator. If it matches, deliver the decision and stop — the agentic seam
    // drives dispatch itself, so it must not also run the browser-initiated
    // park/consume flow below. The command's completion is audited on the worker
    // `ExecResult` round-trip (the `command_completed` event); approval-event audit
    // parity with the browser flow is a follow-up (the manager runtime records the
    // full approval lifecycle in its durable work-item audit).
    if ctx.agentic_exec.resolve_approval(
        &data.exec_request_id.0,
        matches!(data.decision, ApprovalDecision::Approve),
    ) {
        return Ok(());
    }

    // Approve / reject are bound to the connection that requested the preview.
    let outcome = ctx
        .exec_approvals
        .take(&data.exec_request_id, to.as_deref());
    match data.decision {
        ApprovalDecision::Reject => {
            match outcome {
                TakeOutcome::Consumed(consumed) => {
                    // Consumed so it cannot be approved later; the control end
                    // already updated its UI, so no result frame is sent. Carry
                    // the source ConfirmExec frame id (stored at park time) so the
                    // rejection is attributed to the real operator on a manager
                    // link, not the reporting host's token owner.
                    ctx.audit
                        .record(
                            AuditEvent::approval_denied(
                                new_audit_event_id(),
                                audit_now(),
                                &data.exec_request_id.0,
                            )
                            .with_task_id(consumed.source_request_id.as_deref()),
                        )
                        .await;
                }
                TakeOutcome::Forbidden => {
                    log::warn!(
                        "[router] ResolveExec(Reject) from a non-owning connection, ignored \
                         (exec_request_id={})",
                        data.exec_request_id.0
                    );
                }
                TakeOutcome::NotFound => {}
            }
            Ok(())
        }
        ApprovalDecision::Approve => {
            let consumed = match outcome {
                TakeOutcome::Consumed(c) => c,
                // Unknown/expired and cross-connection both return the same
                // generic error (do not leak whether the id exists).
                other => {
                    if matches!(other, TakeOutcome::Forbidden) {
                        log::warn!(
                            "[router] ResolveExec(Approve) from a non-owning connection, denied \
                             (exec_request_id={})",
                            data.exec_request_id.0
                        );
                    }
                    send_exec_result(
                        &ctx.outbound_tx,
                        &request_id,
                        to,
                        ExecResultPayload {
                            exec_request_id: data.exec_request_id,
                            outcome: AgentOutcome::Err(agent_error(
                                AgentErrorKind::InvalidInput,
                                "approval expired or already used",
                                false,
                                true,
                            )),
                        },
                    );
                    return Ok(());
                }
            };

            let capability =
                OperationInput::required_capability(&consumed.classification).map(|c| c.as_str());
            let risk = risk_str(consumed.classification.risk);
            // The ResolveExec frame carrying the approval is what triggers this
            // dispatch, so it is the generation; the ConfirmExec that produced the
            // preview only classified it.
            let (approval_id, plan) = crate::daemon::exec_approval::seal_plan(
                data.exec_request_id.clone(),
                &request_id,
                consumed.draft,
            );
            // Approval granted → capability allowed → command dispatched.
            // ResolveExec is not PDP-wrapped, so the operator ledger key comes
            // from the source ConfirmExec frame request_id stored at park time.
            let xr = data.exec_request_id.0.clone();
            let audit_source = consumed.source_request_id.clone();
            ctx.audit
                .record(
                    AuditEvent::approval_granted(
                        new_audit_event_id(),
                        audit_now(),
                        &xr,
                        &approval_id.0,
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            ctx.audit
                .record(
                    AuditEvent::capability_allowed(
                        new_audit_event_id(),
                        audit_now(),
                        &xr,
                        capability,
                        risk,
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            ctx.audit
                .record(
                    AuditEvent::command_executed(
                        new_audit_event_id(),
                        audit_now(),
                        &xr,
                        &approval_id.0,
                        capability,
                        risk,
                    )
                    .with_task_id(audit_source.as_deref()),
                )
                .await;
            // In SessionApproved mode, approving the first preview of a
            // template grants it for the rest of this connection's session, so
            // subsequent matching commands skip confirmation. The grant is
            // keyed to the connection that requested the preview.
            if let (Some(template_id), Some(conn)) = (
                consumed.session_grant_template.as_ref(),
                consumed.connection_id.as_ref(),
            ) {
                ctx.session_approvals.grant(conn, template_id);
            }
            let result_to = consumed.connection_id.or(to);
            dispatch_exec_plan(ctx, &request_id, result_to, plan, audit_source).await;
            Ok(())
        }
    }
}

/// Dispatch a sealed [`ExecPlan`] to the worker for execution. The worker runs
/// the argv verbatim and replies with `WorkerToService::ExecResult`, which the
/// signaling proxy turns into the outbound `ExecResult(609)` frame. The
/// `request_id` / `connection_id` are echoed through so the proxy can route that
/// frame back. If the worker is unreachable, synthesize an error result here so
/// the control end still gets a definite answer.
pub(super) async fn dispatch_exec_plan(
    ctx: &RouterContext,
    request_id: &str,
    to_connection_id: Option<String>,
    plan: desk_agent_protocol::exec::ExecPlan,
    audit_source_request_id: Option<String>,
) {
    let exec_request_id = plan.exec_request_id.clone();
    let plan_generation = plan.execution_generation.clone();

    // Claim this dispatch in the ledger before the worker can start anything. A
    // redelivered frame is answered from the record instead of run a second time.
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
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    outcome,
                },
            );
            return;
        }
        ExecAdmission::AcceptedOutcomeUnknown(reason) => {
            // Deliberately not an error that reads as "did not run": the change may
            // already have happened, and saying otherwise would invite a retry of it.
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    outcome: AgentOutcome::Err(agent_error(
                        AgentErrorKind::Internal,
                        &reason,
                        false,
                        true,
                    )),
                },
            );
            return;
        }
        ExecAdmission::Refused(reason) => {
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    outcome: AgentOutcome::Err(agent_error(
                        AgentErrorKind::PermissionDenied,
                        &reason,
                        false,
                        true,
                    )),
                },
            );
            return;
        }
        ExecAdmission::AtCapacity(reason) => {
            send_exec_result(
                &ctx.outbound_tx,
                request_id,
                to_connection_id,
                ExecResultPayload {
                    exec_request_id,
                    // Retryable: nothing ran, and the ceiling frees up.
                    outcome: AgentOutcome::Err(agent_error(
                        AgentErrorKind::HostAtCapacity,
                        &reason,
                        true,
                        true,
                    )),
                },
            );
            return;
        }
    }

    let payload = ExecPlanPayload {
        request_id: request_id.to_string(),
        connection_id: to_connection_id.clone(),
        plan,
        audit_source_request_id,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ExecPlan(payload))
        .await
    {
        // Nothing was started, so the slot is free again immediately.
        ctx.exec_capacity.release(&plan_generation);
        send_exec_result(
            &ctx.outbound_tx,
            request_id,
            to_connection_id,
            ExecResultPayload {
                exec_request_id,
                outcome: AgentOutcome::Err(agent_error(
                    AgentErrorKind::TargetOffline,
                    &format!("worker unavailable: {e}"),
                    true,
                    true,
                )),
            },
        );
    }
}

/// Send a `EdgeExecResult(614)` toward the manager as a notification-style
/// frame, correlated by the per-attempt `request_id`. Used both for the early
/// PEP rejections (synthesized here) and for the worker's completed result
/// (relayed by the signaling proxy).
pub(crate) fn send_edge_exec_result(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    disposition: EdgeExecDisposition,
) {
    let payload = EdgeExecResultPayload {
        request_id: request_id.to_string(),
        disposition,
    };
    send_notification(
        outbound_tx,
        request_id,
        SignalingType::EdgeExecResult,
        None,
        &payload,
        "EdgeExecResult",
    );
}

/// Whether the sealed plan's own render matches the [`ExecPlanDraft`] a validator
/// authoritatively reconstructed. Compares every executable field (the ids and
/// approval token are not on the draft). Shared by the fleet and agentic PEP paths.
pub(super) fn plan_matches_draft(
    plan: &ExecPlan,
    expected: &desk_agent_protocol::exec::ExecPlanDraft,
) -> bool {
    expected.program == plan.program
        && expected.argv == plan.argv
        && expected.risk == plan.risk
        && expected.shell == plan.shell
        && expected.cwd == plan.cwd
        && expected.template_id == plan.template_id
        && expected.timeout_ms == plan.timeout_ms
        && expected.max_stdout_bytes == plan.max_stdout_bytes
        && expected.max_stderr_bytes == plan.max_stderr_bytes
        && expected.fingerprint == plan.fingerprint
}

/// Source-agnostic PEP checks that every sealed [`ExecPlan`] must pass regardless
/// of how it was rendered: the effective blocklist over the full argv, and the
/// `risk <= max_risk` ceiling. The template-reproduction check differs by source
/// and lives in the per-source validators.
pub(super) fn pep_common_checks(
    plan: &ExecPlan,
    max_risk: desk_agent_protocol::RiskLevel,
    blocklist: &[desk_agent_protocol::command_blocklist::BlocklistRule],
) -> Option<String> {
    // The blocklist operates over the full argv (program is `argv[0]`), matched
    // against the effective set (built-in floor on an unsynced link, the manager's
    // built-in-minus-disabled ∪ custom set on a fleet link) — never a second
    // compiled-in pass, so an admin-disabled rule is genuinely gone here too.
    let full_argv: Vec<String> = std::iter::once(plan.program.clone())
        .chain(plan.argv.iter().cloned())
        .collect();
    let lc = full_argv.join(" ").to_ascii_lowercase();
    if let Some(rule) = desk_agent_protocol::command_blocklist::blocklist_match(blocklist, &lc) {
        return Some(format!("pep_rejected:blocklist:{rule}"));
    }

    // max_risk ceiling (independent of the manager's per-device decision).
    if plan.risk > max_risk {
        return Some(format!(
            "pep_rejected:risk_exceeds_max:{:?}>{:?}",
            plan.risk, max_risk
        ));
    }

    // Enforcement-tier fail-closed: a plan that demands native-hard containment must
    // not spawn on a host that can only provide the baseline tier. This runs before
    // dispatch (the reason surfaces as RejectedBeforeDispatch), so the host never
    // silently downgrades — the manager only ever learns the command ran under the
    // tier it required, or that it was refused.
    if plan.containment.required_enforcement
        == desk_agent_protocol::exec::RequiredEnforcement::NativeHard
        && !crate::worker::exec_containment::provides_native_hard()
    {
        return Some("pep_rejected:native_hard_unavailable".to_string());
    }

    None
}

/// Re-validate a manager-sealed **fleet** [`ExecPlan`] against this daemon's own
/// view (defense in depth — the manager draft is never trusted). Returns the
/// model-safe rejection reason on the first failure, or `None` when the plan
/// passes. Order: common checks (blocklist, risk ceiling) → exact-argv whitelist
/// + fingerprint.
///
/// Fleet exec has no per-request limit input, so the authoritative render is
/// always the fixed fleet defaults with no cwd (identical to the sealing side in
/// `fleet_approval::verify_template_unchanged`). The plan's own `cwd` /
/// `timeout_ms` / output caps are therefore compared *against those authoritative
/// values* — never fed back into the expected render. If the render used the
/// plan's own limits, a tampered limit would be hashed into both sides and the
/// fingerprint would still agree (self-consistent tamper); rebuilding from the
/// fixed authority is what makes a widened timeout / output cap detectable. A
/// `template_id` is unique only per-org, so several synced **operator** templates
/// can share it; try every candidate and accept if any one reproduces the plan
/// exactly. This path never consults the built-in templates — a fleet plan must
/// come from an operator template.
pub(super) fn validate_fleet_edge_exec(
    plan: &ExecPlan,
    max_risk: desk_agent_protocol::RiskLevel,
    templates: &[desk_agent_protocol::command_template::SyncedCommandTemplate],
    blocklist: &[desk_agent_protocol::command_blocklist::BlocklistRule],
) -> Option<String> {
    if let Some(reason) = pep_common_checks(plan, max_risk, blocklist) {
        return Some(reason);
    }

    let mut saw_candidate = false;
    let mut faithful = false;
    for template in templates
        .iter()
        .filter(|t| t.template_id == plan.template_id)
    {
        saw_candidate = true;
        let expected = build_exact_argv_draft(
            template,
            None,
            DEFAULT_OUTPUT_BYTES,
            DEFAULT_OUTPUT_BYTES,
            None,
        );
        if plan_matches_draft(plan, &expected) {
            faithful = true;
            break;
        }
    }
    if !faithful {
        return Some(if saw_candidate {
            "pep_rejected:template_drift".to_string()
        } else {
            "pep_rejected:template_not_in_allowlist".to_string()
        });
    }

    None
}

/// Re-validate a manager-sealed **agentic** [`ExecPlan`] by re-running the shared
/// command classifier over the daemon-only `validation_input` envelope. Returns
/// the model-safe rejection reason on the first failure, or `None` when the plan
/// passes. Order: common checks (blocklist, risk ceiling) → re-classification.
///
/// The agentic plan was sealed at the manager from a per-turn classification of
/// this exact input (built-in **or** operator template, clamped per-turn limits +
/// the input's cwd), which the fixed fleet render cannot reproduce. So instead of
/// re-rendering a template with fleet defaults, the daemon feeds `validation_input`
/// back through [`classify_command_with_all`] with its own operator snapshot and
/// effective blocklist — the same function, the same tables the manager used — and
/// requires the result to be `ConfirmRequired` with a draft that reproduces the
/// sealed plan field-for-field. This naturally covers both the built-in and
/// operator template families and the per-turn clamped limits / cwd, and it makes
/// an in-bounds limit tamper detectable: the classifier re-derives the limits from
/// the input, so a plan whose limits were altered away from what the input yields
/// no longer matches.
///
/// Honest boundary: this defeats a tamper that alters only the sealed plan's
/// **executable / classification draft fields** (program, argv, cwd, shell, risk,
/// template_id, limits, fingerprint) — a self-consistent forgery of what would run.
/// It does not by itself vouch for the two id fields that are not on the draft
/// (`exec_request_id`, `approval_id`); those are bound separately in
/// [`handle_edge_exec_request_inbound`] (frame-id match + non-empty approval token),
/// and their values remain transport-trusted manager metadata, not an independent
/// cryptographic commitment. Nor is it a commitment against a manager that alters
/// `validation_input` and `plan` in lockstep — that is the same trust level as the
/// fleet path, where the manager is the PDP.
pub(super) fn validate_agentic_edge_exec(
    plan: &ExecPlan,
    validation_input: &desk_agent_protocol::ExecInput,
    max_risk: desk_agent_protocol::RiskLevel,
    templates: &[desk_agent_protocol::command_template::SyncedCommandTemplate],
    blocklist: &[desk_agent_protocol::command_blocklist::BlocklistRule],
) -> Option<String> {
    if let Some(reason) = pep_common_checks(plan, max_risk, blocklist) {
        return Some(reason);
    }

    let outcome = desk_diagnose_core::exec_classify::classify_command_with_all(
        validation_input,
        templates,
        blocklist,
    );
    if outcome.classification.decision != ExecDecision::ConfirmRequired {
        return Some("pep_rejected:agentic_not_executable".to_string());
    }
    let Some(expected) = outcome.draft else {
        return Some("pep_rejected:agentic_no_draft".to_string());
    };
    if !plan_matches_draft(plan, &expected) {
        return Some("pep_rejected:agentic_reclassify_drift".to_string());
    }

    None
}
