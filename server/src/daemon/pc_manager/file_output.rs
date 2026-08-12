//! File-transfer delivery to per-connection browser data channels.

use super::*;

/// Route a worker-emitted file-transfer payload onto the matching
/// connection's per-connection writer task (queued via
/// `PeerConnectionContext::file_transfer_writer_tx`). The actual
/// `dc.send` / `dc.send_text` runs inside the spawned task — see
/// [`spawn_file_transfer_writer_task`] for the write logic and the
/// silent-drop policy.
///
/// Decoupling the write from this dispatch hop is what keeps the
/// daemon's main IPC loop in
/// `signaling_proxy::run_signaling_proxy` from head-of-line blocking
/// behind a slow / stalled DataChannel: a 989 MB transfer that fills
/// SCTP send buffers no longer delays unrelated typed-IPC traffic
/// (`FilesListed`, `Heartbeat`, ...). The dispatch itself
/// is `O(1)` — registry lookup + non-blocking
/// `UnboundedSender::send`.
///
/// Permission gate: file transfer is on its own access category
/// (`security.allow_file_transfer`) — independent from
/// `accept_control` / `accept_clipboard_sync`. The browser
/// file-management UI opens a fresh PC that never requests remote
/// control, so the daemon must forward write-back unconditionally.
/// The actual permission check lives in the worker dispatcher
/// (`worker::file_transfer_dispatcher::permission_for`), which runs
/// `check_security_permission(allow_file_transfer, FileTransfer)`
/// before processing the inbound command. If the worker is satisfied,
/// any reply it emits is by definition authorised — re-checking here
/// against the unrelated `accept_control` flag would silently drop
/// every download (regression fixed 2026-05-05).
///
/// Silent-drop branches at this layer:
///
/// - Unknown `connection_id` — race against `CloseRemoteSession`; trace.
/// - Writer task gone (sender disconnected) — debug. Happens during
///   teardown when the context has dropped but a stale payload was
///   already in the daemon's `worker_rx` queue.
pub async fn write_file_transfer_data(registry: &PcRegistry, payload: FileTransferPayload) {
    let ctx_arc = match registry.get(&payload.connection_id).await {
        Some(c) => c,
        None => {
            log::trace!(
                "[pc_manager] dropping file transfer data for unknown connection {}",
                payload.connection_id
            );
            return;
        }
    };
    // Clone the writer + connection_id under the read guard, then DROP
    // the guard before awaiting `Sender::send`. Holding the read guard
    // across the bounded send would block every other reader of this
    // PeerConnectionContext (clipboard / signaling state / ...) for the
    // entire SCTP-backpressure pause — the daemon's main IPC drain
    // would also park, defeating the lane-separation guarantee.
    let (writer_tx, conn_id) = {
        let ctx = ctx_arc.read().await;
        (
            ctx.file_transfer_writer_tx.clone(),
            ctx.connection_id.clone(),
        )
    };
    if let Err(e) = writer_tx.send(payload).await {
        log::debug!("[pc_manager] file transfer writer task gone for {conn_id}: {e}");
    }
}

/// Spawn the per-connection file-transfer writer. Drains
/// `rx` serially and routes each payload to the matching DataChannel
/// stored in `dc_slot`.
///
/// Lifetime is tied to the sender end inside
/// `PeerConnectionContext::file_transfer_writer_tx`: when that
/// context drops (registry release in `cleanup_pc`), all senders are
/// gone and `rx.recv()` returns `None`, exiting the task.
///
/// When `worker_mgr` is `Some`, a failed `dc.send` is reported back to
/// the worker via [`ServiceToWorker::FileTransferSendFailed`] so the
/// worker dispatcher can abort the matching in-flight transfer and
/// emit a `TransferError` to the browser. The error is also classified
/// ([`FileTransferSendErrorKind`]) so the worker (and the daemon log)
/// can distinguish a configuration bug (`PacketTooLarge`) from normal
/// teardown (`TransportClosed`). When `worker_mgr` is `None`
/// (test-only callers), the failure is logged and dropped so tests
/// don't need to wire a real `WorkerManager`.
///
/// Silent-drop branches inside the task:
///
/// - No file-transfer DC registered — debug (browser hasn't opened it
///   yet, or PC was torn down before the DC frame arrived).
/// - DC not in `Open` state — debug.
/// - send_text on non-UTF-8 bytes — warn + drop. Defends against a
///   buggy worker that sets `is_text=true` on raw chunk bytes.
pub(super) fn spawn_file_transfer_writer_task(
    connection_id: String,
    mut rx: mpsc::Receiver<FileTransferPayload>,
    dc_slot: Arc<RwLock<Option<Arc<RTCDataChannel>>>>,
    worker_mgr: Option<WorkerManager>,
) {
    // `tokio::spawn` (not `actix_web::rt::spawn`) is intentional:
    // the task only awaits `mpsc::recv` and `webrtc-rs` futures, both
    // of which are `Send` and need no `LocalSet`. Using
    // `actix_web::rt::spawn` (which is `spawn_local`) would force
    // every `#[tokio::test]` that calls `create_for_request_remote`
    // to wrap itself in a `LocalSet` for the constructor to succeed.
    tokio::spawn(async move {
        let mut window = DaemonFtWindow::default();
        // `last_send_done` anchors the `recv_idle` measurement: time
        // between completing one `dc.send` and pulling the next
        // payload off the bounded queue. A persistently large idle
        // gap during a slow transfer is the smoking gun for an
        // upstream stall (worker / IPC / disk); a near-zero gap with
        // long `dc_send` points the finger at SCTP / webrtc-rs.
        // Initialised to `Instant::now()` so the very first sample's
        // idle gap measures from task start, not from a previous send
        // that never happened.
        let mut last_send_done = std::time::Instant::now();
        while let Some(payload) = rx.recv().await {
            let recv_idle = last_send_done.elapsed();
            let dc_opt = dc_slot.read().await.clone();
            let dc = match dc_opt {
                Some(d) => d,
                None => {
                    log::debug!(
                        "[pc_manager] dropping file transfer data for {connection_id} — no \
                         file_transfer DataChannel registered yet"
                    );
                    continue;
                }
            };
            if dc.ready_state()
                != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            {
                log::debug!(
                    "[pc_manager] dropping file transfer data for {connection_id} — DC state \
                     is {:?}, not Open",
                    dc.ready_state()
                );
                continue;
            }
            // Sample the SCTP transmit buffer occupancy BEFORE the
            // send so the window's `buffered_max` / `buffered_avg`
            // reflect what we hand off to webrtc-rs (post-send the
            // number can momentarily drop as bytes get flushed onto
            // the wire, which would mask sustained occupancy).
            let buffered_before = dc.buffered_amount().await as u64;
            let payload_len = payload.data.len() as u64;
            let is_text = payload.is_text;
            let payload_transfer_id = payload.transfer_id.clone();
            let send_start = std::time::Instant::now();
            let result = if is_text {
                let s = match std::str::from_utf8(&payload.data) {
                    Ok(s) => s.to_string(),
                    Err(e) => {
                        log::warn!(
                            "[pc_manager] file transfer text for {connection_id} not UTF-8: \
                             {e}; dropping"
                        );
                        continue;
                    }
                };
                dc.send_text(s).await
            } else {
                dc.send(&bytes::Bytes::from(payload.data)).await
            };
            let dc_send_elapsed = send_start.elapsed();
            last_send_done = std::time::Instant::now();
            if let Err(e) = result {
                let kind = classify_dc_send_error(&e);
                match kind {
                    FileTransferSendErrorKind::PacketTooLarge => {
                        // Configuration bug: the chosen chunk_size +
                        // binary-header exceeds the remote SDP's
                        // a=max-message-size. The whole transfer is
                        // doomed (every subsequent chunk trips the same
                        // check) so this is logged at error! and the
                        // worker is told to abort.
                        log::error!(
                            "[pc_manager] {connection_id}: SCTP packet too large \
                             (chunk_size + header > remote max_message_size): {e}"
                        );
                    }
                    FileTransferSendErrorKind::TransportClosed => {
                        // Normal teardown / peer disconnect; the
                        // cleanup_pc path is already on its way.
                        log::debug!("[pc_manager] {connection_id}: DC closed mid-transfer: {e}");
                    }
                    FileTransferSendErrorKind::Other => {
                        log::warn!(
                            "[pc_manager] {connection_id}: file transfer dc.send failed: {e}"
                        );
                    }
                }
                if let Some(mgr) = worker_mgr.as_ref() {
                    let notify =
                        ServiceToWorker::FileTransferSendFailed(FileTransferSendFailedPayload {
                            connection_id: connection_id.clone(),
                            transfer_id: payload_transfer_id,
                            chunk_index: None,
                            kind,
                            error: e.to_string(),
                        });
                    if let Err(send_err) = mgr.send_to_worker(notify).await {
                        log::debug!(
                            "[pc_manager] {connection_id}: could not deliver \
                             FileTransferSendFailed to worker: {send_err}"
                        );
                    }
                }
                // Still account for the failed send in the window so
                // the next flush surfaces the failure latency.
            }
            window.record(
                payload_len,
                is_text,
                recv_idle,
                dc_send_elapsed,
                buffered_before,
            );
            if window.is_full() {
                if let Some(line) = window.flush_line(&connection_id) {
                    log::info!("{line}");
                }
                window.reset();
            }
        }
        // Trailing flush so the last partial window does not vanish
        // when the sender drops on PC teardown.
        if let Some(line) = window.flush_line(&connection_id) {
            log::info!("{line}");
        }
        log::debug!(
            "[pc_manager] file transfer writer task exited for {connection_id} (sender dropped)"
        );
    });
}
