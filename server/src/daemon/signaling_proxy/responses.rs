//! Worker response and terminal notification adapters.

use super::*;

/// Dispatch a [`WorkerToService::VirtualDisplayAttachResult`] to the
/// supervisor if it exists; in non-service-daemon modes
/// (`virtual_display = None`) production routes never produce this
/// variant, so a stray reply is either a test fixture or a logic bug —
/// drop it with a warning rather than panic. Extracted from the proxy
/// match arm to keep the routing logic unit-testable without spinning
/// up the full proxy task / outbound channel infrastructure.
pub(super) async fn dispatch_attach_result(
    payload: desk_ipc_protocol::message::VirtualDisplayAttachResultPayload,
    virtual_display: Option<
        &std::sync::Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>,
    >,
) {
    match virtual_display {
        Some(supervisor) => {
            supervisor.on_worker_attach_result(payload).await;
        }
        None => {
            warn!(
                "[SignalingProxy] VirtualDisplayAttachResult arrived while supervisor \
                 disabled (non-service-daemon mode?); dropping instance_id={}",
                payload.instance_id,
            );
        }
    }
}

/// Helper: build the outbound `SignalingModel` for a
/// `WorkerToService::VirtualDisplayMode` response. Applied →
/// success response carrying the mode the driver actually applied
/// (which may have been snapped to a nearby supported configuration);
/// Failed → `SignalingModel::error(INVALID_STATE, reason)`.
///
/// On a successful `Applied` outcome we also update the supervisor's
/// `last_known_refresh_hz` cache — this is the daemon's authoritative
/// source for the refresh-hz fallback when the auto-resolution browser
/// hook sends `refresh_hz=0`. Stray responses in non-service-daemon
/// mode (`supervisor=None`) are tolerated and the cache simply does
/// not update.
///
/// Kept as a free function so the routing logic can be unit-tested
/// without spinning up a signaling-proxy task. The call site in the
/// proxy loop only deals with the serialisation + outbound broadcast.
pub(super) fn build_virtual_display_response(
    payload: desk_ipc_protocol::message::VirtualDisplayModeResponsePayload,
    supervisor: Option<&std::sync::Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
) -> Result<SignalingModel, desk_signal_facade::error::DeskSignalFacadeError> {
    let connection_id = Some(payload.connection_id);
    match payload.outcome {
        VirtualDisplayModeOutcome::Applied(data) => {
            if let Some(supervisor) = supervisor {
                // Cache the full mode so the router's idempotent
                // short-circuit can compare exact (width, height,
                // refresh_hz) on the next inbound 205. `record_applied_mode`
                // silently drops any update with a zero component, so a
                // malformed worker echo cannot poison the cache.
                supervisor.record_applied_mode(data.width, data.height, data.refresh_hz);
            }
            let response = ChangeDisplaySettingsPayload {
                width: data.width,
                height: data.height,
                refresh_hz: data.refresh_hz,
                auto: false,
            };
            SignalingModel::success_response(
                &payload.request_id,
                SignalingType::DisplaySettingsChanged,
                None,
                connection_id,
                Some(&response),
            )
        }
        VirtualDisplayModeOutcome::Failed(reason) => SignalingModel::error(
            &payload.request_id,
            SignalingType::DisplaySettingsChanged,
            None,
            connection_id,
            DeskErrorCode::INVALID_STATE,
            &reason,
        ),
    }
}

/// Rebuild the outbound `Manager*` response `SignalingModel` (with the
/// `request_id` echoed for correlation) and broadcast it to the
/// browser via `outbound_tx`. Build / serialise failures are
/// non-fatal — log + drop, no panic on the bus.
///
/// `from_connection_id` is left `None` (the daemon is the responder
/// here, not a peer browser); `to_connection_id` is `Option<String>`
/// because manager-plane / `ListTerminalCommands` requests can be HTTP-API-
/// triggered without an originating browser PC — in that case the
/// signal/manager server matches the response by `request_id` alone
/// (see `signal-facade::model::connection::request_callback_map`).
pub(super) fn send_manager_response<T>(
    outbound_tx: &broadcast::Sender<String>,
    type_name: &'static str,
    request_id: &str,
    connection_id: &Option<String>,
    signaling_type: SignalingType,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    match SignalingModel::success_response(
        request_id,
        signaling_type,
        None,
        connection_id.clone(),
        data,
    ) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => warn!(
                "[SignalingProxy] Failed to serialise {type_name} response for {connection_id:?}: \
                 {e} (request_id={request_id})"
            ),
        },
        Err(e) => warn!(
            "[SignalingProxy] Failed to build {type_name} response model for {connection_id:?}: \
             {e} (request_id={request_id})"
        ),
    }
}

/// Build a server-initiated `new_request` `SignalingModel` (no `request_id`
/// correlation — the daemon mints a fresh one inside `new_request`)
/// for terminal-plane notifications (`TerminalOutputProduced`,
/// `TerminalClosed`) and broadcast it to the browser via
/// `outbound_tx`. Build / serialise failures are non-fatal —
/// log + drop, no panic on the bus. Mirrors the shape
/// `service::terminal` used to construct directly when worker still
/// owned the WS path.
pub(super) fn send_terminal_notification<T>(
    outbound_tx: &broadcast::Sender<String>,
    type_name: &'static str,
    connection_id: &str,
    signaling_type: SignalingType,
    data: Option<&T>,
) where
    T: serde::Serialize + ?Sized,
{
    match SignalingModel::new_request(signaling_type, Some(connection_id.to_string()), data) {
        Ok(model) => match serde_json::to_string(&model) {
            Ok(text) => {
                let _ = outbound_tx.send(text);
            }
            Err(e) => warn!(
                "[SignalingProxy] Failed to serialise {type_name} notification for \
                 {connection_id}: {e}"
            ),
        },
        Err(e) => warn!(
            "[SignalingProxy] Failed to build {type_name} notification model for \
             {connection_id}: {e}"
        ),
    }
}
