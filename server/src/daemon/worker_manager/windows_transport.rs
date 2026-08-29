//! Windows named-pipe transport for session workers.

use super::*;

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_pipe_server(
    pipe_name: &str,
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
    let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
    let incarnation = msg_tx.incarnation();
    info!("Creating Named Pipe server for worker {incarnation}: {pipe_path}");

    // Look up the SID owning the target session so the pipe ACL grants
    // access only to SYSTEM + Administrators + that user. Interactive
    // ServiceDaemon runs may lack WTSQueryUserToken privileges, so the
    // resolver can use the daemon account only when it is in this exact
    // Windows session. A failure still falls back to SY+BA only (never to
    // "Everyone") — see `pipe_security::query_worker_pipe_user_sid`.
    let allowed_user_sid =
        match crate::daemon::pipe_security::query_worker_pipe_user_sid(session_id) {
            Ok(sid) => sid,
            Err(e) => {
                warn!(
                    "Failed to query user SID for session {session_id}: {e}; \
                 falling back to SY+BA-only pipe ACL"
                );
                None
            }
        };
    let sddl_str = crate::daemon::pipe_security::build_pipe_sddl(allowed_user_sid.as_deref());
    info!("Pipe ACL SDDL = '{sddl_str}'");

    let server = create_named_pipe_with_sddl(&pipe_path, &sddl_str)?;

    // Pre-create the secondary "media" pipe under the
    // same ACL so it exists by the time the worker (which receives the
    // pipe name in Init) tries to connect. Creating both up-front means
    // the worker never races against pipe creation; it only ever races
    // against connect.
    let media_pipe_name = format!("{pipe_name}-media");
    let media_pipe_path = format!(r"\\.\pipe\{media_pipe_name}");
    let media_server = create_named_pipe_with_sddl(&media_pipe_path, &sddl_str)?;

    // File lane: third dedicated pipe for file-transfer data
    // (download chunks / control replies / upload chunks / cancels)
    // running independent of event + media so SCTP backpressure on a
    // slow browser DC propagates end-to-end without HOL-blocking
    // heartbeat or signaling.
    let file_pipe_name = format!("{pipe_name}-file");
    let file_pipe_path = format!(r"\\.\pipe\{file_pipe_name}");
    let file_server = create_named_pipe_with_sddl(&file_pipe_path, &sddl_str)?;

    let desktop_name_copy = desktop_name.clone();

    info!("Waiting for Worker to connect on {pipe_path}...");
    match tokio::time::timeout(Duration::from_secs(15), server.connect()).await {
        Ok(Ok(())) => info!("Worker connected"),
        Ok(Err(e)) => {
            error!("Pipe connection error for {pipe_path}: {e}");
            worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
            return Ok(());
        }
        Err(_) => {
            warn!("Timed out waiting for worker to connect on {pipe_path}; triggering recovery");
            worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
            return Ok(());
        }
    }

    let (mut reader, mut writer) = tokio::io::split(server);

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
            media_pipe_name: Some(media_pipe_name.clone()),
            file_pipe_name: Some(file_pipe_name.clone()),
            remote_access_locked: remote_access_state.is_locked(),
            remote_access_state_version: remote_access_state.state_version,
        }),
    )
    .await?;
    info!(
        "Sent Init to Worker (media_pipe_name={}, file_pipe_name={})",
        media_pipe_name, file_pipe_name
    );

    // Wait for the worker to dial back on the media pipe. The connect
    // timeout is generous because some workers (Winlogon under SYSTEM
    // token) take longer to spin up their media producer; on timeout we
    // proceed *without* media so the rest of the IPC continues to work,
    // and surface a warning so operators know media frames will not flow
    // for this worker.
    let media_handle =
        match tokio::time::timeout(Duration::from_secs(15), media_server.connect()).await {
            Ok(Ok(())) => {
                info!("Worker connected on media pipe {media_pipe_path}");
                let (media_reader, _media_writer) = tokio::io::split(media_server);
                let receiver = framed::make_media_receiver(media_reader);
                Some(spawn_media_receiver_task(
                    receiver,
                    pc_registry.clone(),
                    msg_tx.gate(),
                ))
            }
            Ok(Err(e)) => {
                warn!(
                    "Media pipe connect failed for {media_pipe_path}: {e}; \
                 worker will run without media transport (no video frames will flow)"
                );
                None
            }
            Err(_) => {
                warn!(
                    "Timed out waiting for worker on media pipe {media_pipe_path}; \
                 worker will run without media transport"
                );
                None
            }
        };

    // Wait for the worker to dial back on the file pipe. Unlike media,
    // the file lane is mandatory: file_transfer is the only way the
    // browser's file UI talks to the host worker, and routing it onto
    // the event lane on failure would silently restore the HOL bug
    // fix-2026-05-05 was supposed to prevent. On accept failure we
    // surface a warning and drop into recovery rather than degrading.
    let file_drain_handle =
        match tokio::time::timeout(Duration::from_secs(15), file_server.connect()).await {
            Ok(Ok(())) => {
                info!("Worker connected on file pipe {file_pipe_path}");
                let (file_reader, file_writer) = tokio::io::split(file_server);
                let sender = framed::spawn_file_sender::<_, FileTransferPayload>(file_writer);
                // Publish the sender into the slot so DC forwarders'
                // `WorkerManager::send_file_to_worker` look-ups resolve.
                *file_sender_slot.write().await = Some(sender);
                let receiver = framed::make_event_receiver::<_, FileTransferPayload>(file_reader);
                // Daemon-side drain task: each worker→daemon payload feeds
                // straight into `pc_manager::write_file_transfer_data`. A
                // single serial drain accepts cross-connection HOL as a
                // documented trade-off; per-connection lanes can be added
                // later if it becomes a problem.
                Some(spawn_file_drain_task(
                    receiver,
                    pc_registry.clone(),
                    msg_tx.gate(),
                ))
            }
            Ok(Err(e)) => {
                warn!(
                    "File pipe connect failed for {file_pipe_path}: {e}; \
                 dropping into recovery (no file lane = no file transfer)"
                );
                worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
                return Ok(());
            }
            Err(_) => {
                warn!(
                    "Timed out waiting for worker on file pipe {file_pipe_path}; \
                 dropping into recovery"
                );
                worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
                return Ok(());
            }
        };

    // Keep-PC semantics: browser-facing `SignalingType::DesktopReady` is
    // not emitted on worker (re)spawn. The browser's WebRTC PC stays up
    // across worker swaps; the daemon's `signaling_proxy` calls
    // `pc_registry.resume_active_media` on the worker's first
    // `Capabilities` to re-issue cached `StartMedia` + `ForceKeyframe`,
    // and the per-PC `media_paused` flag clears on the first IDR.

    let expected = bridge_loop(reader, writer, &mut cmd_rx, &msg_tx, pipe_name).await;
    info!("Pipe server for {pipe_name} exiting");

    // Stop the auxiliary readers so their tasks don't keep a reference
    // to the now-dead worker pipe alive.
    if let Some(handle) = media_handle {
        handle.abort();
    }
    if let Some(handle) = file_drain_handle {
        handle.abort();
    }
    // Drop the file-lane sender so any in-flight `send_file_to_worker`
    // call observes `Closed` instead of stalling indefinitely on the
    // dead pipe writer.
    *file_sender_slot.write().await = None;

    if !expected {
        worker_mgr.handle_crash_recovery(incarnation, session_id, desktop_name_copy);
    }

    Ok(())
}

/// Build a `tokio::net::windows::named_pipe::NamedPipeServer` whose
/// DACL is derived from the supplied SDDL string. Pulled out so the
/// event pipe and the media pipe share exactly the same ACL
/// path — the security analysis in `pipe_security` covers both.
#[cfg(target_os = "windows")]
pub(super) fn create_named_pipe_with_sddl(
    pipe_path: &str,
    sddl_str: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer, Box<dyn std::error::Error>> {
    use tokio::net::windows::named_pipe::ServerOptions;
    unsafe {
        use std::ffi::c_void;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
        use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
        use windows_core::PCWSTR;

        let sddl_w: Vec<u16> = std::ffi::OsStr::new(sddl_str)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut sd: PSECURITY_DESCRIPTOR = PSECURITY_DESCRIPTOR::default();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            1, // SDDL_REVISION_1
            &mut sd,
            None,
        )
        .is_err()
        {
            return Err("Failed to convert SDDL to Security Descriptor".into());
        }

        let mut sa = SECURITY_ATTRIBUTES::default();
        sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
        sa.lpSecurityDescriptor = sd.0 as *mut c_void;
        sa.bInheritHandle = windows::Win32::Foundation::FALSE;

        let srv_res = ServerOptions::new()
            .first_pipe_instance(true)
            .create_with_security_attributes_raw(pipe_path, &mut sa as *mut _ as *mut c_void);

        let _ = LocalFree(Some(HLOCAL(sd.0)));
        Ok(srv_res?)
    }
}
