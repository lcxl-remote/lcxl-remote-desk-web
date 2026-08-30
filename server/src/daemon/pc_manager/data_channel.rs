//! Browser DataChannel classification, permission gating, and worker forwarding.

use std::sync::Arc;

use desk_ipc_protocol::message::{
    ClipboardPayload, FileTransferPayload, InputPayload, OpaqueConnectionPayload, ServiceToWorker,
};
use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::RwLock;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::RTCPeerConnection;

use crate::daemon::worker_manager::WorkerManager;
// =====================================================================
// DataChannel routing daemon → worker
// =====================================================================

/// DataChannel labels the browser opens against the daemon-held PC.
/// Mirrors the constants in `crate::model::data_channel` (kept locally
/// so this module does not depend on that one in tests / docs).
const DC_LABEL_MOUSE: &str = "mouse_event";
const DC_LABEL_MOUSE_MOVE: &str = "mouse_move_event";
const DC_LABEL_KEYBOARD: &str = "keyboard_event";
const DC_LABEL_CLIPBOARD: &str = "clipboard_event";
const DC_LABEL_FILE_TRANSFER: &str = "file_transfer_event";
const DC_LABEL_WHITEBOARD: &str = "whiteboard_event";
const DC_LABEL_CURSOR_SYNC: &str = "cursor_sync_event";

/// What to do with a DataChannel message based on its label. Pure
/// classification — no I/O — so it stays cheap to test exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DcRoute {
    /// Mouse non-move events (click / wheel). Gated by `accept_control`.
    Mouse,
    /// High-frequency mouse-move events. Gated by `accept_control`,
    /// kept distinct so the worker can apply move-specific coalescing.
    MouseMove,
    /// Keyboard events. Gated by `accept_control`.
    Keyboard,
    /// Clipboard writes (browser → host). Gated by `accept_clipboard_sync`.
    Clipboard,
    /// File-transfer commands. Gated by `accept_control` (file ops are
    /// part of the control surface).
    FileTransfer,
    /// Whiteboard commands. Gated by `accept_control`.
    Whiteboard,
    /// Cursor-sync DataChannel — the browser doesn't push to it; we
    /// stash the channel handle so the worker→daemon CursorData
    /// path has somewhere to write to.
    CursorSync,
}

/// Map a DataChannel `label` to its route. Returns `None` for
/// unknown labels so the caller can warn-and-drop without panicking.
pub(super) fn classify_dc_label(label: &str) -> Option<DcRoute> {
    match label {
        DC_LABEL_MOUSE => Some(DcRoute::Mouse),
        DC_LABEL_MOUSE_MOVE => Some(DcRoute::MouseMove),
        DC_LABEL_KEYBOARD => Some(DcRoute::Keyboard),
        DC_LABEL_CLIPBOARD => Some(DcRoute::Clipboard),
        DC_LABEL_FILE_TRANSFER => Some(DcRoute::FileTransfer),
        DC_LABEL_WHITEBOARD => Some(DcRoute::Whiteboard),
        DC_LABEL_CURSOR_SYNC => Some(DcRoute::CursorSync),
        _ => None,
    }
}

/// Build the `ServiceToWorker` IPC variant a given DcRoute should
/// forward as. Used by the daemon's `on_data_channel.on_message`
/// handler. Only browser→host directions are handled here; the
/// `Clipboard` arm uses `ClipboardWrite` (browser writing to host
/// clipboard); a future browser→host clipboard *request* DC would map
/// to `ClipboardRequest` but the current protocol multiplexes both
/// over the same `clipboard_event` channel and the worker disambiguates
/// by payload, so this always emits `ClipboardWrite`.
pub(super) fn route_to_service_msg(
    route: DcRoute,
    connection_id: &str,
    data: Vec<u8>,
) -> ServiceToWorker {
    match route {
        DcRoute::Mouse => ServiceToWorker::MouseInput(InputPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        DcRoute::MouseMove => ServiceToWorker::MouseMoveInput(InputPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        DcRoute::Keyboard => ServiceToWorker::KeyboardInput(InputPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        DcRoute::Clipboard => ServiceToWorker::ClipboardWrite(ClipboardPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        // FileTransfer is handled separately in
        // `install_browser_dc_message_forwarder` and never reaches
        // `route_to_service_msg`: it rides its own dedicated file lane
        // (see `desk-ipc-protocol::dual_transport`), not the event lane.
        DcRoute::FileTransfer => unreachable!(
            "FileTransfer is routed through WorkerManager::send_file_to_worker, \
             not the event lane"
        ),
        DcRoute::Whiteboard => ServiceToWorker::WhiteboardCommand(OpaqueConnectionPayload {
            connection_id: connection_id.to_string(),
            data,
        }),
        // CursorSync is read-side only; it never produces an IPC
        // message — the caller should not invoke this for it.
        DcRoute::CursorSync => unreachable!("CursorSync DC has no upstream message variant"),
    }
}

/// Permission gate. Returns `true` if the message should be forwarded
/// to the worker given the current `SignalingState`. Mirrors the
/// per-handler gating that used to live in the worker's `handle_*_event`
/// functions; consolidating it here means the worker can
/// trust every IPC variant it receives — gating is a daemon-side
/// concern only for routes whose access category lines up with a
/// SignalingState flag. `CursorSync` is filtered out before this is
/// called.
///
/// File transfer is its own access category (`allow_file_transfer`),
/// independent of `accept_control` which governs mouse/keyboard. The
/// browser file-management UI opens a *fresh* WebRTC connection that
/// has never requested control, so any daemon-side gate keyed on
/// `accept_control` would silently drop every download/upload. We let
/// file_transfer_event traffic through here and the worker's
/// `FileTransferDispatcher` runs the actual `check_security_permission`
/// per connection (the same per-DC permission cache the worker maintains).
pub(super) async fn route_is_permitted(
    route: DcRoute,
    state: &Arc<RwLock<SignalingState>>,
) -> bool {
    let s = state.read().await;
    // Capability restriction for redeemed-grant and legacy-support sessions is no
    // longer a coarse daemon-side hard-deny door here; it is enforced per
    // capability by the worker- and daemon-side `meet(ceiling, global)` gates
    // (clipboard via the control-grant meet that sets `accept_clipboard_sync`,
    // file transfer / whiteboard via their worker dispatcher gates). This gate now
    // only routes on the runtime grant bits (`accept_control` /
    // `accept_clipboard_sync`), which the ceiling already tightened.
    match route {
        DcRoute::Mouse | DcRoute::MouseMove | DcRoute::Keyboard => s.accept_control,
        DcRoute::Clipboard => s.accept_clipboard_sync,
        DcRoute::FileTransfer => true,
        // Whiteboard rides on the control grant, matching the worker's
        // historical per-handler gating.
        DcRoute::Whiteboard => s.accept_control,
        DcRoute::CursorSync => unreachable!("CursorSync DC has no message route"),
    }
}

/// Install the daemon's `on_data_channel` callback. Each browser-opened
/// DataChannel either (a) gets its `on_message` wired into the
/// IPC-forwarding closure that ships to the worker via
/// `ServiceToWorker::*`, or (b) for `cursor_sync_event`, has its
/// `Arc<RTCDataChannel>` stashed in the per-connection
/// `cursor_data_channel` slot for cursor-write-back. A third path:
/// `clipboard_event` channels are *both* stashed
/// (so the worker can push back via `WorkerToService::ClipboardRead`)
/// *and* wired with the on_message forwarder (so browser→host writes
/// flow through `ServiceToWorker::ClipboardWrite`).
///
/// Permission gates (`accept_control` / `accept_clipboard_sync`) are
/// checked *here*, before IPC, so the worker side can blindly trust
/// any IPC message it gets — keeping the trust boundary on the daemon
/// side where it belongs.
pub fn register_data_channel_router(
    pc: Arc<RTCPeerConnection>,
    connection_id: String,
    signaling_state: Arc<RwLock<SignalingState>>,
    cursor_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    latest_cursor_payload: Arc<RwLock<Option<String>>>,
    clipboard_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    file_transfer_data_channel: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    worker_mgr: WorkerManager,
) {
    pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
        let label = dc.label().to_owned();
        let dc_id = dc.id();
        let connection_id = connection_id.clone();
        let signaling_state = Arc::clone(&signaling_state);
        let cursor_data_channel = Arc::clone(&cursor_data_channel);
        let latest_cursor_payload = Arc::clone(&latest_cursor_payload);
        let clipboard_data_channel = Arc::clone(&clipboard_data_channel);
        let file_transfer_data_channel = Arc::clone(&file_transfer_data_channel);
        let worker_mgr = worker_mgr.clone();
        Box::pin(async move {
            log::info!("[DcRouter] {connection_id}: new DataChannel label='{label}' id={dc_id}");
            let route = match classify_dc_label(&label) {
                Some(r) => r,
                None => {
                    log::warn!(
                        "[DcRouter] {connection_id}: unknown DC label '{label}' — dropping channel"
                    );
                    return;
                }
            };
            if route == DcRoute::CursorSync {
                // Read-only cursor-shape write-back: the browser never pushes to
                // this channel (we install no on_message forwarder — hence the
                // early return before `route_is_permitted`). It carries no input
                // injection or exfiltration, so it is intentionally allowed even
                // for restricted temporary-support sessions; the restriction gate
                // in `route_is_permitted` deliberately never sees it.
                let mut slot = cursor_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                drop(slot);
                let replay_dc = Arc::clone(&dc);
                let replay_payload = Arc::clone(&latest_cursor_payload);
                let replay_connection_id = connection_id.clone();
                dc.on_open(Box::new(move || {
                    Box::pin(async move {
                        let cached = replay_payload.read().await.clone();
                        if let Some(payload) = cached
                            && let Err(error) = replay_dc.send_text(payload).await
                        {
                            log::warn!(
                                "[DcRouter] {replay_connection_id}: failed to replay cached \
                                 cursor on channel open: {error}"
                            );
                        }
                    })
                }));
                log::info!(
                    "[DcRouter] {connection_id}: stashed cursor_sync_event channel \
                     for worker→daemon cursor write-back"
                );
                return;
            }
            if route == DcRoute::Clipboard {
                let mut slot = clipboard_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed clipboard_event channel \
                     for worker→daemon clipboard write-back"
                );
                // Fall through to install the on_message forwarder so
                // browser→host writes still flow as ClipboardWrite IPC.
            }
            if route == DcRoute::FileTransfer {
                let mut slot = file_transfer_data_channel.write().await;
                *slot = Some(Arc::clone(&dc));
                log::info!(
                    "[DcRouter] {connection_id}: stashed file_transfer_event channel \
                     for worker→daemon file write-back"
                );
                // Fall through to install the on_message forwarder so
                // browser→host commands and chunks flow over the
                // dedicated file lane (not the event lane) via
                // `WorkerManager::send_file_to_worker` — see the
                // FileTransfer special case inside the forwarder.
            }
            install_browser_dc_message_forwarder(
                dc,
                connection_id,
                route,
                signaling_state,
                worker_mgr,
            );
        })
    }));
}

/// Install the per-DC `on_message` callback that gates on
/// `signaling_state` and forwards bytes to the worker via the worker
/// manager's IPC sender. Pulled out of the closure body so the routing
/// logic is unit-testable in isolation (the closure itself can't be
/// unit-tested without spinning up a full PC).
fn install_browser_dc_message_forwarder(
    dc: Arc<RTCDataChannel>,
    connection_id: String,
    route: DcRoute,
    signaling_state: Arc<RwLock<SignalingState>>,
    worker_mgr: WorkerManager,
) {
    dc.on_message(Box::new(
        move |msg: webrtc::data_channel::data_channel_message::DataChannelMessage| {
            let connection_id = connection_id.clone();
            let signaling_state = Arc::clone(&signaling_state);
            let worker_mgr = worker_mgr.clone();
            let bytes = msg.data.to_vec();
            let is_text = msg.is_string;
            Box::pin(async move {
                if !route_is_permitted(route, &signaling_state).await {
                    log::debug!(
                        "[DcRouter] {connection_id}: dropped {route:?} message (permission denied)"
                    );
                    return;
                }
                // FileTransfer rides its own dedicated lane — see
                // `desk-ipc-protocol::dual_transport`. Routing it
                // through `send_to_worker` (event lane) would put the
                // GB-scale download bytes back into the same queue as
                // heartbeats / manager responses, which is exactly the
                // HOL-blocking regression fix-2026-05-05 forbids.
                if route == DcRoute::FileTransfer {
                    // Browser → daemon file-transfer chunks/control don't
                    // carry an IPC-visible transfer_id: the routing key is
                    // either a binary header (first 36 bytes) the worker
                    // parses, or a JSON envelope it deserializes. The
                    // daemon stays protocol-agnostic and forwards the
                    // payload verbatim with `transfer_id: None`. Only the
                    // reverse direction (worker → daemon) sets the field,
                    // and only so the writer task can scope a `dc.send`
                    // failure when reporting `FileTransferSendFailed`.
                    let payload = FileTransferPayload {
                        connection_id: connection_id.clone(),
                        data: bytes,
                        is_text,
                        transfer_id: None,
                    };
                    if let Err(e) = worker_mgr
                        .send_file_to_connection_worker(&connection_id, payload)
                        .await
                    {
                        // Possible causes: worker not yet up (file lane
                        // not yet ready) or peer crashed mid-stream.
                        // Either way the browser's SCTP timeout will
                        // surface the failure to the user; we simply
                        // log and drop the command here.
                        log::warn!(
                            "[DcRouter] {connection_id}: failed to forward FileTransfer \
                             to worker via file lane: {e}"
                        );
                    }
                    return;
                }
                let svc_msg = route_to_service_msg(route, &connection_id, bytes);
                let result = if matches!(
                    route,
                    DcRoute::Mouse | DcRoute::MouseMove | DcRoute::Keyboard
                ) {
                    worker_mgr
                        .send_to_interactive_connection_worker(&connection_id, svc_msg)
                        .await
                } else {
                    worker_mgr
                        .send_to_connection_worker(&connection_id, svc_msg)
                        .await
                };
                if let Err(e) = result {
                    log::warn!(
                        "[DcRouter] {connection_id}: failed to forward {route:?} to worker: {e}"
                    );
                }
            })
        },
    ));
}
