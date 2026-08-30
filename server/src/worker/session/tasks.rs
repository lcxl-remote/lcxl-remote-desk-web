use super::*;

/// Open the daemon-side media pipe (Windows: named pipe; Unix: domain
/// socket) and wrap the writer half in a [`MediaSender`] that flushes
/// onto it via the framed transport from `desk-ipc-protocol`.
///
/// Reader half is dropped because the media transport is uni-
/// directional (worker → daemon). The daemon does not push
/// commands on this pipe — it uses the event pipe for that.
pub(super) async fn connect_media_pipe(
    pipe_name: &str,
) -> Result<Arc<dyn MediaSender>, Box<dyn std::error::Error>> {
    #[cfg(target_os = "windows")]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        // Same retry loop as the event pipe — the daemon creates the
        // pipe as part of `run_pipe_server` but a fast worker may dial
        // before that point.
        let client = {
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(&pipe_path) {
                    Ok(c) => break c,
                    Err(e) if attempts < 10 => {
                        attempts += 1;
                        warn!(
                            "Media pipe not ready (attempt {}), retrying in 200ms: {}",
                            attempts, e
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
        };
        let (_reader, writer) = tokio::io::split(client);
        Ok(framed::spawn_media_sender(writer))
    }
    #[cfg(not(target_os = "windows"))]
    {
        use tokio::net::UnixStream;
        let stream = UnixStream::connect(pipe_name).await?;
        let (_reader, writer) = tokio::io::split(stream);
        Ok(framed::spawn_media_sender(writer))
    }
}

/// Open the daemon-side **file-transfer** pipe (Windows: named pipe;
/// Unix: domain socket). Unlike [`connect_media_pipe`] this transport
/// is **bidirectional**: the worker emits download chunks / control
/// replies on the writer half, and consumes upload chunks / control
/// commands on the reader half. The framed sender uses
/// `FILE_QUEUE_CAP = 32` so backpressure surfaces at the worker as a
/// parked `send().await` inside the dispatcher's `emit_*` helpers.
pub(super) async fn connect_file_pipe(
    pipe_name: &str,
) -> Result<
    (
        Arc<dyn EventSender<FileTransferPayload>>,
        Box<dyn EventReceiver<FileTransferPayload>>,
    ),
    Box<dyn std::error::Error>,
> {
    #[cfg(target_os = "windows")]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        let client = {
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(&pipe_path) {
                    Ok(c) => break c,
                    Err(e) if attempts < 10 => {
                        attempts += 1;
                        warn!(
                            "File pipe not ready (attempt {}), retrying in 200ms: {}",
                            attempts, e
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                    Err(e) => return Err(Box::new(e)),
                }
            }
        };
        let (reader, writer) = tokio::io::split(client);
        let sender = framed::spawn_file_sender::<_, FileTransferPayload>(writer);
        let receiver = framed::make_event_receiver::<_, FileTransferPayload>(reader);
        Ok((sender, receiver))
    }
    #[cfg(not(target_os = "windows"))]
    {
        use tokio::net::UnixStream;
        let stream = UnixStream::connect(pipe_name).await?;
        let (reader, writer) = tokio::io::split(stream);
        let sender = framed::spawn_file_sender::<_, FileTransferPayload>(writer);
        let receiver = framed::make_event_receiver::<_, FileTransferPayload>(reader);
        Ok((sender, receiver))
    }
}

/// Whether [`WorkerSession::run_with_transports`] should call
/// [`crate::telemetry::init_telemetry`] for itself.
///
/// `shared_hub_is_some` is `true` whenever the worker runs in-process inside
/// the host (portable / DeskServer modes); `false` for the named-pipe
/// SessionWorker path that runs in a dedicated OS process.
///
/// Telemetry init installs the **global default** tracing subscriber, which
/// can only be set once per process. Calling it again from an in-process
/// worker panics with `SetGlobalDefaultError`. Conversely, the named-pipe
/// worker is a separate process whose subscriber slot is empty, so it must
/// init.
pub(super) fn should_init_worker_telemetry(shared_hub_is_some: bool) -> bool {
    !shared_hub_is_some
}

/// Spawn a task that drains the dispatcher-facing mpsc and forwards each
/// message onto the supplied [`EventSender`]. Replaces the old byte-stream
/// writer task so the same forwarder works for the named-pipe path (where
/// the sender is `framed::FramedEventSender`) and the in-process path
/// (where the sender is `inprocess::InProcessEventSender`). Decoupling the
/// forwarder from the main `select!` preserves the property that a slow
/// handler cannot stall heartbeats or other queued outbound messages. The
/// task exits when all dispatcher senders drop (clean shutdown) or when
/// the underlying transport returns `Closed`.
#[cfg(test)]
pub(super) fn spawn_event_forwarder_task(
    rx: mpsc::UnboundedReceiver<WorkerToService>,
    sender: Arc<dyn EventSender<WorkerToService>>,
) -> tokio::task::JoinHandle<()> {
    spawn_profiled_event_forwarder_task(rx, sender, WorkerProfile::SessionUser)
}

pub(super) fn spawn_profiled_event_forwarder_task(
    mut rx: mpsc::UnboundedReceiver<WorkerToService>,
    sender: Arc<dyn EventSender<WorkerToService>>,
    profile: WorkerProfile,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if !msg.allowed_for_profile(profile) {
                error!(
                    "Refusing outbound {:?} from {:?} worker",
                    std::mem::discriminant(&msg),
                    profile
                );
                continue;
            }
            if let Err(e) = sender.send(msg).await {
                warn!("Failed to forward IPC message: {}", e);
                break;
            }
        }
    })
}

/// Spawn an independent heartbeat task that pushes `Heartbeat` to the writer
/// queue every `interval`. Runs in its own task so it stays alive even when
/// the main `select!` is blocked awaiting a long handler. The task exits when
/// the writer queue is closed (writer task gone) or it is aborted.
pub(super) fn spawn_heartbeat_task(
    writer_tx: mpsc::UnboundedSender<WorkerToService>,
    interval: tokio::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut timer = tokio::time::interval(interval);
        loop {
            timer.tick().await;
            let hb = WorkerToService::Heartbeat(HeartbeatPayload {
                timestamp_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                active_connections: 0,
                cpu_usage: None,
                memory_usage: None,
            });
            if writer_tx.send(hb).is_err() {
                break;
            }
        }
    })
}
