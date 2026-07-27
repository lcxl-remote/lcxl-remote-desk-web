use super::*;

pub(super) async fn handle_manager_system_info_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ManagerRequestRefPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerSystemInfoRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerSystemInfoRequest: {e}");
    }
    Ok(())
}

/// Report the host's system settings to a manager.
///
/// Answered here rather than in the worker: the daemon holds the settings a
/// change is committed against, so reading them anywhere else would let a
/// manager see values that were superseded before it asked.
pub(super) async fn handle_manager_query_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let remote_settings = {
        let settings = ctx.settings.read().await;
        RemoteSystemSettings {
            enable_ipv6: settings.system.enable_ipv6,
            port: settings.system.port,
            listen_addr_ipv4: settings.system.listen_addr_ipv4.clone(),
            listen_addr_ipv6: settings.system.listen_addr_ipv6.clone(),
            locale: settings.system.locale.clone(),
            signaling_url: settings.system.signaling_url.clone(),
            signaling_token: settings.system.signaling_token.clone(),
            manager_url: settings.system.manager_url.clone(),
            auto_start: settings.system.auto_start,
            manager_api_token: settings.system.manager_api_token.clone(),
        }
    };
    emit_success_response(ctx, model, Some(&remote_settings));
    Ok(())
}

/// Apply a manager-pushed `RemoteSystemSettings` onto the local `SystemSettings`.
///
/// `auto_start` is intentionally NOT copied: it is node-local OS state (a
/// LaunchAgent on macOS, an OS-service / login entry elsewhere) and may only be
/// changed via this node's own `/settings` endpoint, never pushed from a remote
/// manager — otherwise a manager-wide settings update could silently toggle a
/// host's unattended auto-start. The protocol field is left untouched; we simply
/// don't act on it here.
fn apply_remote_system_settings(
    system: &mut crate::model::settings::SystemSettings,
    remote: RemoteSystemSettings,
) {
    system.enable_ipv6 = remote.enable_ipv6;
    system.port = remote.port;
    system.listen_addr_ipv4 = remote.listen_addr_ipv4;
    system.listen_addr_ipv6 = remote.listen_addr_ipv6;
    system.locale = remote.locale;
    system.signaling_url = remote.signaling_url;
    system.signaling_token = remote.signaling_token;
    system.manager_url = remote.manager_url;
    system.manager_api_token = remote.manager_api_token;
}

pub(super) async fn handle_manager_file_list_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "ManagerFileList") else {
        return Ok(());
    };
    let params = match model.get_data::<FileListParams>() {
        Ok(p) => p,
        Err(e) => {
            log::warn!(
                "[router] ManagerFileList payload parse failed for {connection_id:?}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerFileListRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: connection_id.to_string(),
        params,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerFileListRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerFileListRequest: {e}");
    }
    Ok(())
}

pub(super) async fn handle_manager_file_delete_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "ManagerFileDelete") else {
        return Ok(());
    };
    let request = match model.get_data::<DeleteFileRequest>() {
        Ok(r) => r,
        Err(e) => {
            log::warn!(
                "[router] ManagerFileDelete payload parse failed for {connection_id:?}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ManagerFileDeleteRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: connection_id.to_string(),
        request,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ManagerFileDeleteRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ManagerFileDeleteRequest: {e}");
    }
    Ok(())
}

/// Apply a manager's system-settings update.
///
/// One update can move the locale alongside ordinary fields, and they commit
/// together: splitting them would put a second write between the two and leave
/// a crash in the middle with half the change on disk. The manager is told
/// which way it went — a parse failure and a failed write are different things
/// to a caller deciding whether to retry, and silence would leave it guessing.
pub(super) async fn handle_manager_update_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let connection_id = optional_from_connection_id(model);
    let remote = match model.get_data::<RemoteSystemSettings>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] ManagerUpdateSettings payload parse failed for {connection_id:?}: \
                 {e}; rejecting (request_id={})",
                model.request_id,
            );
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                "unreadable system settings payload",
            );
            return Ok(());
        }
    };
    match ctx
        .settings_coordinator
        .commit(move |settings| {
            apply_remote_system_settings(&mut settings.system, remote);
            Ok(())
        })
        .await
    {
        Ok(outcome) => {
            if let Some(locale) = outcome.locale_changed_to {
                announce_locale(ctx, &locale).await;
            }
            emit_success_response(ctx, model, Option::<&()>::None);
        }
        Err(error) => {
            log::warn!(
                "[router] ManagerUpdateSettings could not be applied for {connection_id:?}: \
                 {error} (request_id={})",
                model.request_id,
            );
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::SYSTEM_ERROR,
                "failed to apply system settings",
            );
        }
    }
    Ok(())
}

/// Tell the local shell the host now runs in a different locale, so its own UI
/// follows. The worker is told by the coordinator as part of the commit.
async fn announce_locale(ctx: &RouterContext, locale: &str) {
    let _ = ctx.host_control_hub.send_command(
        crate::host_control::HostControlMessage::GlobalLocaleChanged {
            locale: locale.to_string(),
        },
    );
}

// ---- Terminal-plane typed-IPC dispatch helpers ----
//
// The 5 inbound terminal request types share the same skeleton as the
// manager-plane helpers — pull `from_connection_id`, build the typed
// `ServiceToWorker::*Request` payload, ship it via
// `WorkerManager::send_to_worker`. Differences are only in payload
// type and whether the inbound model carries a body / a request_id.
// Errors are non-fatal for the WS connection: parse / send failures
// log + drop.

pub(super) async fn handle_start_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "StartTerminal") else {
        return Ok(());
    };
    let session = match model.get_data::<StartTerminalSession>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] StartTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    // The terminal WS is a distinct connection that never does a `RequestRemote`, so
    // this is its admission-establishing frame: register the connection's capability
    // ceiling + admission (+ grant index) from the validated stamp before shipping
    // the request to the worker, so the worker-side `meet(ceiling, global)` gate
    // enforces it from the very first terminal request. Fail-closed: a capped ceiling
    // that cannot reach the worker refuses the terminal (never starts it ceiling-less).
    if !register_terminal_admission(ctx, connection_id).await {
        return Ok(());
    }
    ctx.host_control_hub.host_activity().ensure_session(
        connection_id,
        ctx.inbound_start_terminal_authz
            .as_ref()
            .map(|authz| authz.actor.clone())
            .unwrap_or_else(desk_signal_facade::model::request_remote_authz::ActorSummary::unknown),
    );

    let payload = StartTerminalRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: connection_id.to_string(),
        session,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::StartTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed StartTerminalRequest: {e}");
    }
    Ok(())
}

/// Register a terminal connection's admission, worker ceiling, and grant index from
/// the validated `StartTerminal` stamp (the terminal analogue of what
/// `handle_request_remote` does for a control connection). Returns `false` — refuse
/// the terminal — only when a capped ceiling fails to reach the worker (fail-closed:
/// a terminal must never run with no worker-side cap, which would fall back to the
/// host global). A central stamp with `access_ceiling: None` is an owner session; a
/// bare frame (owner-only relay / local path, no stamp) is likewise admitted as
/// owner with no ceiling.
pub(super) async fn register_terminal_admission(ctx: &RouterContext, connection_id: &str) -> bool {
    match ctx.inbound_start_terminal_authz.as_ref() {
        Some(authz) => {
            if let Some(ceiling) = authz.access_ceiling.as_ref() {
                if let Err(e) = ctx
                    .worker_mgr
                    .send_to_worker(ServiceToWorker::SetConnectionCeiling(
                        desk_ipc_protocol::message::SetConnectionCeilingPayload {
                            connection_id: connection_id.to_string(),
                            ceiling: Some(ceiling.clone()),
                        },
                    ))
                    .await
                {
                    log::warn!(
                        "[router] StartTerminal ceiling registration failed for {connection_id}: \
                         {e}; refusing terminal"
                    );
                    return false;
                }
                ctx.pc_registry
                    .record_admission(
                        connection_id,
                        pc_manager::Admission::Capped(ceiling.clone()),
                    )
                    .await;
            } else {
                ctx.pc_registry
                    .record_admission(connection_id, pc_manager::Admission::OwnerFull)
                    .await;
            }
            // Index a capped terminal under its grant so a directed revocation /
            // dial-code regeneration tears it down with the rest of the session.
            if let Some(gsid) = authz.grant_session_id.as_deref() {
                ctx.pc_registry
                    .index_grant_connection(gsid, authz.generation, connection_id)
                    .await;
            }
        }
        None => {
            ctx.pc_registry
                .record_admission(connection_id, pc_manager::Admission::OwnerFull)
                .await;
        }
    }
    ctx.pc_registry
        .mark_terminal_connection(connection_id)
        .await;
    true
}

pub(super) async fn handle_send_data_to_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "SendDataToTerminal") else {
        return Ok(());
    };
    let data = match model.get_data_with_type::<TerminalInputData>() {
        Ok(Some(d)) => d,
        Ok(None) => {
            // Empty payload — match the legacy handler's silent ignore.
            return Ok(());
        }
        Err(e) => {
            log::warn!(
                "[router] SendDataToTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = SendDataToTerminalPayload {
        connection_id: connection_id.to_string(),
        data,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::SendDataToTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed SendDataToTerminalRequest: {e}");
    }
    Ok(())
}

pub(super) async fn handle_resize_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "ResizeTerminal") else {
        return Ok(());
    };
    let data = match model.get_data_with_type::<TerminalResizeData>() {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(()),
        Err(e) => {
            log::warn!(
                "[router] ResizeTerminal payload parse failed for {connection_id}: {e}; \
                 dropping (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let payload = ResizeTerminalPayload {
        connection_id: connection_id.to_string(),
        data,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ResizeTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ResizeTerminalRequest: {e}");
    }
    Ok(())
}

pub(super) async fn handle_close_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let Some(connection_id) = require_from_connection_id(model, "CloseTerminal") else {
        return Ok(());
    };
    let payload = CloseTerminalPayload {
        connection_id: connection_id.to_string(),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::CloseTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed CloseTerminalRequest: {e}");
    }

    // Clear the terminal connection's whole capability footprint so nothing survives
    // its close: worker ceiling, admission, grant index, terminal mark. Gated on the
    // terminal marker so a stray `CloseTerminal` from a non-terminal connection can
    // never clear that connection's admission. The connection id is a fresh UUID
    // (never reused), but clearing promptly also bounds the maps' growth.
    if ctx.pc_registry.is_terminal_connection(connection_id).await {
        if let Err(e) = ctx
            .worker_mgr
            .send_to_worker(ServiceToWorker::SetConnectionCeiling(
                desk_ipc_protocol::message::SetConnectionCeilingPayload {
                    connection_id: connection_id.to_string(),
                    ceiling: None,
                },
            ))
            .await
        {
            log::debug!(
                "[router] terminal ceiling clear for {connection_id} did not reach worker: {e}"
            );
        }
        ctx.pc_registry.clear_admission(connection_id).await;
        ctx.pc_registry
            .unindex_grant_connection(connection_id)
            .await;
        ctx.pc_registry
            .unmark_terminal_connection(connection_id)
            .await;
    }
    Ok(())
}

pub(super) async fn handle_list_terminal_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let payload = ListTerminalRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: optional_from_connection_id(model),
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::ListTerminalRequest(payload))
        .await
    {
        log::warn!("[router] failed to send typed ListTerminalRequest: {e}");
    }
    Ok(())
}

// ---- AI agent plane typed-IPC dispatch ----
//
// Inbound `AgentRequest` from a control end carries the
// non-authoritative `desk_agent_protocol::AgentRequestData` (operation +
// reason). The daemon two-phase-parses the operation against its
// supported-kind set (so an unknown *newer* kind degrades to
// `UnsupportedCapability` instead of failing serde), derives the
// capability from the input, authorizes it against a server-computed
// scope, stamps every trusted field, and ships a typed
// `ServiceToWorker::AgentRequest` to the worker. Any rejection short-
// circuits with an outbound `AgentResponse(AgentOutcome::Err)`; the
// route itself always returns `Ok(())` (the control-end-visible failure
// is the outcome we already emitted).

/// Outer `OperationInput` tags this build understands. A control end on
/// a newer protocol may send a kind outside this set; the two-phase
/// parse turns that into `UnsupportedCapability`.
pub(super) const SUPPORTED_OPERATION_KINDS: &[&str] = &["read_context", "exec"];

/// Inner `ContextKind` tags (the supported read capabilities) this build
/// can collect. The unknown-kind check descends to this level because
/// the permission point is nested — `operation.input.kind` is only the
/// `read_context` / `exec` dispatch layer; the real capability is
/// `operation.input.params.kind.kind`.
pub(super) const SUPPORTED_READ_KINDS: &[&str] = &[
    "system_info",
    "process_list",
    "network_ports",
    "service_status",
    "log_recent",
    "container_list",
    "container_inspect",
    "container_logs",
    "screen_capture_current",
];
