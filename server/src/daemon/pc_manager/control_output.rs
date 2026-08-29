//! Cursor and clipboard delivery to browser data channels.

use super::*;

/// Write a worker-emitted cursor-sync payload to the matching
/// connection's `cursor_sync_event` DataChannel. The daemon performs the
/// `channel.send_text(json)` based on a `WorkerToService::CursorData` IPC
/// the worker pushes from its capture loop.
///
/// All "channel-not-open" / "connection-unknown" paths are silent:
///
/// - Unknown `connection_id` — race against `CloseRemoteSession`; trace-log.
/// - No cursor DataChannel registered yet — browser hasn't opened the
///   `cursor_sync_event` channel for this connection (e.g. control
///   not granted, browser still negotiating). Debug-log + drop.
/// - Channel registered but not in `Open` state — the WebRTC
///   handshake hasn't completed for that DC; debug-log + drop.
/// - Send failed — log warn and continue; the next cursor update will
///   resync the browser without operator intervention.
pub async fn write_cursor_data(registry: &PcRegistry, payload: CursorDataPayload) {
    let ctx = match registry.get(&payload.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping cursor data for unknown connection {}",
                payload.connection_id
            );
            return;
        }
    };
    let cursor_json = match std::str::from_utf8(&payload.data) {
        Ok(value) => value.to_string(),
        Err(e) => {
            log::warn!(
                "[pc_manager] cursor data for {} not UTF-8: {e}; dropping",
                payload.connection_id
            );
            return;
        }
    };
    let dc_opt = {
        let ctx = ctx.read().await;
        *ctx.latest_cursor_payload.write().await = Some(cursor_json.clone());
        ctx.cursor_data_channel.read().await.clone()
    };
    let dc = match dc_opt {
        Some(d) => d,
        None => {
            log::debug!(
                "[pc_manager] dropping cursor data for {} — no cursor_sync DataChannel \
                 registered yet (browser hasn't opened it)",
                payload.connection_id
            );
            return;
        }
    };
    if dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
        log::debug!(
            "[pc_manager] dropping cursor data for {} — DC state is {:?}, not Open",
            payload.connection_id,
            dc.ready_state()
        );
        return;
    }
    // Worker ships JSON bytes (see CursorSyncData serialisation in
    // model::data_channel); the daemon hands them through unchanged.
    // We use `send_text` rather than `send` so the browser receives a
    // text frame matching the legacy wire shape exactly.
    if let Err(e) = dc.send_text(cursor_json).await {
        log::warn!(
            "[pc_manager] failed to send cursor data for {}: {e}",
            payload.connection_id
        );
    }
}

/// Write a worker-emitted clipboard payload (text or chunked image —
/// already JSON-encoded as `ClipboardEventData`) to the matching
/// connection's `clipboard_event` DataChannel. The daemon writes the
/// JSON unchanged so the browser sees the same wire shape the worker's
/// `service::clipboard_event::handle_clipboard_event` polling task
/// used to emit.
///
/// Permission gating is applied here (not the worker): the worker
/// emits unconditionally for every active connection so it does not
/// have to track per-connection accept state, and the daemon drops the
/// IPC if `accept_control && accept_clipboard_sync` is not set on
/// the matching `SignalingState`. This keeps the trust boundary on the
/// daemon side, same as the browser→worker direction in
/// `register_data_channel_router`.
///
/// Silent-drop branches:
///
/// - Unknown `connection_id` — race against `CloseRemoteSession`; trace-log.
/// - Permission not granted — `accept_clipboard_sync` is false; debug-log.
/// - No clipboard DataChannel registered yet — browser hasn't opened
///   the `clipboard_event` channel; debug-log.
/// - Channel registered but not in `Open` state — debug-log.
/// - Non-UTF-8 bytes — warn + drop (worker should always serialise
///   `ClipboardEventData` as JSON; this defends against a
///   mismatched-version worker).
/// - Send failed — warn-log; the next clipboard change will resync
///   the browser without operator intervention.
pub async fn write_clipboard_data(registry: &PcRegistry, payload: ClipboardPayload) {
    let ctx = match registry.get(&payload.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping clipboard data for unknown connection {}",
                payload.connection_id
            );
            return;
        }
    };
    let (dc_opt, accepted) = {
        let ctx = ctx.read().await;
        let dc = ctx.clipboard_data_channel.read().await.clone();
        let s = ctx.signaling_state.read().await;
        (dc, s.accept_control && s.accept_clipboard_sync)
    };
    if !accepted {
        log::debug!(
            "[pc_manager] dropping clipboard data for {} — permission not granted",
            payload.connection_id
        );
        return;
    }
    let dc = match dc_opt {
        Some(d) => d,
        None => {
            log::debug!(
                "[pc_manager] dropping clipboard data for {} — no clipboard DataChannel \
                 registered yet (browser hasn't opened it)",
                payload.connection_id
            );
            return;
        }
    };
    if dc.ready_state() != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open {
        log::debug!(
            "[pc_manager] dropping clipboard data for {} — DC state is {:?}, not Open",
            payload.connection_id,
            dc.ready_state()
        );
        return;
    }
    let s = match std::str::from_utf8(&payload.data) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "[pc_manager] clipboard data for {} not UTF-8: {e}; dropping",
                payload.connection_id
            );
            return;
        }
    };
    if let Err(e) = dc.send_text(s.to_string()).await {
        log::warn!(
            "[pc_manager] failed to send clipboard data for {}: {e}",
            payload.connection_id
        );
    }
}
