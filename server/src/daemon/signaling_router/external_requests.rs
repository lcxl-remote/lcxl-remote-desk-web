use super::*;

/// Handle an inbound remote-collect request from the manager. Runs the
/// daemon's read-only collectors over the policy-gated capability set, refits
/// any screenshot into a model-ready data URL, redacts text evidence, and
/// streams the resulting [`EvidenceSnapshot`](desk_agent_protocol::evidence::EvidenceSnapshot)
/// back to the manager as a chunked `CollectResponse`. Always replies (a chunk
/// stream or an error frame) so the manager's pending entry never hangs.
pub(super) async fn handle_collect_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request: CollectRequest = match model.get_data::<CollectRequest>() {
        Ok(r) => r,
        Err(e) => {
            // No request_id to correlate; log and drop (the manager times out).
            log::warn!("[router] dropping malformed CollectRequest: {e}");
            return Ok(());
        }
    };
    let request_id = request.request_id.clone();

    // The collector is only injected where an in-process worker can collect
    // (Default / DeskServer). Without it the edge cannot serve a remote
    // collection — report a wholesale error.
    let Some(orchestrator) = ctx.diagnose_orchestrator.clone() else {
        send_collect_error(
            &ctx.outbound_tx,
            &request_id,
            AgentErrorKind::SessionUnavailable,
            "evidence collector is not available on this host",
        );
        return Ok(());
    };

    match orchestrator
        .collect_for_remote(&request_id, &request.request)
        .await
    {
        Ok(snapshot) => {
            match desk_diagnose_core::chunk::chunk_snapshot(
                &request_id,
                &snapshot,
                COLLECT_CHUNK_PAYLOAD_LIMIT,
            ) {
                Ok(chunks) => {
                    for chunk in chunks {
                        send_collect_response(&ctx.outbound_tx, &CollectResponse::Chunk(chunk));
                    }
                }
                Err(e) => {
                    send_collect_error(
                        &ctx.outbound_tx,
                        &request_id,
                        AgentErrorKind::Internal,
                        &format!("failed to encode evidence snapshot: {e}"),
                    );
                }
            }
        }
        Err(e) => {
            // Preserve the failure class (notably a fail-closed `RedactionFailed`)
            // so the central orchestrator audits it correctly.
            send_collect_error(&ctx.outbound_tx, &request_id, e.kind, &e.message);
        }
    }
    Ok(())
}

/// Serialize and emit a [`CollectResponse`] frame toward the manager over the
/// outbound lane. Mirrors the audit-event emit path: a server-initiated
/// `new_request` (its signaling `request_id` is unused — correlation rides the
/// payload's `request_id`), consumed only by the manager's collect observer.
pub(super) fn send_collect_response(
    outbound_tx: &broadcast::Sender<String>,
    response: &CollectResponse,
) {
    match SignalingModel::new_request(
        SignalingType::EvidenceCollectionUpdated,
        None,
        Some(response),
    ) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => log::warn!("[collect] failed to serialize CollectResponse: {e}"),
        },
        Err(e) => log::warn!("[collect] failed to build CollectResponse model: {e}"),
    }
}

/// Emit a wholesale [`CollectResponse::Error`] for `request_id`, tagged with the
/// structured failure `kind` so the central orchestrator can audit it.
pub(super) fn send_collect_error(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    kind: AgentErrorKind,
    reason: &str,
) {
    send_collect_response(
        outbound_tx,
        &CollectResponse::Error(CollectResponseError {
            request_id: request_id.to_string(),
            error_kind: kind,
            reason: reason.to_string(),
        }),
    );
}

/// Handle an inbound remote read-tool request from the manager (§8.3). Runs the
/// one server-stamped capability call against the in-process device agent (which
/// enforces the envelope's gate), redacts the result fail-closed, and streams it
/// back as a chunked `RemoteToolResponse`. Always replies (a chunk stream or an
/// error frame) so the manager's pending entry never hangs.
pub(super) async fn handle_remote_tool_request_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::remote_tool::{
        REMOTE_TOOL_CHUNK_PAYLOAD_LIMIT, RemoteToolRequest, RemoteToolResponse,
    };
    let request: RemoteToolRequest = match model.get_data::<RemoteToolRequest>() {
        Ok(r) => r,
        Err(e) => {
            // No request_id to correlate; log and drop (the manager times out).
            log::warn!("[router] dropping malformed RemoteToolRequest: {e}");
            return Ok(());
        }
    };
    let request_id = request.request_id.clone();

    if !ctx.settings.read().await.device_assistant.enabled {
        send_remote_tool_error(
            &ctx.outbound_tx,
            &request_id,
            AgentErrorKind::UnsupportedCapability,
            "Device Assistant is disabled on this device",
        );
        return Ok(());
    }

    // The read invoker is only injected where an in-process worker can read
    // (Default / DeskServer). Without it the edge cannot serve a remote read.
    let Some(invoker) = ctx.remote_read.clone() else {
        send_remote_tool_error(
            &ctx.outbound_tx,
            &request_id,
            AgentErrorKind::SessionUnavailable,
            "remote read is not available on this host",
        );
        return Ok(());
    };

    match invoker.invoke_redacted(request.envelope.into()).await {
        Ok(output) => match serde_json::to_vec(&output) {
            Ok(bytes) => {
                for chunk in desk_diagnose_core::chunk::chunk_bytes(
                    &request_id,
                    &bytes,
                    REMOTE_TOOL_CHUNK_PAYLOAD_LIMIT,
                ) {
                    send_remote_tool_response(&ctx.outbound_tx, &RemoteToolResponse::Chunk(chunk));
                }
            }
            Err(e) => {
                send_remote_tool_error(
                    &ctx.outbound_tx,
                    &request_id,
                    AgentErrorKind::Internal,
                    &format!("failed to encode remote tool result: {e}"),
                );
            }
        },
        Err(e) => {
            // Preserve the failure class (notably a fail-closed `RedactionFailed`
            // or a gate `PermissionDenied`) so the central loop reports it safely.
            send_remote_tool_error(&ctx.outbound_tx, &request_id, e.kind, &e.message);
        }
    }
    Ok(())
}

/// Serialize and emit a [`RemoteToolResponse`](desk_agent_protocol::remote_tool::RemoteToolResponse)
/// frame toward the manager over the outbound lane (correlation rides the
/// payload's `request_id`, consumed only by the manager's remote-tool observer).
pub(super) fn send_remote_tool_response(
    outbound_tx: &broadcast::Sender<String>,
    response: &desk_agent_protocol::remote_tool::RemoteToolResponse,
) {
    match SignalingModel::new_request(SignalingType::RemoteToolOutputUpdated, None, Some(response))
    {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => log::warn!("[rtool] failed to serialize RemoteToolResponse: {e}"),
        },
        Err(e) => log::warn!("[rtool] failed to build RemoteToolResponse model: {e}"),
    }
}

/// Emit a wholesale [`RemoteToolResponse::Error`](desk_agent_protocol::remote_tool::RemoteToolResponse)
/// for `request_id`, tagged with the model-safe failure so the central loop turns
/// it into an error tool-result.
pub(super) fn send_remote_tool_error(
    outbound_tx: &broadcast::Sender<String>,
    request_id: &str,
    kind: AgentErrorKind,
    reason: &str,
) {
    use desk_agent_protocol::remote_tool::{RemoteToolResponse, RemoteToolResponseError};
    send_remote_tool_response(
        outbound_tx,
        &RemoteToolResponse::Error(RemoteToolResponseError {
            request_id: request_id.to_string(),
            error: AgentError {
                kind,
                message: reason.to_string(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            },
        }),
    );
}

/// Apply an inbound `CommandTemplateSync` from the manager: parse the payload,
/// reject an unknown wire version, and replace the operator-template cache
/// (entries are shape-validated, fail-closed, inside `replace`). The exec
/// classifier picks up the new set on the next `ConfirmExec`.
pub(super) fn handle_command_template_sync_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::command_template::{
        COMMAND_TEMPLATE_SYNC_VERSION, CommandTemplateSyncPayload,
        MIN_COMMAND_TEMPLATE_SYNC_VERSION,
    };
    let payload = match model.get_data::<CommandTemplateSyncPayload>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[router] bad CommandTemplateSync payload: {e}");
            return Ok(());
        }
    };
    // Accept any version in the supported range; a version outside it (e.g. a
    // future version reaching an older daemon) is safely ignored. The set-narrowing
    // wire epoch — not the payload version — is what guards against a stale
    // pre-narrowing sender: `replace` rejects any frame below the current epoch
    // floor, so a payload that predates set narrowing (epoch 0) never widens the
    // cache regardless of its version.
    if !(MIN_COMMAND_TEMPLATE_SYNC_VERSION..=COMMAND_TEMPLATE_SYNC_VERSION)
        .contains(&payload.version)
    {
        log::warn!(
            "[router] ignoring CommandTemplateSync with unsupported version {}",
            payload.version
        );
        return Ok(());
    }
    let revision = payload.command_template_revision;
    let epoch = payload.epoch;
    match ctx
        .command_templates
        .replace(payload.templates, epoch, revision)
    {
        Some(accepted) => log::info!(
            "[router] applied operator command-template sync: {accepted} template(s) (epoch {epoch}, revision {revision:?})"
        ),
        None => log::info!(
            "[router] ignored stale operator command-template sync (epoch {epoch}, revision {revision:?})"
        ),
    }
    Ok(())
}

/// Apply an inbound `CommandBlocklistSync` from the manager: parse the payload,
/// reject an unknown wire version, and replace the effective-blocklist cache
/// (revision-gated, fail-closed inside `replace`). A frame with no revision is
/// dropped — the manager always stamps one, and for the blocklist a revision is
/// required to enforce monotonic ordering. The exec classifier's Step 0 picks up
/// the new set on the next classification.
/// Surface a manager-issued temporary support code to the local user and arm the
/// session's expiry teardown.
///
/// The code arrives over the host's dedicated Support upstream (the source gate
/// has already dropped any non-central origin). The daemon records it in the
/// support link state for the local UI and spawns a timer that ends the session
/// at the code's expiry — guarded by the session epoch so a stale timer from an
/// earlier session cannot tear down a newer one. The signaling proxy's support
/// loop performs the actual upstream / PC teardown when the state flips inactive.
pub(super) fn handle_support_code_issued_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_signal_facade::model::support::SupportCodeIssuedData;
    let state = ctx.support_link_state.clone();
    let request_id = model.request_id.clone();
    let Some(response_state) = model.response_state.as_ref() else {
        log::warn!("[support] ignoring SupportCodeIssued without response state");
        return Ok(());
    };
    if !response_state.is_success() {
        actix_web::rt::spawn(async move {
            if !state.settle_request(&request_id).await {
                log::warn!("[support] ignoring stale SupportCodeIssued response");
                return;
            }
            log::warn!("[support] manager refused support-code request");
            state.finish().await;
        });
        return Ok(());
    }
    let payload = match model.get_data::<SupportCodeIssuedData>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[support] bad SupportCodeIssued payload: {e}");
            return Ok(());
        }
    };
    let expires_at = payload.expires_at;
    let code = payload.code;
    let armed_epoch = state.epoch();
    actix_web::rt::spawn(async move {
        if !state.settle_request(&request_id).await {
            log::warn!("[support] ignoring stale SupportCodeIssued response");
            return;
        }
        log::info!("[support] manager issued temporary support code (expires_at={expires_at})");
        state.set_snapshot(code, expires_at).await;
        let now = chrono::Utc::now().timestamp();
        let remaining = expires_at.saturating_sub(now).max(0) as u64;
        tokio::time::sleep(std::time::Duration::from_secs(remaining)).await;
        // Only tear down if this is still the same session — a manual stop or a
        // fresh start (which bumps the epoch) supersedes this timer.
        if state.epoch() == armed_epoch && state.is_active() {
            log::info!("[support] temporary support code expired; ending session");
            state.request_stop();
        }
    });
    Ok(())
}

/// Apply an inbound `RevokeAccessGrant` from the manager (the source gate has
/// already dropped any non-central origin). Direct-closes every grant session this
/// host holds whose recorded generation is `≤ revoked_generation`, cutting an
/// already-established peer connection immediately after a dial-code regeneration —
/// the in-flight teardown that the `authorize` generation check alone can only
/// enforce on the session's *next* `RequestRemoteAccess`.
pub(super) async fn handle_revoke_access_grant_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_signal_facade::model::access_grant::RevokeAccessGrantData;
    let payload = match model.get_data::<RevokeAccessGrantData>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[grant] bad RevokeAccessGrant payload: {e}");
            return Ok(());
        }
    };
    // Session-scoped teardown (the owner ended one temporary-support session):
    // close exactly that grant session, leaving its generation-mates up. A
    // generation-scoped frame (no session id) closes the whole superseded range.
    if let Some(grant_session_id) = payload.grant_session_id.as_deref() {
        log::info!(
            "[grant] manager revoked grant session {} for device {} (reason: {})",
            grant_session_id,
            payload.target_device,
            payload.reason
        );
        pc_manager::close_grant_session(
            &ctx.pc_registry,
            &ctx.worker_mgr,
            ctx.virtual_display.as_ref(),
            grant_session_id,
            &payload.reason,
        )
        .await;
        return Ok(());
    }
    log::info!(
        "[grant] manager revoked grants for device {} at generation <= {} (reason: {})",
        payload.target_device,
        payload.revoked_generation,
        payload.reason
    );
    pc_manager::close_grants_up_to_generation(
        &ctx.pc_registry,
        &ctx.worker_mgr,
        ctx.virtual_display.as_ref(),
        payload.revoked_generation,
        &payload.reason,
    )
    .await;
    Ok(())
}

pub(super) fn handle_command_blocklist_sync_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    use desk_agent_protocol::command_blocklist::{
        COMMAND_BLOCKLIST_SYNC_VERSION, CommandBlocklistSyncPayload,
        MIN_COMMAND_BLOCKLIST_SYNC_VERSION,
    };
    let payload = match model.get_data::<CommandBlocklistSyncPayload>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[router] bad CommandBlocklistSync payload: {e}");
            return Ok(());
        }
    };
    if !(MIN_COMMAND_BLOCKLIST_SYNC_VERSION..=COMMAND_BLOCKLIST_SYNC_VERSION)
        .contains(&payload.version)
    {
        log::warn!(
            "[router] ignoring CommandBlocklistSync with unsupported version {}",
            payload.version
        );
        return Ok(());
    }
    // The blocklist requires a revision to enforce monotonic ordering (a stale
    // frame must never roll back a newer set, re-opening a denied command). A
    // frame without one is malformed for this type — drop it and keep the
    // current cache (which is at worst the built-in floor).
    let Some(revision) = payload.command_blocklist_revision else {
        log::warn!("[router] dropping CommandBlocklistSync without a revision");
        return Ok(());
    };
    match ctx.command_blocklist.replace(payload.rules, revision) {
        Some(count) => log::info!(
            "[router] applied command-blocklist sync: {count} effective rule(s) (revision {revision})"
        ),
        None => {
            log::warn!("[router] command-blocklist sync at revision {revision} rejected as stale")
        }
    }
    Ok(())
}
