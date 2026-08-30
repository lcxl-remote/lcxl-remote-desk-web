use super::*;

pub(super) fn agent_error(
    kind: AgentErrorKind,
    message: &str,
    retryable: bool,
    safe_for_model: bool,
) -> AgentError {
    AgentError {
        kind,
        message: message.to_string(),
        retryable,
        safe_for_model,
        error_code: None,
    }
}

/// Two-phase unknown-kind validation over the raw `AgentRequestData`
/// JSON. Runs **before** the typed `from_value` so an unknown kind
/// surfaces as a structured `UnsupportedCapability` rather than a serde
/// parse error (which would arrive too late to build a graceful
/// outcome). Descends both the outer `operation.input.kind` and — for
/// `read_context` — the inner `operation.input.params.kind.kind`.
pub(super) fn validate_invoke_agent_capability_kinds(
    raw: &serde_json::Value,
) -> Result<(), AgentError> {
    let outer = raw
        .get("operation")
        .and_then(|o| o.get("input"))
        .and_then(|i| i.get("kind"))
        .and_then(|k| k.as_str());
    let Some(outer) = outer else {
        return Err(agent_error(
            AgentErrorKind::InvalidInput,
            "missing operation.input.kind",
            false,
            true,
        ));
    };
    if !SUPPORTED_OPERATION_KINDS.contains(&outer) {
        return Err(agent_error(
            AgentErrorKind::UnsupportedCapability,
            &format!("unsupported operation kind '{outer}'"),
            false,
            true,
        ));
    }
    if outer == "read_context" {
        let inner = raw
            .get("operation")
            .and_then(|o| o.get("input"))
            .and_then(|i| i.get("params"))
            .and_then(|p| p.get("kind"))
            .and_then(|k| k.get("kind"))
            .and_then(|k| k.as_str());
        let Some(inner) = inner else {
            return Err(agent_error(
                AgentErrorKind::InvalidInput,
                "missing operation.input.params.kind.kind",
                false,
                true,
            ));
        };
        if !SUPPORTED_READ_KINDS.contains(&inner) {
            return Err(agent_error(
                AgentErrorKind::UnsupportedCapability,
                &format!("unsupported read kind '{inner}'"),
                false,
                true,
            ));
        }
    }
    Ok(())
}

/// Server-computed grant for the single-machine read path. This deployment
/// grants the full supported read set in `ReadOnly` mode. The authorization
/// mechanism ([`authorize`]) is still exercised so configured scopes can
/// narrow `granted`.
pub(super) fn default_read_scope() -> AgentScope {
    AgentScope {
        granted: vec![
            Capability::SystemInfo,
            Capability::ProcessList,
            Capability::NetworkPorts,
            Capability::ServiceStatus,
            Capability::LogRecent,
            Capability::ContainerList,
            Capability::ContainerInspect,
            Capability::ContainerLogs,
            Capability::ScreenCaptureCurrent,
            Capability::DesktopSessionInspect,
            Capability::DesktopUiInspect,
            Capability::OfficeDocumentInspect,
        ],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    }
}

/// Whether `capability` is covered by the granted set. Pure so the
/// `PermissionDenied` path is unit-testable without a live router.
pub(super) fn authorize(capability: Capability, granted: &[Capability]) -> bool {
    granted.contains(&capability)
}

/// Server-injected actor. Never sourced from the control end (which
/// structurally cannot express it — `AgentRequestData` carries no actor
/// field). The single-machine path has no session identity plumbed into the
/// router, so the local operator is represented as a `System` actor;
/// fleet / authenticated paths will inject the real principal here.
pub(super) fn server_actor() -> ActorRef {
    ActorRef {
        actor_type: ActorType::System,
        actor_id: "local-operator".to_string(),
    }
}

/// Emit an `AgentCapabilityCompleted(AgentOutcome::Err)` back to the control end.
/// Business / capability-level failures ride the `signaling_data`
/// `AgentOutcome`, not `SignalingResponseState`, so the
/// control-end UI receives the full structured error. Build / serialise
/// failures are non-fatal — log + drop.
pub(super) fn emit_agent_error(ctx: &RouterContext, model: &SignalingModel, error: AgentError) {
    let outcome = AgentOutcome::Err(error);
    match SignalingModel::success_response(
        &model.request_id,
        SignalingType::AgentCapabilityCompleted,
        None,
        model.from_connection_id.clone(),
        Some(&outcome),
    ) {
        Ok(reply) => match serde_json::to_string(&reply) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(e) => log::warn!(
                "[router] failed to serialise AgentCapabilityCompleted error: {e} (request_id={})",
                model.request_id,
            ),
        },
        Err(e) => log::warn!(
            "[router] failed to build AgentCapabilityCompleted error: {e} (request_id={})",
            model.request_id,
        ),
    }
}

/// Route a control-end `TerminalCopilotAsk`. The terminal copilot is orchestrated
/// by the central signaling brain (signal / manager): the control end sends the
/// ask — carrying the terminal context inline — to the central server, which dials
/// the model and streams `TerminalCopilotEvent` frames back. This host runs no
/// local copilot. If an ask still reaches the edge router (a link without a central
/// brain), answer with one terminal `TerminalCopilotEvent::error` so the control
/// end stops waiting on the stream.
pub(super) async fn handle_terminal_copilot_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let mut sink = copilot_signaling_sink(
        ctx.outbound_tx.clone(),
        model.from_connection_id.clone(),
        model.request_id.clone(),
    );
    sink.emit_error(agent_error(
        AgentErrorKind::UnsupportedCapability,
        "the terminal copilot is handled by the central signaling server",
        false,
        true,
    ));
    Ok(())
}

/// Route a control-end `TerminalCompleteAsk`. Inline command completion is
/// orchestrated centrally too: the central server dials the model over the inline
/// terminal context the control end supplies; the edge runs none locally. If an
/// ask still reaches the edge router, answer with one error `TerminalCompleteResult`
/// so the control end always gets a response.
pub(super) async fn handle_terminal_complete_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::terminal_complete::TerminalCompleteResult;
    crate::terminal_complete::send_completion_result(
        &ctx.outbound_tx,
        model.from_connection_id.clone(),
        &TerminalCompleteResult::failed(
            &model.request_id,
            agent_error(
                AgentErrorKind::UnsupportedCapability,
                "terminal command completion is handled by the central signaling server",
                false,
                true,
            ),
        ),
    );
    Ok(())
}

/// Send an `ExecPreview(606)` to the control end as a notification-style frame
/// (`response_state = None`), mirroring `send_diagnose_frame`. Build / serialise
/// failures are non-fatal — log + drop.
pub(crate) fn send_exec_preview(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    to_connection_id: Option<String>,
    preview: ExecPreview,
) {
    send_notification(
        outbound_tx,
        request_id,
        SignalingType::ExecutionPreviewGenerated,
        to_connection_id,
        &preview,
        "ExecPreview",
    );
}

/// Send an `ExecutionCompleted(609)` to the control end as a notification-style frame.
pub(super) fn send_execution_completed(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    to_connection_id: Option<String>,
    payload: ExecResultPayload,
) {
    send_notification(
        outbound_tx,
        request_id,
        SignalingType::ExecutionCompleted,
        to_connection_id,
        &payload,
        "ExecutionCompleted",
    );
}

/// Shared notification-frame sender for the exec plane. `response_state = None`
/// so the control end treats each frame as an event, not a one-shot response.
pub(super) fn send_notification<T: serde::Serialize>(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    signaling_type: SignalingType,
    to_connection_id: Option<String>,
    data: &T,
    label: &str,
) {
    let value = match serde_json::to_value(data) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[router] failed to serialise {label}: {e} (request_id={request_id})");
            return;
        }
    };
    let frame = SignalingModel::new(
        request_id,
        signaling_type,
        None,
        to_connection_id,
        Some(value),
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = outbound_tx.send(text);
        }
        Err(e) => {
            log::warn!("[router] failed to serialise {label} frame: {e} (request_id={request_id})")
        }
    }
}

/// Build a non-executable [`ExecPreview`] (blocked / off-template / mode-denied /
/// gate-denied). No pending approval is created.
pub(super) fn non_executable_preview(
    shell: String,
    command: String,
    cwd: Option<String>,
    timeout_ms: u32,
    risk: desk_agent_protocol::RiskLevel,
    blocked_reason: Option<String>,
) -> ExecPreview {
    ExecPreview {
        exec_request_id: None,
        shell,
        command,
        cwd,
        approval_timeout_ms: 0,
        timeout_ms,
        risk,
        execution_basis: desk_agent_protocol::exec::ExecExecutionBasis::Template,
        principal: desk_agent_protocol::exec::ExecutionPrincipal::SessionUser,
        requires_confirmation: false,
        executable: false,
        blocked_reason,
    }
}

/// Extract the shell label from an exec target (empty for a non-shell target).
pub(super) fn exec_shell_label(input: &desk_agent_protocol::ExecInput) -> String {
    match &input.target {
        desk_agent_protocol::ExecTarget::Shell { shell } => shell.clone(),
        desk_agent_protocol::ExecTarget::Domain { .. } => String::new(),
    }
}
