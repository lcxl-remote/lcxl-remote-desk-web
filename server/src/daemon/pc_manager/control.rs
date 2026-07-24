//! Browser control and clipboard permission handling.

use super::*;

/// Daemon side of `SignalingType::RequireControl`. Mirrors the
/// worker-side `DeskSession::handle_request_control` but runs against
/// the daemon-held PC. The browser sends this to either
/// (a) request control + clipboard grants (`accept = true`) or (b)
/// release them (`accept = false`); the daemon dispatches to the
/// host-control hub for user approval (subject to settings allow /
/// remember bits), updates the per-connection [`SignalingState`], and
/// emits the matching reply back through the outbound sink:
///
/// - `accept = true` && approved → `AcceptControl`
/// - `accept = true` && denied → `DenyControl` (state stays false)
/// - `accept = false` (release) → `CloseControl` (state goes false)
///
/// The daemon `on_data_channel` router gates each forwarded
/// browser-input event on the resulting `accept_control` /
/// `accept_clipboard_sync` flags, so the worker only ever sees IPC
/// payloads the user has authorised.
pub async fn handle_require_control(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    settings: &SharedSettings,
    host_control_hub: &Arc<HostControlHub>,
    model: &SignalingModel,
) -> Result<ControlOutcome, DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let ctx = registry.get(from_connection_id).await.ok_or_else(|| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!(
                "No PC for {from_connection_id} (RequireControl arrived before RequestRemote?)"
            ),
        ))
    })?;

    let control_data = model.get_data::<SignalRequestControlData>()?;
    log::info!(
        "[pc_manager] {from_connection_id} RequireControl: {:?}",
        control_data
    );

    // Snapshot the pre-decision state for the short-circuit helpers
    // (re-grant of an already-accepted permission must not re-prompt
    // the user). Read lock dropped before the approval await so the
    // signaling-state write below can take exclusive access cleanly.
    let (currently_has_control, currently_has_clipboard) = {
        let ctx = ctx.read().await;
        let s = ctx.signaling_state.read().await;
        (s.accept_control, s.accept_clipboard_sync)
    };

    // Releasing control (accept = false) is never a privileged action and must
    // never prompt the host. The browser sends RequireControl{accept=false} when
    // the user clicks "cancel control"; routing that through the approval path
    // would pop a spurious authorization dialog on the host just as the
    // controller is walking away (and, with allow_remote_control = None, block on
    // the UI-readiness probe). Short-circuit straight to the release reply.
    if !control_data.accept {
        {
            let ctx = ctx.read().await;
            let mut s = ctx.signaling_state.write().await;
            s.accept_control = false;
            s.accept_clipboard_sync = false;
        }
        log::info!("[pc_manager] {from_connection_id}: release (CloseControl)");
        send_response::<()>(
            outbound,
            &model.request_id,
            SignalingType::CloseControl,
            from_connection_id,
            None,
        )?;
        return Ok(ControlOutcome {
            connection_id: from_connection_id.to_string(),
            accept_control: false,
            changed: currently_has_control,
        });
    }

    // From here on the browser is requesting a grant (accept = true). The
    // effective permission is the connection's capability ceiling met with the
    // host global, so a redeemed-grant session can only be tightened relative to
    // the owner's global; an owner session carries no ceiling and uses the global
    // verbatim.
    let access_ceiling = ctx
        .read()
        .await
        .signaling_state
        .read()
        .await
        .access_ceiling
        .clone();
    let allow_control = effective_permission(
        access_ceiling.as_ref(),
        settings.read().await.security.allow_remote_control,
        |c| c.allow_remote_control,
    );
    let allow_clipboard = effective_permission(
        access_ceiling.as_ref(),
        settings.read().await.security.allow_clipboard_sync,
        |c| c.allow_clipboard_sync,
    );

    let control_approved =
        if should_short_circuit_control(control_data.accept, currently_has_control) {
            log::info!(
                "[pc_manager] {from_connection_id}: short-circuit RemoteControl (already accepted)"
            );
            true
        } else {
            check_security_permission(
                settings,
                host_control_hub,
                allow_control,
                SecurityPermissionType::RemoteControl,
                Some(from_connection_id.to_string()),
                // Capped grant / code-session: honor the prompt but never widen the
                // owner's global allow_* from a borrowed session's "remember".
                access_ceiling.is_some(),
            )
            .await
        };

    if !control_approved {
        log::warn!("[pc_manager] {from_connection_id}: RemoteControl denied");
        {
            let ctx = ctx.read().await;
            let mut s = ctx.signaling_state.write().await;
            s.accept_control = false;
            s.accept_clipboard_sync = false;
        }
        send_response::<()>(
            outbound,
            &model.request_id,
            SignalingType::DenyControl,
            from_connection_id,
            None,
        )?;
        // Denial sets accept_control = false; this PC's value changed
        // iff it was previously holding control. Short-circuiting the
        // current value avoids spurious exclusive-mode updates when the
        // user denies a brand-new RequireControl.
        return Ok(ControlOutcome {
            connection_id: from_connection_id.to_string(),
            accept_control: false,
            changed: currently_has_control,
        });
    }

    let clipboard_approved = if !control_data.accept_clipboard_sync {
        false
    } else if should_short_circuit_clipboard(
        control_data.accept_clipboard_sync,
        currently_has_clipboard,
    ) {
        log::info!(
            "[pc_manager] {from_connection_id}: short-circuit ClipboardSync (already accepted)"
        );
        true
    } else {
        check_security_permission(
            settings,
            host_control_hub,
            allow_clipboard,
            SecurityPermissionType::ClipboardSync,
            Some(from_connection_id.to_string()),
            access_ceiling.is_some(),
        )
        .await
    };

    {
        let ctx = ctx.read().await;
        let mut s = ctx.signaling_state.write().await;
        s.accept_control = true;
        s.accept_clipboard_sync = clipboard_approved;
        log::info!(
            "[pc_manager] {from_connection_id}: AcceptControl \
             (accept_control=true, accept_clipboard_sync={clipboard_approved})"
        );
    }

    send_response::<()>(
        outbound,
        &model.request_id,
        SignalingType::AcceptControl,
        from_connection_id,
        None,
    )?;
    Ok(ControlOutcome {
        connection_id: from_connection_id.to_string(),
        accept_control: true,
        changed: !currently_has_control,
    })
}

/// Outcome the router needs to update the exclusive-mode layer. The
/// `changed` flag is true iff `accept_control` actually moved (a
/// re-grant of an already-accepted permission short-circuits in
/// `handle_require_control` but still returns `changed = false`),
/// letting the router skip the exclusive recompute entirely in that
/// common case. `connection_id` is the PC whose state moved; the
/// router does not currently key off it but the field is in place so
/// per-connection logging stays useful.
#[derive(Debug, Clone)]
pub struct ControlOutcome {
    pub connection_id: String,
    pub accept_control: bool,
    pub changed: bool,
}
