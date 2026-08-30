//! Shared daemon-to-worker event transport bridge.

use super::*;

/// Named-pipe / Unix-socket bridge: wrap the byte-stream halves in framed
/// event transports and delegate to [`bridge_event_transport`]. The
/// transport-agnostic main loop is shared with the in-process portable
/// path so behavioural differences (cmd → wire, wire → msg, daemon-
/// initiated vs unexpected exit) live in exactly one place.
pub(super) async fn bridge_loop<R, W>(
    reader: R,
    writer: W,
    cmd_rx: &mut mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: &WorkerMessageSink,
    name: &str,
) -> bool
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    bridge_loop_with_profile(
        reader,
        writer,
        cmd_rx,
        msg_tx,
        name,
        WorkerProfile::SessionUser,
    )
    .await
}

pub(super) async fn bridge_loop_with_profile<R, W>(
    reader: R,
    writer: W,
    cmd_rx: &mut mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: &WorkerMessageSink,
    name: &str,
    profile: WorkerProfile,
) -> bool
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let event_tx: Arc<dyn EventSender<ServiceToWorker>> = framed::spawn_event_sender(writer);
    let event_rx: Box<dyn EventReceiver<WorkerToService>> = framed::make_event_receiver(reader);
    bridge_event_transport_with_profile(event_rx, event_tx, cmd_rx, msg_tx, name, profile).await
}

/// Transport-agnostic bridge between the daemon's internal channels (`cmd_rx`
/// for daemon → worker; `msg_tx` for worker → daemon) and the supplied event
/// transport pair. Returns `true` when the daemon initiated the shutdown
/// (Shutdown command sent or cmd channel closed) and `false` when the worker
/// side dropped first — the caller uses this to decide whether to trigger
/// crash-recovery.
///
/// One bridge serves exactly one worker, which is why `msg_tx` is a
/// [`WorkerMessageSink`] rather than the daemon's shared channel: the sink knows
/// whose messages these are and stamps them, so the daemon can still tell them
/// apart after this worker has been replaced.
pub(super) async fn bridge_event_transport(
    event_rx: Box<dyn EventReceiver<WorkerToService>>,
    event_tx: Arc<dyn EventSender<ServiceToWorker>>,
    cmd_rx: &mut mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: &WorkerMessageSink,
    name: &str,
) -> bool {
    bridge_event_transport_with_profile(
        event_rx,
        event_tx,
        cmd_rx,
        msg_tx,
        name,
        WorkerProfile::SessionUser,
    )
    .await
}

pub(super) async fn bridge_event_transport_with_profile(
    mut event_rx: Box<dyn EventReceiver<WorkerToService>>,
    event_tx: Arc<dyn EventSender<ServiceToWorker>>,
    cmd_rx: &mut mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: &WorkerMessageSink,
    name: &str,
    profile: WorkerProfile,
) -> bool {
    let (worker_msg_tx, mut worker_msg_rx) = mpsc::unbounded_channel::<Option<WorkerToService>>();
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Some(m) => {
                    if worker_msg_tx.send(Some(m)).is_err() {
                        break;
                    }
                }
                None => {
                    let _ = worker_msg_tx.send(None);
                    break;
                }
            }
        }
    });

    let mut daemon_initiated = false;
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(msg) => {
                        if !msg.allowed_for_profile(profile) {
                            error!(
                                "Refusing daemon command {:?} for {:?} worker [{name}]",
                                std::mem::discriminant(&msg),
                                profile
                            );
                            continue;
                        }
                        if matches!(msg, ServiceToWorker::Shutdown) {
                            daemon_initiated = true;
                        }
                        if let Err(e) = event_tx.send(msg).await {
                            error!("Failed to send to Worker [{name}]: {e}");
                            break;
                        }
                    }
                    None => {
                        info!("Command channel closed for [{name}], shutting down");
                        daemon_initiated = true;
                        break;
                    }
                }
            }
            msg_result = worker_msg_rx.recv() => {
                match msg_result {
                    Some(Some(msg)) => {
                        if !msg.allowed_for_profile(profile) {
                            error!(
                                "Refusing worker event {:?} from {:?} worker [{name}]",
                                std::mem::discriminant(&msg),
                                profile
                            );
                            continue;
                        }
                        if !msg_tx.send(msg) {
                            error!("SignalingProxy receiver dropped for [{name}]");
                            break;
                        }
                    }
                    Some(None) => {
                        info!("Worker event transport closed for [{name}]");
                        break;
                    }
                    None => {
                        info!("Worker reader task stopped for [{name}]");
                        break;
                    }
                }
            }
        }
    }
    daemon_initiated
}
