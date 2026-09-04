//! Inbound frame dispatch and execution lifecycle forwarding.

use super::*;

/// Inbound-text dispatcher pulled out of `maintain_proxy_connection`
/// so the parse / route sequence is reusable for tests and the
/// per-frame logic stays out of the WS select loop.
/// Signaling types that are server-originated central→daemon plumbing: they are
/// accepted only from the trusted-central link. A Local / remote-signaling origin
/// (no trusted PDP) must never inject operator templates, weaken the command
/// blocklist, drive an evidence collection, dispatch a sealed execution plan,
/// drive a remote read, surface a forged support code to the local user, or forge
/// a grant-session teardown that tears down a legitimate session.
pub(super) fn is_trusted_central_only(t: SignalingType) -> bool {
    matches!(
        t,
        SignalingType::SyncCommandTemplates
            | SignalingType::SyncCommandBlocklist
            | SignalingType::CollectEvidence
            | SignalingType::ExecuteEdgePlan
            | SignalingType::InvokeRemoteTool
            | SignalingType::SupportCodeIssued
            | SignalingType::RevokeAccessGrant
            | SignalingType::UpdateDeviceAssistantSettings
    )
}

///
/// Parses the inbound text once, applies source-gated authorization wrapper
/// handling ([`gate_authz_frame`]), and hands the model to
/// [`signaling_router::route`]. The router exhaustively dispatches:
/// PC / SDP / ICE types are handled inline, worker-bound types ride
/// dedicated `ServiceToWorker::*` typed IPC variants, and
/// daemon-emitted notifications are trace-logged + dropped.
pub(super) async fn handle_inbound_signaling_text(
    text_str: String,
    router_ctx: &RouterContext,
    source: InboundSignalingSource,
    fatal_quota_reject_enabled: bool,
) -> InboundOutcome {
    let parsed = match serde_json::from_str::<SignalingModel>(&text_str) {
        Ok(m) => m,
        Err(e) => {
            warn!("[Proxy] Dropping malformed signaling text: {e}");
            return InboundOutcome::Continue;
        }
    };

    // Manager-link registration rejection: an `Error(-1)` carrying a device-quota
    // fatal code. Intercept before the router (which swallows `Error` frames) so the
    // caller can stop the auto-reconnect storm. Only honoured on the manager link;
    // the same code on any other link is treated as transient (just dropped below).
    if fatal_quota_reject_enabled
        && let Some((error_code, message)) = fatal_registration_reject(&parsed)
    {
        return InboundOutcome::FatalReject {
            error_code,
            message,
        };
    }

    // Source gate: server-originated central→daemon plumbing (see
    // [`is_trusted_central_only`]) is accepted only from the trusted-central link.
    // For the blocklist this is critical: a forged sync with a higher revision and
    // a thinned rule set would otherwise wipe the daemon's floor and fail-open.
    if is_trusted_central_only(parsed.signaling_type)
        && source != InboundSignalingSource::TrustedCentral
    {
        warn!(
            "[Proxy] Dropping {:?} from non-central source {source:?}",
            parsed.signaling_type
        );
        return InboundOutcome::Continue;
    }

    let expected_audience = {
        let s = router_ctx.settings.read().await;
        s.system.get_client_id().unwrap_or_default()
    };
    let now = chrono::Utc::now().to_rfc3339();

    // `EdgeExecRequest` uses a dedicated authorization gate: a trusted-but-
    // invalid request is answered with a synthesized denied result (so the
    // central pending entry resolves) rather than silently dropped.
    if parsed.signaling_type == SignalingType::ExecuteEdgePlan {
        match gate_fleet_exec_frame(parsed, &expected_audience, &now) {
            FleetExecGateOutcome::Pass(unwrapped, block) => {
                let effective_ctx = RouterContext {
                    inbound_authz: Some(block),
                    ..router_ctx.clone()
                };
                if let Err(e) = signaling_router::route(&unwrapped, &effective_ctx).await {
                    warn!("[Proxy] router handler failed for EdgeExecRequest: {e}");
                }
            }
            FleetExecGateOutcome::Denied { request_id, reason } => {
                warn!("[Proxy] EdgeExecRequest denied ({reason}); replying denied result");
                signaling_router::send_edge_execution_completed(
                    &router_ctx.outbound_tx,
                    &request_id,
                    desk_agent_protocol::edge_exec::EdgeExecDisposition::RejectedBeforeDispatch {
                        error: desk_agent_protocol::edge_exec::EdgeExecDisposition::safe_error(
                            desk_agent_protocol::AgentErrorKind::PermissionDenied,
                            reason,
                            false,
                        ),
                    },
                );
            }
            FleetExecGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping EdgeExecRequest: {reason}");
            }
        }
        return InboundOutcome::Continue;
    }

    // `CollectRequest` / `RemoteToolRequest` carry an optional authorization
    // wrapper: bare from the enterprise-manager path, wrapped from an OSS signal
    // central brain. Validate-and-unwrap when wrapped, pass through when bare.
    // The router handlers parse the inner payload either way, so unwrapping here
    // keeps them untouched.
    if is_central_plumbing_frame(parsed.signaling_type) {
        match gate_optional_central_wrapper(parsed, &expected_audience, &now) {
            AuthzGateOutcome::Pass(unwrapped, authz) => {
                let effective_ctx;
                let ctx_ref = match authz {
                    Some(block) => {
                        effective_ctx = RouterContext {
                            inbound_authz: Some(block),
                            ..router_ctx.clone()
                        };
                        &effective_ctx
                    }
                    None => router_ctx,
                };
                if let Err(e) = signaling_router::route(&unwrapped, ctx_ref).await {
                    warn!(
                        "[Proxy] router handler failed for {:?}: {e}",
                        unwrapped.signaling_type
                    );
                }
            }
            AuthzGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping central plumbing frame: {reason}");
            }
        }
        return InboundOutcome::Continue;
    }

    // `RequestRemoteAccess` carries the capability-ceiling stamp: required and validated
    // on the trusted-central link (a bare one there is dropped as a downgrade
    // attempt), rejected if stamped from a non-central source, passed through bare
    // on the owner-only relay path. The validated stamp rides into the router via
    // the context so the freshly-created session inherits its restriction /
    // ceiling.
    if parsed.signaling_type == SignalingType::RequestRemoteAccess {
        match gate_request_remote_frame(parsed, source, &expected_audience, &now) {
            RequestRemoteGateOutcome::Pass(unwrapped, authz) => {
                let effective_ctx;
                let ctx_ref = if authz.is_some() {
                    effective_ctx = RouterContext {
                        inbound_request_remote_authz: authz,
                        ..router_ctx.clone()
                    };
                    &effective_ctx
                } else {
                    router_ctx
                };
                if let Err(e) = signaling_router::route(&unwrapped, ctx_ref).await {
                    warn!("[Proxy] router handler failed for RequestRemoteAccess: {e}");
                }
            }
            RequestRemoteGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping RequestRemoteAccess: {reason}");
            }
        }
        return InboundOutcome::Continue;
    }

    // `StartTerminal` carries the same capability-ceiling stamp as `RequestRemoteAccess`
    // (it is the admission-establishing frame for the distinct terminal WS): required
    // and validated on the trusted-central link, rejected if stamped from a
    // non-central source, passed through bare on the owner-only relay path. The
    // validated stamp rides into the router via the context so
    // `handle_start_terminal_inbound` can register the connection's ceiling +
    // admission + grant index.
    if parsed.signaling_type == SignalingType::StartTerminal {
        match gate_start_terminal_frame(parsed, source, &expected_audience, &now) {
            RequestRemoteGateOutcome::Pass(unwrapped, authz) => {
                let effective_ctx;
                let ctx_ref = if authz.is_some() {
                    effective_ctx = RouterContext {
                        inbound_start_terminal_authz: authz,
                        ..router_ctx.clone()
                    };
                    &effective_ctx
                } else {
                    router_ctx
                };
                if let Err(e) = signaling_router::route(&unwrapped, ctx_ref).await {
                    warn!("[Proxy] router handler failed for StartTerminal: {e}");
                }
            }
            RequestRemoteGateOutcome::Drop(reason) => {
                warn!("[Proxy] Dropping StartTerminal: {reason}");
            }
        }
        return InboundOutcome::Continue;
    }

    let (parsed, authz) = match gate_authz_frame(parsed, source, &expected_audience, &now) {
        AuthzGateOutcome::Pass(m, authz) => (m, authz),
        AuthzGateOutcome::Drop(reason) => {
            warn!("[Proxy] Dropping AI frame: {reason}");
            return InboundOutcome::Continue;
        }
    };

    // A validated central authorization rides into the handlers via a per-call
    // clone of the router context (cheap: the context is Arc-backed), keeping
    // `route()` and the AI handler signatures untouched.
    let effective_ctx;
    let ctx_ref = if authz.is_some() {
        effective_ctx = RouterContext {
            inbound_authz: authz,
            ..router_ctx.clone()
        };
        &effective_ctx
    } else {
        router_ctx
    };

    if let Err(e) = signaling_router::route(&parsed, ctx_ref).await {
        warn!(
            "[Proxy] router handler failed for {:?}: {e}; dropping unrouted message",
            parsed.signaling_type,
        );
    }

    InboundOutcome::Continue
}

/// Send an `ExecLifecycle(625)` about one execution to whoever asked for it.
///
/// Notification-style and best-effort: these frames report progress, and the
/// authoritative answer is always a state query against the ledger. Dropping one
/// therefore costs nothing an upstream would act on.
pub(crate) fn send_exec_lifecycle(
    outbound_tx: &tokio::sync::broadcast::Sender<String>,
    execution_generation: &str,
    to_connection_id: Option<String>,
    event: ExecLifecycleEvent,
) {
    let payload = ExecLifecyclePayload {
        execution_generation: execution_generation.to_string(),
        event,
    };
    let data = match serde_json::to_value(&payload) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[exec-lifecycle] could not serialise the frame: {e}");
            return;
        }
    };
    let frame = SignalingModel::new(
        execution_generation,
        SignalingType::ExecutionProgressUpdated,
        None,
        to_connection_id,
        Some(data),
        None,
    );
    match serde_json::to_string(&frame) {
        Ok(text) => {
            let _ = outbound_tx.send(text);
        }
        Err(e) => log::warn!("[exec-lifecycle] could not serialise the frame: {e}"),
    }
}
