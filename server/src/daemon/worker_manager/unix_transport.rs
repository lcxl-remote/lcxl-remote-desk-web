//! Unix-domain socket transport for session workers.

use super::*;

#[cfg(not(target_os = "windows"))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_pipe_server(
    socket_path: &str,
    session_id: u32,
    desktop_name: Option<String>,
    config_json: String,
    log_dir: String,
    data_dir: String,
    mut cmd_rx: mpsc::UnboundedReceiver<ServiceToWorker>,
    msg_tx: WorkerMessageSink,
    worker_mgr: WorkerManager,
    host_upstream_url: String,
    ipc_token: Option<String>,
    pc_registry: PcRegistry,
    file_sender_slot: Arc<RwLock<Option<Arc<dyn EventSender<FileTransferPayload>>>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    let incarnation = msg_tx.incarnation();
    info!("Creating Unix socket server for worker {incarnation}: {socket_path}");
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;

    // Mirror the Windows path: dedicated file-lane Unix socket so SCTP
    // backpressure on a slow browser DC does not head-of-line block
    // event-lane traffic. Mandatory; on accept failure we fall into
    // recovery rather than degrading to event-lane fallback.
    let file_socket_path = format!("{socket_path}-file");
    let _ = std::fs::remove_file(&file_socket_path);
    let file_listener = UnixListener::bind(&file_socket_path)?;

    let desktop_name_copy = desktop_name.clone();

    info!("Waiting for Worker to connect...");
    let stream = match tokio::time::timeout(Duration::from_secs(15), listener.accept()).await {
        Ok(Ok((stream, _))) => {
            info!("Worker connected");
            stream
        }
        Ok(Err(e)) => {
            error!("Unix socket accept error for {socket_path}: {e}");
            worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
            let _ = std::fs::remove_file(socket_path);
            let _ = std::fs::remove_file(&file_socket_path);
            return Ok(());
        }
        Err(_) => {
            warn!("Timed out waiting for worker to connect on {socket_path}; triggering recovery");
            worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
            let _ = std::fs::remove_file(socket_path);
            let _ = std::fs::remove_file(&file_socket_path);
            return Ok(());
        }
    };

    let (mut reader, mut writer) = tokio::io::split(stream);

    match read_message::<_, WorkerToService>(&mut reader).await? {
        WorkerToService::Ready => info!("Worker reported Ready"),
        other => warn!("Expected Ready, got: {other:?}"),
    }

    let remote_access_state = worker_mgr.remote_access_state();
    write_message(
        &mut writer,
        &ServiceToWorker::Init(WorkerInitPayload {
            session_id: format!("session-{session_id}"),
            os_session_id: session_id,
            desktop_name,
            config_json,
            log_dir: Some(log_dir),
            data_dir: Some(data_dir),
            signaling_url: None,
            auth_token: ipc_token,
            host_upstream_url: Some(host_upstream_url),
            // This Unix-socket path does not create a dedicated media
            // socket, so the worker runs without a separate media
            // transport (single-pipe fallback).
            media_pipe_name: None,
            file_pipe_name: Some(file_socket_path.clone()),
            remote_access_locked: remote_access_state.is_locked(),
            remote_access_state_version: remote_access_state.state_version,
        }),
    )
    .await?;

    // Wait for the worker to dial back on the file Unix socket. Same
    // policy as Windows: mandatory, recover on failure.
    let file_drain_handle =
        match tokio::time::timeout(Duration::from_secs(15), file_listener.accept()).await {
            Ok(Ok((file_stream, _))) => {
                info!("Worker connected on file socket {file_socket_path}");
                let (file_reader, file_writer) = tokio::io::split(file_stream);
                let sender = framed::spawn_file_sender::<_, FileTransferPayload>(file_writer);
                *file_sender_slot.write().await = Some(sender);
                let receiver = framed::make_event_receiver::<_, FileTransferPayload>(file_reader);
                Some(spawn_file_drain_task(
                    receiver,
                    pc_registry.clone(),
                    msg_tx.gate(),
                ))
            }
            Ok(Err(e)) => {
                warn!(
                    "File socket accept failed for {file_socket_path}: {e}; \
                 dropping into recovery (no file lane = no file transfer)"
                );
                worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&file_socket_path);
                return Ok(());
            }
            Err(_) => {
                warn!(
                    "Timed out waiting for worker on file socket {file_socket_path}; \
                 dropping into recovery"
                );
                worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
                let _ = std::fs::remove_file(socket_path);
                let _ = std::fs::remove_file(&file_socket_path);
                return Ok(());
            }
        };

    // Keep-PC: see the Windows path above; browser-facing DesktopReady is
    // not emitted on worker spawn.

    let expected = bridge_loop(reader, writer, &mut cmd_rx, &msg_tx, socket_path).await;
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(&file_socket_path);

    if let Some(handle) = file_drain_handle {
        handle.abort();
    }
    *file_sender_slot.write().await = None;

    if !expected {
        worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
    }

    Ok(())
}
