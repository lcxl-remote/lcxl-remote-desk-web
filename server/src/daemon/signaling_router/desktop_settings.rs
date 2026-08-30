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
    let response_type =
        response_type_for_request(model.signaling_type).unwrap_or(SignalingType::Error);
    match SignalingModel::error(
        &model.request_id,
        response_type,
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

/// Emit the protocol-level `Error` used when a frame is rejected before its
/// request-specific handler is entered (for example at door1).
pub(super) fn emit_standard_error_response(
    ctx: &RouterContext,
    model: &SignalingModel,
    code: DeskErrorCode,
    message: &str,
) {
    match SignalingModel::error(
        &model.request_id,
        SignalingType::Error,
        None,
        model.from_connection_id.clone(),
        code,
        message,
    ) {
        Ok(error_model) => match serde_json::to_string(&error_model) {
            Ok(text) => {
                let _ = ctx.outbound_tx.send(text);
            }
            Err(error) => log::warn!(
                "[router] failed to serialise protocol Error: {error} (request_id={})",
                model.request_id,
            ),
        },
        Err(error) => log::warn!(
            "[router] failed to build protocol Error: {error} (request_id={})",
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
    connection_epoch: String,
    width: u32,
    height: u32,
    refresh_hz: u32,
) {
    let response_type = SignalingType::DisplaySettingsChanged;
    let payload = ChangeDisplaySettingsPayload {
        connection_epoch,
        width,
        height,
        refresh_hz,
        auto: false,
    };
    match SignalingModel::success_response(
        &model.request_id,
        response_type,
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
    let Some(connection_id) = model.from_connection_id.as_deref() else {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::INVALID_PARAMS,
            "ChangeDisplaySettings requires a source connection",
        );
        return Ok(());
    };
    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::REMOTE_DESK_OFFLINE,
            "remote desktop connection is no longer active",
        );
        return Ok(());
    };
    if pc.read().await.connection_epoch != payload.connection_epoch {
        return Ok(());
    }

    // Auto-only gate: single-client. Refuses so a second browser
    // cannot fight the first one over the IDD resolution; manual
    // requests bypass this (operators can still drive resolution from
    // any tab through the regular UI). Placed before `validate_mode`
    // so a multi-client tab gets the dedicated
    // ADAPTIVE_RESOLUTION_REQUIRES_SINGLE_CLIENT error rather than a generic
    // INVALID_PARAMS on malformed inputs.
    //
    // No host setting check here: adaptive web resolution is a local
    // browser preference and this request is already connection-scoped.
    // Reading a server-wide default would always see `false`, blocking the
    // browser's request no matter how the user toggled the checkbox.
    // The browser hook already gates on the same flag locally, so the
    // request only reaches here when the user has opted in; defence in
    // depth is still provided by `virtual_display.enabled`,
    // `supervisor.is_active`, the single-client guard above, and the
    // throttle below.
    if payload.auto && ctx.pc_registry.remote_desktop_count().await != 1 {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::ADAPTIVE_RESOLUTION_REQUIRES_SINGLE_CLIENT,
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
        emit_applied_response(
            ctx,
            model,
            payload.connection_epoch.clone(),
            cached_w,
            cached_h,
            cached_hz,
        );
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
        connection_id: connection_id.clone(),
        connection_epoch: payload.connection_epoch,
        width: payload.width,
        height: payload.height,
        refresh_hz: effective_refresh_hz,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_connection_worker(
            &connection_id,
            ServiceToWorker::SetVirtualDisplayMode(ipc_payload),
        )
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

/// Parse the inbound `SetPrivateScreenVisibility` payload and ship
/// it to the worker as typed [`ServiceToWorker::SetPrivateScreenVisibility`].
/// Replaces the legacy `SignalingMessage` opaque envelope.
///
/// Parse / send failures are non-fatal for the WS connection — they
/// only prevent the toggle from reaching the worker, which is logged.
pub(super) async fn handle_set_private_screen_visibility_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let from_connection_id = match model.from_connection_id.as_deref() {
        Some(id) => id.to_string(),
        None => {
            log::warn!(
                "[router] SetPrivateScreenVisibility missing from_connection_id \
                 (request_id={})",
                model.request_id,
            );
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                "private screen request requires a source connection",
            );
            return Ok(());
        }
    };
    let data = match model.get_data::<SetPrivateScreenVisibilityData>() {
        Ok(d) => d,
        Err(e) => {
            log::warn!(
                "[router] SetPrivateScreenVisibility payload parse failed for \
                 {from_connection_id}: {e}"
            );
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                &format!("bad SetPrivateScreenVisibility payload: {e}"),
            );
            return Ok(());
        }
    };
    let payload = SetPrivateScreenVisibilityPayload {
        request_id: model.request_id.clone(),
        connection_id: from_connection_id.clone(),
        visible: data.visible,
    };
    if let Err(e) = ctx
        .worker_mgr
        .send_to_connection_worker(
            &from_connection_id,
            ServiceToWorker::SetPrivateScreenVisibility(payload),
        )
        .await
    {
        log::warn!(
            "[router] failed to send typed SetPrivateScreenVisibility for \
             {from_connection_id}: {e}",
        );
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::REMOTE_DESK_OFFLINE,
            &format!("worker unavailable: {e}"),
        );
    }
    Ok(())
}

fn video_wire_codec(encoder: desk_signal_facade::model::media_capability::VideoEncoderId) -> u8 {
    use desk_signal_facade::model::media_capability::VideoEncoderId;
    match encoder {
        VideoEncoderId::X264 | VideoEncoderId::OpenH264 => 0,
        VideoEncoderId::Vp8 => 1,
        VideoEncoderId::Vp9 => 2,
        VideoEncoderId::Av1 => 3,
    }
}

fn send_settings_applied(
    ctx: &RouterContext,
    model: &SignalingModel,
    payload: &RemoteSessionSettingsApplied,
) -> Result<(), RouterError> {
    let reply = SignalingModel::success_response(
        &model.request_id,
        SignalingType::RemoteSessionSettingsApplied,
        None,
        model.from_connection_id.clone(),
        Some(payload),
    )
    .map_err(DeskError::from)?;
    let text = serde_json::to_string(&reply).map_err(DeskError::from)?;
    let _ = ctx.outbound_tx.send(text);
    Ok(())
}

fn send_audio_state_snapshot(
    outbound: &broadcast::Sender<String>,
    connection_id: &str,
    payload: &SystemAudioCaptureStateData,
) {
    if let Ok(model) = SignalingModel::new_request(
        SignalingType::SystemAudioCaptureStateChanged,
        Some(connection_id.to_string()),
        Some(payload),
    ) && let Ok(text) = serde_json::to_string(&model)
    {
        let _ = outbound.send(text);
    }
}

/// Commit one successful media group as soon as its matching worker terminal
/// arrives. Video and audio deliberately call this independently: a video
/// restart must not wait behind a local audio-permission prompt before becoming
/// the accepted recovery snapshot.
async fn commit_successful_settings_group(
    ctx: &RouterContext,
    connection_id: &str,
    connection_epoch: &str,
    request_id: &str,
    media_kind: MediaKind,
    requested: &RemoteSessionSettings,
    candidate: &StartMediaPayload,
) -> bool {
    let Some(pc_ctx) = ctx.pc_registry.get(connection_id).await else {
        return false;
    };
    let mut pc = pc_ctx.write().await;
    if pc.connection_epoch != connection_epoch {
        return false;
    }
    let media_coordinator = Arc::clone(&pc.media_coordinator);
    let mut coordinator = media_coordinator.lock().await;
    if coordinator.current_apply_request_id.as_deref() != Some(request_id) {
        return false;
    }
    let Some(mut baseline) = coordinator.accepted_baseline.clone() else {
        return false;
    };
    let Some(mut recovery) = coordinator.recovery_start_media.clone() else {
        return false;
    };

    match media_kind {
        MediaKind::Video => {
            if baseline.video_quality != requested.video_quality {
                coordinator.adaptive_quality_override = None;
            }
            baseline.image_capture = requested.image_capture.clone();
            baseline.video_device_name = requested.video_device_name.clone();
            baseline.show_mouse = requested.show_mouse;
            baseline.video_encoder = requested.video_encoder;
            baseline.video_quality = requested.video_quality;
            baseline.video_fps = requested.video_fps;
            baseline.enable_dirty_rect = requested.enable_dirty_rect;
            baseline.adaptive_bitrate = requested.adaptive_bitrate;
            recovery.video_generation = candidate.video_generation;
            recovery.video_device = candidate.video_device.clone();
            recovery.video_encoder = candidate.video_encoder;
            recovery.fps = candidate.fps;
            recovery.quality = candidate.quality;
            recovery.image_capture = candidate.image_capture.clone();
            recovery.enable_dirty_rect = candidate.enable_dirty_rect;
            recovery.show_mouse = candidate.show_mouse;
            coordinator.video.lifecycle = MediaSlotLifecycle::Stable;
            coordinator.video.pending_generation = None;
            coordinator.video_terminal_waiter = None;
        }
        MediaKind::Audio => {
            baseline.audio = requested.audio.clone();
            recovery.audio_generation = candidate.audio_generation;
            recovery.audio = candidate.audio.clone();
            coordinator.audio.lifecycle = MediaSlotLifecycle::Stable;
            coordinator.audio.pending_generation = None;
            coordinator.audio_terminal_waiter = None;
            coordinator.audio_expected_terminal = None;
        }
    }

    coordinator.accepted_baseline = Some(baseline.clone());
    coordinator.recovery_start_media = Some(recovery.clone());
    *pc.cached_start_media.write().await = Some(recovery);
    let host_settings = pc.host_settings.clone();
    pc.host_settings = baseline.merge_into_host_settings(&host_settings);
    true
}

/// Wait for the one allowed accepted-snapshot recovery to reach its current
/// video/audio terminals. Event handlers and this function serialize through
/// the per-connection coordinator, so a terminal that arrives just before the
/// waiter is installed is observed through `actual_*_phase` instead of lost.
async fn wait_for_recovery_terminals(
    ctx: &RouterContext,
    connection_id: &str,
    apply_deadline: tokio::time::Instant,
) -> bool {
    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        return false;
    };
    let pc = pc.read().await;
    let mut coordinator = pc.media_coordinator.lock().await;
    let expect_audio = coordinator
        .recovery_start_media
        .as_ref()
        .is_some_and(|payload| payload.audio.is_some());
    let video_ready = coordinator.actual_video_phase == Some(MediaPipelinePhase::Streaming);
    let expected_audio = if expect_audio {
        AudioPipelinePhase::Active
    } else {
        AudioPipelinePhase::Off
    };
    // A video-only StartMedia bootstrap deliberately emits no synthetic audio
    // `off` event. The closed fence plus an accepted recovery payload with
    // `audio=None` is already the authoritative terminal state.
    let audio_ready = !expect_audio || coordinator.actual_audio_phase == Some(expected_audio);
    let video_waiter = if video_ready {
        None
    } else {
        let generation = coordinator.video.generation;
        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator.video_terminal_waiter = Some((generation, tx));
        Some(rx)
    };
    let audio_waiter = if audio_ready {
        None
    } else {
        let generation = coordinator.audio.generation;
        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator.audio_terminal_waiter = Some((generation, tx));
        coordinator.audio_expected_terminal = Some(expected_audio);
        Some(rx)
    };
    drop(coordinator);
    drop(pc);
    let terminal_deadline = std::cmp::min(
        apply_deadline,
        tokio::time::Instant::now() + Duration::from_secs(10),
    );

    let video = async {
        match video_waiter {
            None => true,
            Some(waiter) => {
                tokio::time::timeout_at(terminal_deadline, waiter)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .and_then(Result::ok)
                    == Some(MediaPipelinePhase::Streaming)
            }
        }
    };
    let audio = async {
        match audio_waiter {
            None => true,
            Some(waiter) => {
                tokio::time::timeout_at(terminal_deadline, waiter)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .and_then(Result::ok)
                    == Some(expected_audio)
            }
        }
    };
    let (video, audio) = tokio::join!(video, audio);
    video && audio
}

/// Restore only the previously accepted audio pipeline after an audio-group
/// apply failed. A pure audio failure must never restart the healthy video
/// encoder. When audio was previously off, the closed output fence already is
/// the accepted state and no worker command is needed.
async fn recover_audio_from_accepted_snapshot(
    ctx: &RouterContext,
    connection_id: &str,
    connection_epoch: &str,
    apply_deadline: tokio::time::Instant,
) -> bool {
    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        return false;
    };
    let pc_guard = pc.read().await;
    if pc_guard.connection_epoch != connection_epoch {
        return false;
    }
    let mut coordinator = pc_guard.media_coordinator.lock().await;
    let Some(mut recovery) = coordinator.recovery_start_media.clone() else {
        return false;
    };
    if recovery.audio.is_none() {
        coordinator.audio.lifecycle = MediaSlotLifecycle::Stable;
        coordinator.audio.pending_generation = None;
        coordinator.audio_expected_terminal = None;
        coordinator.audio_desired_active = false;
        coordinator.actual_audio_phase = Some(AudioPipelinePhase::Off);
        return true;
    }
    let Some(next_generation) = coordinator.audio.generation.checked_add(1) else {
        return false;
    };
    recovery.audio_generation = next_generation;
    let (tx, rx) = tokio::sync::oneshot::channel();
    coordinator.audio.generation = next_generation;
    coordinator.audio.lifecycle = MediaSlotLifecycle::Transitioning;
    coordinator.audio.pending_generation = Some(next_generation);
    coordinator.audio_desired_active = true;
    coordinator.audio_expected_terminal = Some(AudioPipelinePhase::Active);
    coordinator.audio_terminal_waiter = Some((next_generation, tx));
    pc_guard.media_output_fence.write().await.audio_open = false;
    drop(coordinator);
    drop(pc_guard);

    let sent = ctx
        .worker_mgr
        .send_to_interactive_connection_worker(
            connection_id,
            ServiceToWorker::ApplyMediaSettings(ApplyMediaSettingsPayload {
                source_request_id: None,
                connection_id: connection_id.to_string(),
                connection_epoch: connection_epoch.to_string(),
                media_kind: MediaKind::Audio,
                action: MediaSettingsAction::Start {
                    new_generation: next_generation,
                    settings: recovery.clone(),
                },
            }),
        )
        .await
        .is_ok();
    let active = sent
        && tokio::time::timeout_at(
            std::cmp::min(
                apply_deadline,
                tokio::time::Instant::now() + Duration::from_secs(10),
            ),
            rx,
        )
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(Result::ok)
            == Some(AudioPipelinePhase::Active);

    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        return false;
    };
    let pc_guard = pc.read().await;
    if pc_guard.connection_epoch != connection_epoch {
        return false;
    }
    let mut coordinator = pc_guard.media_coordinator.lock().await;
    coordinator.audio_terminal_waiter = None;
    coordinator.audio_expected_terminal = None;
    coordinator.audio.lifecycle = MediaSlotLifecycle::Stable;
    coordinator.audio.pending_generation = None;
    if active {
        coordinator.recovery_start_media = Some(recovery.clone());
        *pc_guard.cached_start_media.write().await = Some(recovery);
    } else {
        coordinator.audio_desired_active = false;
    }
    active
}

/// Answer has already been sent when this is called. The local approval may
/// wait for the host UI without delaying SDP/ICE setup; all results are
/// revalidated against epoch + approval id + the current policy generation.
pub(super) fn spawn_initial_audio_authorization(ctx: &RouterContext, connection_id: String) {
    let registry = ctx.pc_registry.clone();
    let worker_mgr = ctx.worker_mgr.clone();
    let policy = ctx.policy.clone();
    let hub = Arc::clone(&ctx.host_control_hub);
    let outbound = ctx.outbound_tx.clone();
    tokio::spawn(async move {
        let Some(pc) = registry.get(&connection_id).await else {
            return;
        };
        let pc_guard = pc.read().await;
        let connection_epoch = pc_guard.connection_epoch.clone();
        let access_ceiling = pc_guard.signaling_state.read().await.access_ceiling.clone();
        let media_coordinator = Arc::clone(&pc_guard.media_coordinator);
        drop(pc_guard);
        let approval_id = uuid::Uuid::new_v4().to_string();
        let candidate = {
            let mut coordinator = media_coordinator.lock().await;
            let Some(candidate) = coordinator.pending_audio_candidate.clone() else {
                return;
            };
            coordinator.pending_audio_approval_id = Some(approval_id.clone());
            candidate
        };
        send_audio_state_snapshot(
            &outbound,
            &connection_id,
            &SystemAudioCaptureStateData {
                connection_epoch: connection_epoch.clone(),
                state: SystemAudioCaptureState::Starting,
                accepted_audio: None,
                resolved_audio_device_id: None,
                error_code: None,
            },
        );

        let decided = policy.capability(SecurityPermissionType::SystemAudioCapture);
        let permission =
            effective_permission(access_ceiling.as_ref(), decided.permission, |ceiling| {
                ceiling.allow_system_audio_capture
            });
        let approved = check_security_permission(
            &policy,
            &hub,
            permission,
            decided.generation,
            SecurityPermissionType::SystemAudioCapture,
            Some(connection_id.clone()),
            access_ceiling.is_some(),
        )
        .await;
        let current = policy.capability(SecurityPermissionType::SystemAudioCapture);
        let current_permission =
            effective_permission(access_ceiling.as_ref(), current.permission, |ceiling| {
                ceiling.allow_system_audio_capture
            });
        let still_approved = approved
            && (current.generation == decided.generation || current_permission == Some(true));

        let Some(pc) = registry.get(&connection_id).await else {
            return;
        };
        let pc_guard = pc.read().await;
        if pc_guard.connection_epoch != connection_epoch {
            return;
        }
        let mut coordinator = pc_guard.media_coordinator.lock().await;
        if coordinator.pending_audio_approval_id.as_deref() != Some(&approval_id) {
            return;
        }
        if !still_approved {
            coordinator.pending_audio_approval_id = None;
            coordinator.pending_audio_candidate = None;
            drop(coordinator);
            drop(pc_guard);
            send_audio_state_snapshot(
                &outbound,
                &connection_id,
                &SystemAudioCaptureStateData {
                    connection_epoch,
                    state: SystemAudioCaptureState::Denied,
                    accepted_audio: None,
                    resolved_audio_device_id: None,
                    error_code: Some(DeskErrorCode::PERMISSION_ERROR),
                },
            );
            return;
        }

        let Some(mut start) = coordinator.recovery_start_media.clone() else {
            return;
        };
        let Some(next_generation) = coordinator.audio.generation.checked_add(1) else {
            return;
        };
        start.audio_generation = next_generation;
        start.audio = Some(StartAudioSettings {
            codec: MediaCodec::Opus,
            pipeline: candidate.clone(),
        });
        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator.audio.generation = next_generation;
        coordinator.audio.lifecycle = MediaSlotLifecycle::Transitioning;
        coordinator.audio.pending_generation = Some(next_generation);
        coordinator.audio_desired_active = true;
        coordinator.audio_expected_terminal = Some(AudioPipelinePhase::Active);
        coordinator.audio_terminal_waiter = Some((next_generation, tx));
        drop(coordinator);
        drop(pc_guard);

        let command = ApplyMediaSettingsPayload {
            source_request_id: None,
            connection_id: connection_id.clone(),
            connection_epoch: connection_epoch.clone(),
            media_kind: MediaKind::Audio,
            action: MediaSettingsAction::Start {
                new_generation: next_generation,
                settings: start.clone(),
            },
        };
        let sent = worker_mgr
            .send_to_interactive_connection_worker(
                &connection_id,
                ServiceToWorker::ApplyMediaSettings(command),
            )
            .await
            .is_ok();
        let active = sent
            && tokio::time::timeout(Duration::from_secs(10), rx)
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(Result::ok)
                .is_some_and(|phase| phase == AudioPipelinePhase::Active);

        let Some(pc) = registry.get(&connection_id).await else {
            return;
        };
        let pc_guard = pc.read().await;
        if pc_guard.connection_epoch != connection_epoch {
            return;
        }
        let mut coordinator = pc_guard.media_coordinator.lock().await;
        if coordinator.pending_audio_approval_id.as_deref() != Some(&approval_id) {
            return;
        }
        coordinator.pending_audio_approval_id = None;
        coordinator.pending_audio_candidate = None;
        coordinator.audio_terminal_waiter = None;
        coordinator.audio_expected_terminal = None;
        let (state, accepted_audio, error_code) = if active {
            coordinator.audio.lifecycle = MediaSlotLifecycle::Stable;
            coordinator.audio.pending_generation = None;
            if let Some(baseline) = coordinator.accepted_baseline.as_mut() {
                baseline.audio = Some(candidate.clone());
            }
            coordinator.recovery_start_media = Some(start.clone());
            *pc_guard.cached_start_media.write().await = Some(start);
            (SystemAudioCaptureState::Active, Some(candidate), None)
        } else {
            coordinator.audio_desired_active = false;
            (
                SystemAudioCaptureState::Failed,
                None,
                Some(DeskErrorCode::ACTION_NEED_RETRY),
            )
        };
        drop(coordinator);
        drop(pc_guard);
        send_audio_state_snapshot(
            &outbound,
            &connection_id,
            &SystemAudioCaptureStateData {
                connection_epoch,
                state,
                accepted_audio,
                resolved_audio_device_id: None,
                error_code,
            },
        );
    });
}

/// Apply a complete controller-owned baseline to exactly one connection and
/// always terminate a parsed 301 with a structured 302 response.
pub(super) async fn handle_apply_remote_session_settings_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request = match model.get_data::<ApplyRemoteSessionSettings>() {
        Ok(request) => request,
        Err(e) => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                &format!("bad ApplyRemoteSessionSettings payload: {e}"),
            );
            return Ok(());
        }
    };
    let connection_id = match model.from_connection_id.as_deref() {
        Some(connection_id) => connection_id,
        None => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                "ApplyRemoteSessionSettings requires an admitted connection",
            );
            return Ok(());
        }
    };
    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::REMOTE_DESK_OFFLINE,
            "remote desktop connection is no longer active",
        );
        return Ok(());
    };

    let pc = pc.write().await;
    if request.connection_epoch != pc.connection_epoch {
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::ACTION_NEED_RETRY,
            "stale remote-session connection epoch",
        );
        return Ok(());
    }
    let apply_deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let old = pc.host_settings.clone();
    let requested = request.settings.clone();
    // The RTP track is negotiated from the accepted controller baseline, not
    // from the host-global defaults. In particular, an initial automatic Offer
    // leaves `host_settings.video_encoder` as None even though the track is
    // already bound to a concrete wire codec.
    let (accepted_baseline, adaptive_quality_override) = {
        let coordinator = pc.media_coordinator.lock().await;
        (
            coordinator.accepted_baseline.clone(),
            coordinator.adaptive_quality_override,
        )
    };
    let Some(accepted_baseline) = accepted_baseline else {
        drop(pc);
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::ACTION_NEED_RETRY,
            "remote media baseline is not installed",
        );
        return Ok(());
    };
    let needs_reconnect = video_wire_codec(accepted_baseline.video_encoder)
        != video_wire_codec(requested.video_encoder);
    if needs_reconnect {
        drop(pc);
        return send_settings_applied(
            ctx,
            model,
            &RemoteSessionSettingsApplied {
                connection_epoch: request.connection_epoch,
                effects: RemoteSessionSettingsEffects {
                    video: VideoSettingsEffect::Unchanged,
                    audio: AudioSettingsEffect::Unchanged,
                    connection: ConnectionSettingsEffect::NeedsReconnect,
                },
                // Nothing was applied: report the authoritative current
                // baseline. The controller already retains its requested
                // settings and uses them to build the replacement PC.
                baseline_settings: accepted_baseline,
                runtime_overrides: RemoteSessionSettingsRuntimeOverrides {
                    adaptive_video_quality: adaptive_quality_override,
                },
                errors: Vec::new(),
            },
        );
    }

    let merged = requested.merge_into_host_settings(&old);
    let video_restart = accepted_baseline.image_capture != requested.image_capture
        || accepted_baseline.video_device_name != requested.video_device_name
        || accepted_baseline.video_encoder != requested.video_encoder;
    let video_changed = video_restart
        || accepted_baseline.video_fps != requested.video_fps
        || accepted_baseline.video_quality != requested.video_quality
        || accepted_baseline.enable_dirty_rect != requested.enable_dirty_rect
        || accepted_baseline.show_mouse != requested.show_mouse;
    let audio_changed = accepted_baseline.audio != requested.audio;
    let Some(old_recovery) = pc.cached_start_media.read().await.clone() else {
        drop(pc);
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::ACTION_NEED_RETRY,
            "remote media baseline is not installed",
        );
        return Ok(());
    };
    let mut candidate = old_recovery.clone();
    candidate.video_device =
        (!merged.video_device_name.is_empty()).then(|| merged.video_device_name.clone());
    candidate.video_encoder = requested.video_encoder;
    candidate.fps = merged.video_fps;
    candidate.quality = merged.video_quality;
    candidate.audio = requested.audio.clone().map(|pipeline| StartAudioSettings {
        codec: MediaCodec::Opus,
        pipeline,
    });
    candidate.image_capture = requested.image_capture.clone();
    candidate.enable_dirty_rect = merged.enable_dirty_rect;
    candidate.show_mouse = merged.show_mouse;

    let media_coordinator = Arc::clone(&pc.media_coordinator);
    let mut coordinator = media_coordinator.lock().await;
    if coordinator.current_apply_request_id.is_some() {
        drop(coordinator);
        drop(pc);
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::ACTION_NEED_RETRY,
            "another media settings apply is still in progress",
        );
        return Ok(());
    }
    coordinator.current_apply_request_id = Some(model.request_id.clone());
    let mut video_waiter = None;
    let mut audio_waiter = None;
    let mut video_command = None;
    let mut audio_command = None;
    let mut audio_previous_generation = None;

    if video_changed {
        let current = coordinator.video.generation;
        let target = if video_restart {
            match current.checked_add(1) {
                Some(next) => next,
                None => {
                    coordinator.current_apply_request_id = None;
                    drop(coordinator);
                    drop(pc);
                    emit_error_response(
                        ctx,
                        model,
                        DeskErrorCode::ACTION_NEED_RETRY,
                        "video generation exhausted; reconnect required",
                    );
                    return Ok(());
                }
            }
        } else {
            current
        };
        candidate.video_generation = target;
        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator.video.lifecycle = MediaSlotLifecycle::Transitioning;
        coordinator.video.pending_generation = Some(target);
        coordinator.video.generation = target;
        coordinator.video_terminal_waiter = Some((target, tx));
        video_waiter = Some(rx);
        let action = if video_restart {
            MediaSettingsAction::Restart {
                current_generation: current,
                new_generation: target,
                settings: candidate.clone(),
            }
        } else {
            MediaSettingsAction::LiveVideo {
                target_generation: current,
                settings: UpdateMediaSettingsPayload {
                    connection_id: connection_id.to_string(),
                    connection_epoch: request.connection_epoch.clone(),
                    video_generation: current,
                    fps: Some(merged.video_fps),
                    bitrate_kbps: None,
                    quality: Some(merged.video_quality),
                    enable_dirty_rect: Some(merged.enable_dirty_rect),
                    show_mouse: Some(merged.show_mouse),
                },
            }
        };
        video_command = Some(ApplyMediaSettingsPayload {
            source_request_id: Some(model.request_id.clone()),
            connection_id: connection_id.to_string(),
            connection_epoch: request.connection_epoch.clone(),
            media_kind: MediaKind::Video,
            action,
        });
        if video_restart {
            // Linearize the encoder-generation swap before the Restart enters
            // the worker FIFO. From this point onward old-generation frames
            // are stale, while the replacement pipeline's first IDR is allowed
            // through even if it races ahead of its Streaming terminal event.
            // Without this advance every frame from a successful same-codec
            // implementation switch (for example OpenH264 <-> X264) is
            // rejected by `write_video_frame`, leaving the browser frozen on
            // the last frame from the previous generation.
            let mut fence = pc.media_output_fence.write().await;
            fence.video_epoch = request.connection_epoch.clone();
            fence.video_generation = target;
        }
    }

    if audio_changed {
        let current = coordinator.audio.generation;
        audio_previous_generation = Some(current);
        let (target, action) = match (&accepted_baseline.audio, &candidate.audio) {
            (Some(_), None) => (
                current,
                MediaSettingsAction::Stop {
                    target_generation: current,
                },
            ),
            (None, Some(_)) => {
                let Some(next) = current.checked_add(1) else {
                    coordinator.current_apply_request_id = None;
                    drop(coordinator);
                    drop(pc);
                    emit_error_response(
                        ctx,
                        model,
                        DeskErrorCode::ACTION_NEED_RETRY,
                        "audio generation exhausted; reconnect required",
                    );
                    return Ok(());
                };
                candidate.audio_generation = next;
                (
                    next,
                    MediaSettingsAction::Start {
                        new_generation: next,
                        settings: candidate.clone(),
                    },
                )
            }
            (Some(_), Some(_)) => {
                let Some(next) = current.checked_add(1) else {
                    coordinator.current_apply_request_id = None;
                    drop(coordinator);
                    drop(pc);
                    emit_error_response(
                        ctx,
                        model,
                        DeskErrorCode::ACTION_NEED_RETRY,
                        "audio generation exhausted; reconnect required",
                    );
                    return Ok(());
                };
                candidate.audio_generation = next;
                (
                    next,
                    MediaSettingsAction::Restart {
                        current_generation: current,
                        new_generation: next,
                        settings: candidate.clone(),
                    },
                )
            }
            (None, None) => {
                coordinator.current_apply_request_id = None;
                drop(coordinator);
                drop(pc);
                emit_error_response(
                    ctx,
                    model,
                    DeskErrorCode::ACTION_NEED_RETRY,
                    "accepted audio baseline and recovery snapshot disagree",
                );
                return Ok(());
            }
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        coordinator.audio.lifecycle = MediaSlotLifecycle::Transitioning;
        coordinator.audio.pending_generation = Some(target);
        coordinator.audio_desired_active = candidate.audio.is_some();
        coordinator.audio_expected_terminal = Some(if candidate.audio.is_some() {
            AudioPipelinePhase::Active
        } else {
            AudioPipelinePhase::Off
        });
        coordinator.audio.generation = target;
        coordinator.audio_terminal_waiter = Some((target, tx));
        audio_waiter = Some((rx, candidate.audio.is_some()));
        audio_command = Some(ApplyMediaSettingsPayload {
            source_request_id: Some(model.request_id.clone()),
            connection_id: connection_id.to_string(),
            connection_epoch: request.connection_epoch.clone(),
            media_kind: MediaKind::Audio,
            action,
        });
        // Revocation/restart linearization point: no older audio write can
        // remain in flight after this write lock is released.
        pc.media_output_fence.write().await.audio_open = false;
    }
    drop(coordinator);
    drop(pc);

    let video_sent = if let Some(command) = video_command {
        ctx.worker_mgr
            .send_to_interactive_connection_worker(
                connection_id,
                ServiceToWorker::ApplyMediaSettings(command),
            )
            .await
            .is_ok()
    } else {
        true
    };
    // Start observing and committing the video group before any potentially
    // long-running audio prompt. The WebSocket is FIFO, but local permission UI
    // is an independent async producer and must not delay a completed video
    // transition or its recovery snapshot.
    let video_completion = video_waiter.map(|waiter| {
        let ctx = ctx.clone();
        let connection_id = connection_id.to_string();
        let connection_epoch = request.connection_epoch.clone();
        let request_id = model.request_id.clone();
        let requested = requested.clone();
        let candidate = candidate.clone();
        tokio::spawn(async move {
            let terminal_ok = video_sent
                && tokio::time::timeout_at(
                    std::cmp::min(
                        apply_deadline,
                        tokio::time::Instant::now() + Duration::from_secs(10),
                    ),
                    waiter,
                )
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(Result::ok)
                .is_some_and(|phase| phase == MediaPipelinePhase::Streaming);
            terminal_ok
                && commit_successful_settings_group(
                    &ctx,
                    &connection_id,
                    &connection_epoch,
                    &request_id,
                    MediaKind::Video,
                    &requested,
                    &candidate,
                )
                .await
        })
    });
    // All ordered intake above is complete: the coordinator now owns this
    // request and any immediate video command is already on the FIFO event
    // lane. Permission UI and terminal waits must not park the WebSocket read
    // loop, so finish them in a connection-scoped task. Close/swap invalidate
    // the task through registry membership, epoch and coordinator tokens.
    let task_ctx = ctx.clone();
    let task_model = model.clone();
    let task_connection_id = connection_id.to_string();
    tokio::spawn(async move {
        let ctx = &task_ctx;
        let model = &task_model;
        let connection_id = task_connection_id.as_str();
        let result: Result<(), RouterError> = async {
            let mut audio_permission_denied = false;
            let mut approval_token = None;
            if audio_command.is_some()
                && candidate.audio.is_some()
                && accepted_baseline.audio.is_none()
            {
                let token = uuid::Uuid::new_v4().to_string();
                let Some(pc) = ctx.pc_registry.get(connection_id).await else {
                    return Ok(());
                };
                let pc_guard = pc.read().await;
                if pc_guard.connection_epoch != request.connection_epoch {
                    return Ok(());
                }
                let access_ceiling = pc_guard.signaling_state.read().await.access_ceiling.clone();
                let mut coordinator = pc_guard.media_coordinator.lock().await;
                if coordinator.closed
                    || coordinator.current_apply_request_id.as_deref() != Some(&model.request_id)
                {
                    return Ok(());
                }
                coordinator.pending_audio_approval_id = Some(token.clone());
                drop(coordinator);
                drop(pc_guard);
                approval_token = Some(token);
                let decided = ctx
                    .policy
                    .capability(SecurityPermissionType::SystemAudioCapture);
                let permission =
                    effective_permission(access_ceiling.as_ref(), decided.permission, |ceiling| {
                        ceiling.allow_system_audio_capture
                    });
                let approved = tokio::time::timeout_at(
                    apply_deadline,
                    check_security_permission(
                        &ctx.policy,
                        &ctx.host_control_hub,
                        permission,
                        decided.generation,
                        SecurityPermissionType::SystemAudioCapture,
                        Some(connection_id.to_string()),
                        access_ceiling.is_some(),
                    ),
                )
                .await
                .unwrap_or(false);
                let current = ctx
                    .policy
                    .capability(SecurityPermissionType::SystemAudioCapture);
                let current_permission =
                    effective_permission(access_ceiling.as_ref(), current.permission, |ceiling| {
                        ceiling.allow_system_audio_capture
                    });
                audio_permission_denied = !approved
                    || (current.generation != decided.generation
                        && current_permission != Some(true));
            }

            // Close, worker swap, or a newer policy may have invalidated the task while
            // the local approval UI was open. Revalidate before placing any audio
            // command on the never-drop FIFO lane.
            let Some(pc) = ctx.pc_registry.get(connection_id).await else {
                return Ok(());
            };
            let pc_guard = pc.read().await;
            if pc_guard.connection_epoch != request.connection_epoch {
                return Ok(());
            }
            let mut coordinator = pc_guard.media_coordinator.lock().await;
            let token_matches = approval_token
                .as_ref()
                .is_none_or(|token| coordinator.pending_audio_approval_id.as_ref() == Some(token));
            if coordinator.closed
                || coordinator.current_apply_request_id.as_deref() != Some(&model.request_id)
                || !token_matches
            {
                return Ok(());
            }
            if approval_token.is_some() {
                coordinator.pending_audio_approval_id = None;
            }
            if audio_permission_denied {
                // No audio command has crossed the worker boundary. Roll the
                // reserved slot back locally; treating a permission denial as
                // a pipeline failure would unnecessarily restart the already
                // healthy video encoder through accepted-snapshot recovery.
                coordinator.audio.lifecycle = MediaSlotLifecycle::Stable;
                coordinator.audio.pending_generation = None;
                if let Some(previous_generation) = audio_previous_generation {
                    coordinator.audio.generation = previous_generation;
                }
                coordinator.audio_terminal_waiter = None;
                coordinator.audio_expected_terminal = None;
                coordinator.audio_desired_active = accepted_baseline.audio.is_some();
            }
            drop(coordinator);
            drop(pc_guard);

            let audio_sent = if audio_permission_denied {
                false
            } else if let Some(command) = audio_command {
                ctx.worker_mgr
                    .send_to_interactive_connection_worker(
                        connection_id,
                        ServiceToWorker::ApplyMediaSettings(command),
                    )
                    .await
                    .is_ok()
            } else {
                true
            };

            let audio_terminal = async {
                let terminal_ok = match audio_waiter {
                    Some((waiter, expected_active)) if audio_sent && !audio_permission_denied => {
                        tokio::time::timeout_at(
                            std::cmp::min(
                                apply_deadline,
                                tokio::time::Instant::now() + Duration::from_secs(10),
                            ),
                            waiter,
                        )
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .and_then(Result::ok)
                        .is_some_and(|phase| {
                            phase
                                == if expected_active {
                                    AudioPipelinePhase::Active
                                } else {
                                    AudioPipelinePhase::Off
                                }
                        })
                    }
                    Some(_) => false,
                    None => true,
                };
                if audio_changed && terminal_ok {
                    commit_successful_settings_group(
                        ctx,
                        connection_id,
                        &request.connection_epoch,
                        &model.request_id,
                        MediaKind::Audio,
                        &requested,
                        &candidate,
                    )
                    .await
                } else {
                    terminal_ok
                }
            };
            let audio_ok = audio_terminal.await;
            let video_ok = match video_completion {
                Some(task) => task.await.unwrap_or(false),
                None => true,
            };

            let Some(pc_ctx) = ctx.pc_registry.get(connection_id).await else {
                emit_error_response(
                    ctx,
                    model,
                    DeskErrorCode::ACTION_NEED_RETRY,
                    "connection closed while applying media settings",
                );
                return Ok(());
            };
            let mut pc = pc_ctx.write().await;
            if pc.connection_epoch != request.connection_epoch {
                return Ok(());
            }
            let media_coordinator = Arc::clone(&pc.media_coordinator);
            let mut coordinator = media_coordinator.lock().await;
            // Adaptive bitrate is a daemon-local connection controller rather
            // than a worker pipeline generation. Commit its accepted value
            // independently even when neither media group needed a terminal.
            if let Some(accepted) = coordinator.accepted_baseline.as_mut() {
                accepted.adaptive_bitrate = requested.adaptive_bitrate;
            }
            let baseline = coordinator
                .accepted_baseline
                .clone()
                .unwrap_or_else(|| requested.clone());
            let adaptive_quality_override = coordinator.adaptive_quality_override;
            coordinator.current_apply_request_id = None;
            coordinator.video_terminal_waiter = None;
            coordinator.audio_terminal_waiter = None;
            coordinator.audio_expected_terminal = None;
            if !audio_ok {
                coordinator.audio_desired_active = false;
            }
            drop(coordinator);
            let host_settings = pc.host_settings.clone();
            pc.host_settings = baseline.merge_into_host_settings(&host_settings);
            drop(pc);

            // Any video group command carries the user's baseline quality. Preserve a
            // still-valid adaptive override by replaying it after the group reaches its
            // terminal. A user-authored quality change clears the override inside the
            // commit above, so it is never replayed over an explicit choice.
            let adaptive_quality_override = if video_ok && video_changed {
                if let Some(quality) = adaptive_quality_override {
                    if ctx
                        .pc_registry
                        .update_media_settings_for_connection(
                            &ctx.worker_mgr,
                            connection_id,
                            None,
                            None,
                            Some(quality),
                            None,
                            None,
                        )
                        .await
                    {
                        Some(quality)
                    } else {
                        if let Some(pc) = ctx.pc_registry.get(connection_id).await {
                            pc.read()
                                .await
                                .media_coordinator
                                .lock()
                                .await
                                .adaptive_quality_override = None;
                        }
                        None
                    }
                } else {
                    None
                }
            } else {
                adaptive_quality_override
            };

            let mut audio_snapshot_recovered = false;
            if !video_ok {
                let recovered = match ctx
                    .pc_registry
                    .restart_media_from_cached_payload(
                        connection_id,
                        &ctx.worker_mgr,
                        MediaRestartTrigger::RenegotiatedSettings,
                    )
                    .await
                {
                    RestartOutcome::Restarted => {
                        wait_for_recovery_terminals(ctx, connection_id, apply_deadline).await
                    }
                    RestartOutcome::NoCachedPayload { .. } | RestartOutcome::Failed { .. } => false,
                };
                audio_snapshot_recovered = !audio_ok && recovered;
                if !recovered {
                    log::warn!(
                        "[router] accepted-snapshot recovery did not reach terminal state for \
                 {connection_id}; recycling worker"
                    );
                    if let Err(error) = ctx.worker_mgr.recycle_for_remote_access_timeout().await {
                        log::error!(
                            "[router] failed to recycle worker after media recovery failure for \
                     {connection_id}: {error}"
                        );
                    }
                }
            } else if !audio_ok && !audio_permission_denied {
                audio_snapshot_recovered = recover_audio_from_accepted_snapshot(
                    ctx,
                    connection_id,
                    &request.connection_epoch,
                    apply_deadline,
                )
                .await;
                if !audio_snapshot_recovered {
                    log::warn!(
                        "[router] accepted audio snapshot recovery failed for \
                         {connection_id}; video pipeline was left untouched"
                    );
                }
            }

            if audio_changed {
                send_audio_state_snapshot(
                    &ctx.outbound_tx,
                    connection_id,
                    &SystemAudioCaptureStateData {
                        connection_epoch: request.connection_epoch.clone(),
                        state: if audio_ok {
                            if baseline.audio.is_some() {
                                SystemAudioCaptureState::Active
                            } else {
                                SystemAudioCaptureState::Off
                            }
                        } else if audio_permission_denied {
                            SystemAudioCaptureState::Denied
                        } else if audio_snapshot_recovered {
                            if baseline.audio.is_some() {
                                SystemAudioCaptureState::Active
                            } else {
                                SystemAudioCaptureState::Off
                            }
                        } else {
                            SystemAudioCaptureState::Failed
                        },
                        accepted_audio: baseline.audio.clone(),
                        resolved_audio_device_id: None,
                        error_code: (!audio_ok).then_some(if audio_permission_denied {
                            DeskErrorCode::PERMISSION_ERROR
                        } else {
                            DeskErrorCode::ACTION_NEED_RETRY
                        }),
                    },
                );
            }

            // Connection-scoped adaptive-bitrate toggle: lock → flip → ship
            // the Clear (if any) → commit, all under the state lock so a
            // stale SetCap from the RTCP task can never land after the Clear
            // (see `daemon::bitrate_controller` for the ordering contract).
            if let Some(pc_ctx) = ctx.pc_registry.get(connection_id).await {
                let pc_guard = pc_ctx.read().await;
                let adaptive = Arc::clone(&pc_guard.adaptive_bitrate);
                let epoch = pc_guard.connection_epoch.clone();
                let generation = pc_guard
                    .cached_start_media
                    .read()
                    .await
                    .as_ref()
                    .map_or(0, |payload| payload.video_generation);
                drop(pc_guard);
                let mut state = adaptive.state.lock().await;
                if let Some(directive) = state.set_enabled_and_decide_clear(merged.adaptive_bitrate)
                {
                    crate::daemon::pc_manager::send_cap_directive(
                        &ctx.worker_mgr,
                        connection_id,
                        &epoch,
                        generation,
                        directive,
                        &mut state,
                    )
                    .await;
                }
            }

            send_settings_applied(
                ctx,
                model,
                &RemoteSessionSettingsApplied {
                    connection_epoch: request.connection_epoch,
                    effects: RemoteSessionSettingsEffects {
                        video: if !video_ok {
                            VideoSettingsEffect::Unchanged
                        } else if video_restart {
                            VideoSettingsEffect::Restarted
                        } else if video_changed {
                            VideoSettingsEffect::AppliedLive
                        } else {
                            VideoSettingsEffect::Unchanged
                        },
                        audio: if !audio_ok {
                            AudioSettingsEffect::Unchanged
                        } else {
                            match (
                                accepted_baseline.audio.is_some(),
                                requested.audio.is_some(),
                                audio_changed,
                            ) {
                                (false, true, _) => AudioSettingsEffect::Started,
                                (true, false, _) => AudioSettingsEffect::Stopped,
                                (true, true, true) => AudioSettingsEffect::Restarted,
                                _ => AudioSettingsEffect::Unchanged,
                            }
                        },
                        connection: ConnectionSettingsEffect::Unchanged,
                    },
                    baseline_settings: baseline,
                    runtime_overrides: RemoteSessionSettingsRuntimeOverrides {
                        adaptive_video_quality: adaptive_quality_override,
                    },
                    errors: [
                        (!video_ok).then(|| RemoteSessionSettingsFieldError {
                            field: "video".to_string(),
                            code: DeskErrorCode::ACTION_NEED_RETRY,
                        }),
                        (!audio_ok).then(|| RemoteSessionSettingsFieldError {
                            field: "audio".to_string(),
                            code: if audio_permission_denied {
                                DeskErrorCode::PERMISSION_ERROR
                            } else {
                                DeskErrorCode::ACTION_NEED_RETRY
                            },
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                },
            )
        }
        .await;
        if let Err(error) = result {
            log::warn!(
                "[router] asynchronous media-settings apply failed (request_id={}): {error}",
                model.request_id
            );
        }
    });
    Ok(())
}

pub(super) async fn handle_update_adaptive_video_quality_inbound(
    ctx: &RouterContext,
    model: &SignalingModel,
) -> Result<(), RouterError> {
    let request = match model.get_data::<UpdateAdaptiveVideoQuality>() {
        Ok(request) => request,
        Err(error) => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                &format!("bad UpdateAdaptiveVideoQuality payload: {error}"),
            );
            return Ok(());
        }
    };
    let connection_id = match model.from_connection_id.as_deref() {
        Some(connection_id) => connection_id,
        None => {
            emit_error_response(
                ctx,
                model,
                DeskErrorCode::INVALID_PARAMS,
                "UpdateAdaptiveVideoQuality requires an admitted connection",
            );
            return Ok(());
        }
    };
    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        return Ok(());
    };
    let pc = pc.read().await;
    if request.connection_epoch != pc.connection_epoch {
        return Ok(());
    }
    let media_coordinator = Arc::clone(&pc.media_coordinator);
    let mut coordinator = media_coordinator.lock().await;
    if coordinator.video.lifecycle != MediaSlotLifecycle::Stable
        || coordinator.current_apply_request_id.is_some()
    {
        drop(coordinator);
        drop(pc);
        emit_error_response(
            ctx,
            model,
            DeskErrorCode::ACTION_NEED_RETRY,
            "video settings transition is still in progress",
        );
        return Ok(());
    }
    let generation = coordinator.video.generation;
    // Reserve the video slot so a concurrent 301 observes a short-lived busy
    // state, then release both the PC and coordinator locks before awaiting the
    // worker lane. The generation guard makes a stale 303 harmless even if a
    // teardown races the send.
    coordinator.video.lifecycle = MediaSlotLifecycle::Transitioning;
    coordinator.video.pending_generation = Some(generation);
    drop(coordinator);
    drop(pc);

    let sent = ctx
        .worker_mgr
        .send_to_interactive_connection_worker(
            connection_id,
            ServiceToWorker::UpdateMediaSettings(UpdateMediaSettingsPayload {
                connection_id: connection_id.to_string(),
                connection_epoch: request.connection_epoch.clone(),
                video_generation: generation,
                fps: None,
                bitrate_kbps: None,
                quality: Some(request.video_quality),
                enable_dirty_rect: None,
                show_mouse: None,
            }),
        )
        .await
        .is_ok();

    let Some(pc) = ctx.pc_registry.get(connection_id).await else {
        return Ok(());
    };
    let pc = pc.read().await;
    if pc.connection_epoch != request.connection_epoch {
        return Ok(());
    }
    let mut coordinator = media_coordinator.lock().await;
    if coordinator.video.generation == generation
        && coordinator.video.pending_generation == Some(generation)
    {
        coordinator.video.lifecycle = MediaSlotLifecycle::Stable;
        coordinator.video.pending_generation = None;
        if sent {
            coordinator.adaptive_quality_override = Some(request.video_quality);
        }
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
