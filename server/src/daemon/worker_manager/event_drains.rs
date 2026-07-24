//! Daemon-side media and file-lane receiver tasks.

use super::*;

/// Spawn the daemon-side media receiver. Owns a [`MediaReceiver`] (already
/// constructed by the caller — `framed::make_media_receiver` for named-pipe
/// mode, `inprocess::make_media` for the in-process portable path), decodes
/// each [`MediaFrame`] and forwards to
/// [`crate::daemon::pc_manager::write_video_frame`] for
/// `track.write_sample(...)`. Exits when `recv_frame` returns `None`
/// (transport closed).
pub(super) fn spawn_media_receiver_task(
    mut receiver: Box<dyn MediaReceiver>,
    pc_registry: PcRegistry,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("[MediaReceiver] starting");
        while let Some(frame) = receiver.recv_frame().await {
            debug!(
                "[MediaReceiver] frame seq={} kind={:?} len={} for {}",
                frame.seq,
                frame.kind,
                frame.payload.len(),
                frame.connection_id
            );
            crate::daemon::pc_manager::write_video_frame(&pc_registry, frame).await;
        }
        info!("[MediaReceiver] exiting (transport closed)");
    })
}

/// Spawn the daemon-side file-lane drain. Owns an
/// `EventReceiver<FileTransferPayload>` and routes every payload to
/// `pc_manager::write_file_transfer_data`, which dispatches by
/// `connection_id` to the corresponding browser DC. Single drain task
/// across all connections — cross-connection head-of-line is the
/// known trade-off documented in `dual_transport.rs`.
pub(super) fn spawn_file_drain_task(
    mut receiver: Box<dyn EventReceiver<FileTransferPayload>>,
    pc_registry: PcRegistry,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("[FileDrain] starting");
        while let Some(payload) = receiver.recv().await {
            crate::daemon::pc_manager::write_file_transfer_data(&pc_registry, payload).await;
        }
        info!("[FileDrain] exiting (transport closed)");
    })
}
