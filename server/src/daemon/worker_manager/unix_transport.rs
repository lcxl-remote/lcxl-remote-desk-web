//! Unix-domain socket transport for session workers.

use super::*;

#[cfg(not(target_os = "windows"))]
pub(super) fn cleanup_worker_socket_paths(socket_path: &str) {
    let _ = std::fs::remove_file(socket_path);
    let _ = std::fs::remove_file(format!("{socket_path}-media"));
    let _ = std::fs::remove_file(format!("{socket_path}-file"));
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

#[cfg(not(target_os = "windows"))]
pub(super) fn allocate_worker_socket_path(
    session_id: u32,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    allocate_worker_socket_path_for(session_id, None)
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)] // consumed by the resident Linux worker launcher
pub(super) fn allocate_worker_socket_path_for(
    session_id: u32,
    owner: Option<(u32, u32)>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let effective_uid = unsafe { libc::geteuid() };
    let runtime_root = if effective_uid == 0 {
        std::path::PathBuf::from("/run/lcxl-remote-desk/workers")
    } else {
        std::env::temp_dir().join(format!("lcxl-remote-desk-{effective_uid}/workers"))
    };
    std::fs::create_dir_all(&runtime_root)?;
    std::fs::set_permissions(&runtime_root, std::fs::Permissions::from_mode(0o700))?;

    let metadata = std::fs::symlink_metadata(&runtime_root)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!("unsafe worker runtime directory {}", runtime_root.display()).into());
    }

    let worker_dir = runtime_root.join(format!("session-{session_id}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&worker_dir)?;
    std::fs::set_permissions(&worker_dir, std::fs::Permissions::from_mode(0o700))?;
    if let Some((uid, gid)) = owner {
        if effective_uid != 0 {
            return Err("only a root daemon can assign a worker runtime directory owner".into());
        }
        chown_path(&worker_dir, uid, gid)?;
    }
    Ok(worker_dir.join("event.sock").to_string_lossy().into_owned())
}

#[cfg(not(target_os = "windows"))]
fn chown_path(path: &std::path::Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if unsafe { libc::chown(path.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn secure_worker_socket(path: &std::path::Path, owner: Option<(u32, u32)>) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    if let Some((uid, gid)) = owner {
        if unsafe { libc::geteuid() } != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "only root may assign worker socket ownership",
            ));
        }
        chown_path(path, uid, gid)?;
    }
    let metadata = std::fs::symlink_metadata(path)?;
    let expected_uid = owner.map_or_else(|| unsafe { libc::geteuid() }, |value| value.0);
    let expected_gid = owner.map_or_else(|| unsafe { libc::getegid() }, |value| value.1);
    if !metadata.file_type().is_socket()
        || metadata.uid() != expected_uid
        || metadata.gid() != expected_gid
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "worker socket ownership or mode did not converge",
        ));
    }
    Ok(())
}

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
    worker_identity: Option<WorkerIdentity>,
    socket_owner: Option<(u32, u32)>,
    transport_ready_tx: oneshot::Sender<()>,
) -> Result<(), Box<dyn std::error::Error>> {
    use tokio::net::UnixListener;

    let incarnation = msg_tx.incarnation();
    let worker_key = msg_tx.worker_key().cloned();
    info!("Creating Unix socket server for worker {incarnation}: {socket_path}");
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;

    let media_socket_path = format!("{socket_path}-media");
    let _ = std::fs::remove_file(&media_socket_path);
    let media_listener = match UnixListener::bind(&media_socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            cleanup_worker_socket_paths(socket_path);
            return Err(error.into());
        }
    };

    // Mirror the Windows path: dedicated file-lane Unix socket so SCTP
    // backpressure on a slow browser DC does not head-of-line block
    // event-lane traffic. Mandatory; on accept failure we fall into
    // recovery rather than degrading to event-lane fallback.
    let file_socket_path = format!("{socket_path}-file");
    let _ = std::fs::remove_file(&file_socket_path);
    let file_listener = match UnixListener::bind(&file_socket_path) {
        Ok(listener) => listener,
        Err(error) => {
            cleanup_worker_socket_paths(socket_path);
            return Err(error.into());
        }
    };
    if let Err(error) = secure_worker_socket(std::path::Path::new(socket_path), socket_owner)
        .and_then(|()| secure_worker_socket(std::path::Path::new(&media_socket_path), socket_owner))
        .and_then(|()| secure_worker_socket(std::path::Path::new(&file_socket_path), socket_owner))
    {
        cleanup_worker_socket_paths(socket_path);
        return Err(error.into());
    }

    // The process must not be spawned until both mandatory listeners exist.
    // Otherwise a fast worker can lose the connect race and crash-loop.
    let _ = transport_ready_tx.send(());

    let desktop_name_copy = desktop_name.clone();

    info!("Waiting for Worker to connect...");
    let stream = match tokio::time::timeout(Duration::from_secs(15), listener.accept()).await {
        Ok(Ok((stream, _))) => {
            info!("Worker connected");
            stream
        }
        Ok(Err(e)) => {
            error!("Unix socket accept error for {socket_path}: {e}");
            worker_mgr.handle_worker_transport_failure(
                worker_key.clone(),
                incarnation,
                session_id,
                desktop_name_copy,
            );
            cleanup_worker_socket_paths(socket_path);
            return Ok(());
        }
        Err(_) => {
            warn!("Timed out waiting for worker to connect on {socket_path}; triggering recovery");
            worker_mgr.handle_worker_transport_failure(
                worker_key.clone(),
                incarnation,
                session_id,
                desktop_name_copy,
            );
            cleanup_worker_socket_paths(socket_path);
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
            worker_identity,
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
            media_pipe_name: Some(media_socket_path.clone()),
            file_pipe_name: Some(file_socket_path.clone()),
            remote_access_locked: remote_access_state.is_locked(),
            remote_access_state_version: remote_access_state.state_version,
        }),
    )
    .await?;

    let media_handle =
        match tokio::time::timeout(Duration::from_secs(15), media_listener.accept()).await {
            Ok(Ok((media_stream, _))) => {
                info!("Worker connected on media socket {media_socket_path}");
                let (media_reader, _media_writer) = tokio::io::split(media_stream);
                let receiver = framed::make_media_receiver(media_reader);
                spawn_media_receiver_task(receiver, pc_registry.clone(), msg_tx.gate())
            }
            Ok(Err(error)) => {
                warn!("Media socket accept failed for {media_socket_path}: {error}");
                worker_mgr.handle_worker_transport_failure(
                    worker_key.clone(),
                    incarnation,
                    session_id,
                    desktop_name_copy,
                );
                cleanup_worker_socket_paths(socket_path);
                return Ok(());
            }
            Err(_) => {
                warn!("Timed out waiting for worker on media socket {media_socket_path}");
                worker_mgr.handle_worker_transport_failure(
                    worker_key.clone(),
                    incarnation,
                    session_id,
                    desktop_name_copy,
                );
                cleanup_worker_socket_paths(socket_path);
                return Ok(());
            }
        };

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
                worker_mgr.handle_worker_transport_failure(
                    worker_key.clone(),
                    incarnation,
                    session_id,
                    desktop_name_copy,
                );
                cleanup_worker_socket_paths(socket_path);
                return Ok(());
            }
            Err(_) => {
                warn!(
                    "Timed out waiting for worker on file socket {file_socket_path}; \
                 dropping into recovery"
                );
                worker_mgr.handle_worker_transport_failure(
                    worker_key.clone(),
                    incarnation,
                    session_id,
                    desktop_name_copy,
                );
                cleanup_worker_socket_paths(socket_path);
                return Ok(());
            }
        };

    // Keep-PC: see the Windows path above; browser-facing DesktopReady is
    // not emitted on worker spawn.

    let expected = bridge_loop(reader, writer, &mut cmd_rx, &msg_tx, socket_path).await;
    cleanup_worker_socket_paths(socket_path);

    if let Some(handle) = file_drain_handle {
        handle.abort();
    }
    media_handle.abort();
    *file_sender_slot.write().await = None;

    if !expected {
        worker_mgr.handle_worker_transport_failure(
            worker_key,
            incarnation,
            session_id,
            desktop_name_copy,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn allocated_worker_socket_path_is_absolute_and_private() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let socket_path = allocate_worker_socket_path(42).unwrap();
        let socket_path = std::path::Path::new(&socket_path);
        let worker_dir = socket_path.parent().unwrap();
        let metadata = std::fs::symlink_metadata(worker_dir).unwrap();

        assert!(socket_path.is_absolute());
        assert_eq!(socket_path.file_name().unwrap(), "event.sock");
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);

        cleanup_worker_socket_paths(socket_path.to_str().unwrap());
        assert!(!worker_dir.exists());
    }

    #[test]
    fn bound_worker_socket_is_private_before_process_launch() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let socket_path = allocate_worker_socket_path(43).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        secure_worker_socket(std::path::Path::new(&socket_path), None).unwrap();
        let metadata = std::fs::symlink_metadata(&socket_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        drop(listener);
        cleanup_worker_socket_paths(&socket_path);
    }
}
