use super::*;

impl WorkerSession {
    pub(super) async fn connect_and_serve(
        &self,
        pipe_name: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        info!("WorkerSession connecting to IPC pipe: {}", pipe_name);

        #[cfg(target_os = "windows")]
        let (reader, writer) = self.connect_windows_pipe(pipe_name).await?;

        #[cfg(not(target_os = "windows"))]
        let (reader, writer) = self.connect_unix_socket(pipe_name).await?;

        self.ipc_loop(reader, writer).await
    }

    /// Named-pipe / Unix-socket entry. Performs the Ready / Init handshake
    /// directly on the byte stream (length-prefixed wincode payload — see
    /// [`desk_ipc_protocol::transport::IPC_CONFIG`]), then wraps the remaining
    /// stream in `framed` event transports and connects the optional media
    /// pipe before delegating to [`Self::run_with_transports`]. The
    /// transport-agnostic main loop is shared with the in-process portable
    /// path — only the way transports are constructed differs.
    async fn ipc_loop<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
    ) -> Result<(), Box<dyn std::error::Error>>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        write_message(&mut writer, &WorkerToService::Ready).await?;
        info!("Sent Ready message to Service");

        let init_payload = loop {
            let msg: ServiceToWorker = read_message(&mut reader).await?;
            match msg {
                ServiceToWorker::Init(payload) => {
                    info!(
                        "Received Init: session_id={}, os_session_id={}, desktop={:?}",
                        payload.session_id, payload.os_session_id, payload.desktop_name
                    );
                    break payload;
                }
                ServiceToWorker::Shutdown => {
                    info!("Received Shutdown before Init, exiting");
                    return Ok(());
                }
                other => {
                    warn!("Received {:?} before Init, ignoring", other);
                }
            }
        };

        // Wrap the post-handshake bytes in framed event transports. The
        // wire format (`LengthDelimitedCodec` + wincode payload, see
        // `desk_ipc_protocol::transport::IPC_CONFIG`) is binary compatible
        // with the `read_message` / `write_message` calls above — both speak
        // length-prefixed wincode with the same 16 MB cap.
        let event_tx: Arc<dyn EventSender<WorkerToService>> = framed::spawn_event_sender(writer);
        let event_rx: Box<dyn EventReceiver<ServiceToWorker>> = framed::make_event_receiver(reader);

        // Optional media pipe. Connect failure is non-fatal —
        // the worker continues to serve event-pipe traffic (mouse / clipboard
        // / file transfer / ...) and reports `Capabilities` so the daemon can
        // populate `RequestRemote` Init replies even if no frames flow.
        let media_sender = match init_payload.media_pipe_name.as_deref() {
            Some(name) => {
                info!("Worker connecting to media pipe: {name}");
                match connect_media_pipe(name).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        warn!(
                            "Worker failed to connect to media pipe {name}: {e}; \
                             continuing without media transport"
                        );
                        None
                    }
                }
            }
            None => None,
        };

        // File lane: dedicated bidirectional pipe for download
        // chunks / control replies / upload chunks / cancels — split
        // off from the event lane so SCTP backpressure on a slow
        // browser DataChannel does not head-of-line block heartbeats /
        // manager responses. The daemon always provisions this pipe
        // for named-pipe workers, so a missing `file_pipe_name` is a
        // fatal init error: the worker surfaces an `Error` and exits
        // (no fallback, since that would silently put file bytes back
        // on the event lane).
        let file_pipe_name = match init_payload.file_pipe_name.as_deref() {
            Some(name) => name,
            None => {
                let msg = "WorkerInit lacked file_pipe_name in named-pipe mode; \
                           daemon must provision a dedicated file lane";
                error!("{msg}");
                let err = WorkerToService::Error(desk_ipc_protocol::message::ErrorPayload {
                    code: -1,
                    message: msg.to_string(),
                    recoverable: false,
                    connection_id: None,
                });
                let _ = event_tx.send(err).await;
                return Err(msg.into());
            }
        };
        info!("Worker connecting to file pipe: {file_pipe_name}");
        let (file_sender, file_receiver) = connect_file_pipe(file_pipe_name).await?;

        // Named-pipe path: no shared hub — worker constructs its own
        // (Forwarder if `host_upstream_url` is set, Local otherwise).
        self.run_with_transports(
            init_payload,
            event_rx,
            event_tx,
            media_sender,
            file_sender,
            file_receiver,
            None,
        )
        .await
    }

    #[cfg(target_os = "windows")]
    async fn connect_windows_pipe(
        &self,
        pipe_name: &str,
    ) -> Result<
        (
            impl AsyncRead + Unpin + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
        Box<dyn std::error::Error>,
    > {
        use tokio::net::windows::named_pipe::ClientOptions;

        let pipe_path = format!(r"\\.\pipe\{}", pipe_name);
        info!("Connecting to Named Pipe: {}", pipe_path);

        let client = {
            let mut attempts = 0;
            loop {
                match ClientOptions::new().open(&pipe_path) {
                    Ok(client) => break client,
                    Err(e) if attempts < 10 => {
                        attempts += 1;
                        warn!(
                            "Pipe not ready (attempt {}), retrying in 500ms: {}",
                            attempts, e
                        );
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to pipe after {} attempts: {}",
                            attempts, e
                        );
                        return Err(Box::new(e));
                    }
                }
            }
        };

        let (reader, writer) = tokio::io::split(client);
        Ok((reader, writer))
    }

    #[cfg(not(target_os = "windows"))]
    async fn connect_unix_socket(
        &self,
        socket_path: &str,
    ) -> Result<
        (
            impl AsyncRead + Unpin + Send + 'static,
            impl AsyncWrite + Unpin + Send + 'static,
        ),
        Box<dyn std::error::Error>,
    > {
        use tokio::net::UnixStream;

        info!("Connecting to Unix socket: {}", socket_path);
        let stream = UnixStream::connect(socket_path).await?;
        let (reader, writer) = tokio::io::split(stream);
        Ok((reader, writer))
    }
}
