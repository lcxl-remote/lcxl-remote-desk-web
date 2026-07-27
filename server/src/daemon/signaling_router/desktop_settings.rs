use super::*;

/// Computes whether the daemon currently wants the worker in
/// exclusive mode, plus the pre-detach prompt duration to use when
/// entering. Both router (on control change) and supervisor (on
/// attach edge) reach the answer through this helper so there is a
/// single source of truth.
///
/// `active` is the supervisor's `is_active()` snapshot the caller has
/// already taken (the helper does **not** call back into the
/// supervisor — that would risk a lock cycle and re-introduce the
/// self-reference path).
pub async fn compute_desired_with_active(
    settings: &crate::model::settings::SharedSettings,
    pc_registry: &PcRegistry,
    active: bool,
) -> (bool, u32) {
    let s = settings.read().await;
    let on = s.virtual_display.enabled && s.virtual_display.exclusive;
    let prompt_ms = s.virtual_display.prompt_ms;
    drop(s);
    if !on || !active {
        return (false, prompt_ms);
    }
    let any = pc_registry.any_with_accept_control().await;
    (any, prompt_ms)
}

/// Called by the `RequireControl` route after `handle_require_control`
/// settles the per-PC `accept_control` flag. Pokes the supervisor's
/// `set_desired_exclusive` so its internal driver loop can recompute
/// the IPC to send (if any).
///
/// `outcome.changed = false` short-circuits — a re-grant of an
/// already-accepted permission never moves the desired flag.
pub async fn update_exclusive_after_control_change(
    ctx: &RouterContext,
    outcome: &crate::daemon::pc_manager::ControlOutcome,
) {
    if !outcome.changed {
        return;
    }
    let Some(supervisor) = ctx.virtual_display.as_ref() else {
        return;
    };
    let active = supervisor.is_active().await;
    let (desired, prompt_ms) =
        compute_desired_with_active(&ctx.settings, &ctx.pc_registry, active).await;
    supervisor.set_desired_exclusive(desired, prompt_ms);
}

/// Emit an error response back to the browser via `outbound_tx`. The
/// browser's pending request matches on `request_id` + `signaling_type`.
/// Build / serialise failures are non-fatal — log and drop.
pub(super) fn emit_error_response(
    ctx: &RouterContext,
    model: &SignalingModel,
    code: DeskErrorCode,
    message: &str,
) {
    match SignalingModel::error(
        &model.request_id,
        model.signaling_type,
        None,
        model.from_connection_id.clone(),
        code,
        message,
    ) {
        Ok(error_model) => match serde_json::to_string(&error_model) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(e) => log::warn!(
                "[router] failed to serialise {:?} error response: {e} (request_id={})",
                model.signaling_type,
                model.request_id,
            ),
        },
        Err(e) => log::warn!(
            "[router] failed to build {:?} error response: {e} (request_id={})",
            model.signaling_type,
            model.request_id,
        ),
    }
}

/// Emit a success response to the browser, optionally carrying a body. The
/// counterpart to [`emit_error_response`] for routes the daemon answers itself
/// rather than forwarding to the worker.
pub(super) fn emit_success_response<T: serde::Serialize>(
    ctx: &RouterContext,
    model: &SignalingModel,
    data: Option<&T>,
) {
    match SignalingModel::success_response(
        &model.request_id,
        model.signaling_type,
        None,
        model.from_connection_id.clone(),
        data,
    ) {
        Ok(reply) => match serde_json::to_string(&reply) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(e) => log::warn!(
                "[router] failed to serialise {:?} response: {e} (request_id={})",
                model.signaling_type,
                model.request_id,
            ),
        },
        Err(e) => log::warn!(
            "[router] failed to build {:?} response: {e} (request_id={})",
            model.signaling_type,
            model.request_id,
        ),
    }
}

/// Synthesise an `Applied(width, height, refresh_hz)` success response
/// for a `ChangeDisplaySettings` request whose target already matches
/// the supervisor's cached mode. Used by the idempotent short-circuit:
/// when the browser asks for the resolution the IDD is already at, the
/// router replies inline without round-tripping to the worker. The
/// payload shape mirrors `signaling_proxy::build_virtual_display_response`'s
/// `Applied` branch (a `ChangeDisplaySettingsPayload` with `auto=false`)
/// so the browser cannot distinguish a real `Applied` from this synth.
pub(super) fn emit_applied_response(
    ctx: &RouterContext,
    model: &SignalingModel,
    width: u32,
    height: u32,
    refresh_hz: u32,
) {
    let payload = ChangeDisplaySettingsPayload {
        width,
        height,
        refresh_hz,
        auto: false,
    };
    match SignalingModel::success_response(
        &model.request_id,
        model.signaling_type,
        None,
        model.from_connection_id.clone(),
        Some(&payload),
    ) {
        Ok(reply) => match serde_json::to_string(&reply) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(e) => log::warn!(
                "[router] failed to serialise idempotent ChangeDisplaySettings reply: {e} \
                 (request_id={})",
                model.request_id,
            ),
        },
        Err(e) => log::warn!(
            "[router] failed to build idempotent ChangeDisplaySettings reply: {e} \
             (request_id={})",
            model.request_id,
        ),
    }
}

/// Virtual display: validate + forward a browser-issued
/// `ChangeDisplaySettings`. Inbound model carries
/// `ChangeDisplaySettingsPayload`; daemon checks (in order):
///
/// 1. Service-mode only — `ctx.virtual_display.is_none()` ⇒
///    `FEATURE_UNAVAILABLE` ("only supported in service mode").
/// 2. Toggle on — `settings.virtual_display.enabled == false` ⇒
///    `FEATURE_UNAVAILABLE` ("not enabled").
/// 3. Supervisor live — `is_active() == false` ⇒
///    `FEATURE_UNAVAILABLE` ("unavailable").
/// 4. Payload parses — `INVALID_PARAMS`.
/// 5. Auto + single-client — `payload.auto && pc_registry.len() != 1`
///    ⇒ `INVALID_STATE` ("auto requires single client connection").
///    Manual requests bypass this guard. Server-wide
///    `desk_settings.adaptive_web_page_resolution` is *not* consulted
///    here — see the inline comment in the function body for why.
/// 6. Auto refresh-hz fallback — `payload.auto && refresh_hz == 0`
///    substitutes `supervisor.last_refresh_hz()` (or 60 on cold start)
///    so the daemon owns the authoritative refresh value.
/// 7. Mode within bounds — `validate_mode` ⇒ `INVALID_PARAMS`.
/// 8. Auto throttle — applied *after* `validate_mode` so an invalid
///    payload never burns the next legitimate slot. Interval comes
///    from `settings.virtual_display.adaptive_throttle_ms`; 0 disables
///    the throttle. Manual requests bypass.
/// 9. Worker reachable — `send_to_worker` ⇒ `REMOTE_DESK_OFFLINE`.
///
/// On success the typed `SetVirtualDisplayMode` IPC carries
/// `request_id` + `connection_id` so the worker's reply (via
/// `WorkerToService::VirtualDisplayMode`) can be ferried back to the
/// matching browser PC. `route` itself always returns `Ok(())` — the
/// browser-visible failure is the error response we already emitted.
pub(super) async fn handle_change_display_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let supervisor = match ctx.virtual_display.as_ref() {
        Some(s) => s,
        None => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::FEATURE_UNAVAILABLE,
                "virtual display only supported in service mode",
            );
            return Ok(());
        }
    };
    let settings_snapshot = ctx.settings.read().await.clone();
    if !settings_snapshot.virtual_display.enabled {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "virtual display not enabled",
        );
        return Ok(());
    }
    if !supervisor.is_active().await {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::FEATURE_UNAVAILABLE,
            "virtual display unavailable",
        );
        return Ok(());
    }
    let payload = match model.get_data::<ChangeDisplaySettingsPayload>() {
        Ok(p) => p,
        Err(e) => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                &format!("bad ChangeDisplaySettings payload: {e}"),
            );
            return Ok(());
        }
    };

    // Auto-only gate: single-client. Refuses so a second browser
    // cannot fight the first one over the IDD resolution; manual
    // requests bypass this (operators can still drive resolution from
    // any tab through the regular UI). Placed before `validate_mode`
    // so a multi-client tab gets the more informative INVALID_STATE
    // error rather than a generic INVALID_PARAMS on malformed inputs.
    //
    // No `desk_settings.adaptive_web_page_resolution` check here:
    // that field is per-connection (the browser dialog collects it and
    // ships it via UpdateDeskSettings, which the daemon forwards to
    // the worker without writing back to `ctx.settings.desk`). Reading
    // the server-wide default would always see `false`, blocking the
    // browser's request no matter how the user toggled the checkbox.
    // The browser hook already gates on the same flag locally, so the
    // request only reaches here when the user has opted in; defence in
    // depth is still provided by `virtual_display.enabled`,
    // `supervisor.is_active`, the single-client guard above, and the
    // throttle below.
    if payload.auto && ctx.pc_registry.len().await != 1 {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::INVALID_STATE,
            "auto requires single client connection",
        );
        return Ok(());
    }

    // Auto refresh-hz fallback: the browser hook ships `refresh_hz=0`
    // to let the daemon supply the authoritative value (most recent
    // IDD echo, or 60 as a cold-start default). This stays inside the
    // `payload.auto` branch — a manual `refresh_hz=0` must keep its
    // original semantics (rejected by `validate_mode` as a zero
    // dimension), which the regression test
    // `manual_zero_refresh_still_invalid` pins.
    let effective_refresh_hz = if payload.auto && payload.refresh_hz == 0 {
        let cached = supervisor.last_refresh_hz();
        if cached == 0 { 60 } else { cached }
    } else {
        payload.refresh_hz
    };

    let mode = VirtualDisplayMode {
        width: payload.width,
        height: payload.height,
        refresh_hz: effective_refresh_hz,
    };
    if let Err(e) = validate_mode(mode) {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::INVALID_PARAMS,
            &format!("invalid mode: {e}"),
        );
        return Ok(());
    }

    // Idempotent short-circuit: if the request's (width, height,
    // effective_refresh_hz) exactly matches the supervisor's cached
    // mode (last seen via the worker's `VirtualDisplayMode::Applied`
    // echo), skip the worker IPC entirely and synthesise an Applied
    // response inline. Rationale: the worker's `set_mode` path always
    // triggers an IDD Departure+Arrival driver cycle plus a WGC capture
    // restart, even when the resolution is unchanged. The browser's
    // adaptive-resolution hook frequently re-fires on devicePixelRatio
    // jitter at the same wrapper size, so dropping these no-op
    // round-trips removes a large source of visible WGC restart
    // hitches.
    //
    // Placed *after* `validate_mode` (so an invalid payload still
    // returns INVALID_PARAMS rather than masking the validation bug as
    // a fake Applied) and *before* `try_consume_auto_slot` (an
    // idempotent hit has zero IDD cost, so it should not consume a
    // throttle slot the operator has reserved for real changes).
    // `last_known_mode()` returns `None` until the worker has reported
    // a fully-formed Applied (all three components non-zero) AND the
    // current attach generation has not been torn down — dimensions
    // are cleared on every attach lifecycle transition, see
    // `VirtualDisplaySupervisor::reset_known_dimensions` doc.
    if let Some((cached_w, cached_h, cached_hz)) = supervisor.last_known_mode()
        && payload.width == cached_w
        && payload.height == cached_h
        && effective_refresh_hz == cached_hz
    {
        log::debug!(
            "[router] ChangeDisplaySettings idempotent hit {cached_w}x{cached_h}@{cached_hz}; \
             skipping worker IPC (request_id={})",
            model.request_id,
        );
        emit_applied_response(ctx, model, cached_w, cached_h, cached_hz);
        return Ok(());
    }

    // Throttle is the last gate before IPC. Placed *after*
    // `validate_mode` so an invalid auto payload never burns the
    // operator's next legitimate slot.
    if payload.auto {
        let min_interval =
            Duration::from_millis(settings_snapshot.virtual_display.adaptive_throttle_ms);
        if !supervisor.try_consume_auto_slot(tokio::time::Instant::now(), min_interval) {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_STATE,
                "auto change throttled",
            );
            return Ok(());
        }
    }

    let connection_id = model.from_connection_id.clone().unwrap_or_default();
    let ipc_payload = SetVirtualDisplayModePayload {
        request_id: model.request_id.clone(),
        connection_id,
        width: payload.width,
        height: payload.height,
        refresh_hz: effective_refresh_hz,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::SetVirtualDisplayMode(ipc_payload))
        .await
    {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::REMOTE_DESK_OFFLINE,
            &format!("worker unavailable: {e}"),
        );
    }
    Ok(())
}

/// Parse the inbound `EnablePrivateScreen` payload and ship
/// it to the worker as typed [`ServiceToWorker::EnablePrivateScreen`].
/// Replaces the legacy `SignalingMessage` opaque envelope.
///
/// Parse / send failures are non-fatal for the WS connection — they
/// only prevent the toggle from reaching the worker, which is logged.
pub(super) async fn handle_enable_private_screen_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let from_connection_id = match model.from_connection_id.as_deref() {
        Some(id) => id.to_string(),
        None => {
            log::warn!(
                "[router] EnablePrivateScreen missing from_connection_id; ignoring \
                 (request_id={})",
                model.request_id,
            );
            return Ok(());
        }
    };
    let data = match model.get_data::<EnablePrivateScreenData>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!(
                "[router] EnablePrivateScreen payload parse failed for {from_connection_id}: \
                 {e}; ignoring"
            );
            return Ok(());
        }
    };
    let payload = EnablePrivateScreenPayload {
        connection_id: from_connection_id.clone(),
        enable: data.enable,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::EnablePrivateScreen(payload))
        .await
    {
        log::warn!(
            "[router] failed to send typed EnablePrivateScreen for {from_connection_id}: {e}",
        );
    }
    Ok(())
}

/// Parses the inbound `UpdateDeskSettings` payload, fans out the
/// media-relevant knobs as `UpdateMediaSettings` IPC (so the
/// per-connection encoder pipeline retunes live), applies the
/// connection-scoped adaptive-bitrate toggle, and ships the full
/// settings to the worker as typed
/// [`ServiceToWorker::UpdateDeskSettings`] (the worker keeps that
/// dispatch path as a hook; it currently applies nothing from it).
///
/// `adaptive_bitrate` deliberately does **not** ride the global
/// fan-out: it is a per-browser session preference (persisted in the
/// browser, not server-side), so it only updates the state of the
/// connection that sent the message. fps / quality / dirty_rect keep
/// their existing fan-out-to-all semantics.
pub(super) async fn handle_update_desk_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let settings = match model.get_data::<DeskSettings>() {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[router] UpdateDeskSettings payload parse failed: {e}; dropping \
                 (no media retune, no worker forward)"
            );
            return Ok(());
        }
    };

    ctx.pc_registry
        .broadcast_media_settings_update(
            &ctx.worker_mgr,
            Some(settings.video_fps),
            None,
            Some(settings.video_quality),
            Some(settings.enable_dirty_rect),
        )
        .await;

    // Connection-scoped adaptive-bitrate toggle: lock → flip → ship
    // the Clear (if any) → commit, all under the state lock so a
    // stale SetCap from the RTCP task can never land after the Clear
    // (see `daemon::bitrate_controller` for the ordering contract).
    if let Some(conn_id) = model.from_connection_id.as_deref()
        && let Some(pc_ctx) = ctx.pc_registry.get(conn_id).await
    {
        let adaptive = { Arc::clone(&pc_ctx.read().await.adaptive_bitrate) };
        let mut state = adaptive.state.lock().await;
        if let Some(directive) = state.set_enabled_and_decide_clear(settings.adaptive_bitrate) {
            crate::daemon::pc_manager::send_cap_directive(
                &ctx.worker_mgr,
                conn_id,
                directive,
                &mut state,
            )
            .await;
        }
    }

    let from_connection_id = model
        .from_connection_id
        .clone()
        .unwrap_or_else(|| "<unscoped>".to_string());
    let payload = UpdateDeskSettingsPayload {
        connection_id: from_connection_id.clone(),
        settings,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_worker(ServiceToWorker::UpdateDeskSettings(payload))
        .await
    {
        log::warn!(
            "[router] failed to send typed UpdateDeskSettings for {from_connection_id}: {e}",
        );
    }
    Ok(())
}

// ---- Manager-plane typed-IPC dispatch helpers ----
//
// All five share the same skeleton — pull `from_connection_id` (the
// browser's PC ID), build the typed `ServiceToWorker::Manager*Request`
// payload, ship it via `WorkerManager::send_to_worker`. Differences are
// only in payload type and whether the inbound model carries a body.
// The `request_id` is echoed verbatim so the worker's
// `ManagerResponseRefPayload` / typed-response payload can correlate.
// Errors are non-fatal for the WS connection: parse / send failures
// log + drop, same fail-soft semantics the SignalingMessage bridge
// had.

/// Helper: extract `from_connection_id` from an inbound model when
/// the routing path requires one (e.g. terminal session traffic that
/// keys per-PTY on the originating browser/terminal connection).
/// Missing => log and return None so the caller drops the message.
pub(super) fn require_from_connection_id<'a>(
    model: &'a SignalingModel,
    signaling_type_name: &'static str,
) -> Option<&'a str> {
    match model.from_connection_id.as_deref() {
        Some(id) => Some(id),
        None => {
            log::warn!(
                "[router] {signaling_type_name} missing from_connection_id; ignoring \
                 (request_id={})",
                model.request_id,
            );
            None
        }
    }
}

/// Clone `from_connection_id` for non-interactive manager requests that still
/// support request-id-only REST correlation.
pub(super) fn optional_from_connection_id(model: &SignalingModel) -> Option<String> {
    model.from_connection_id.clone()
}
