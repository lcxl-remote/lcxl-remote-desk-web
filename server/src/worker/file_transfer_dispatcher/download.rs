use super::*;

impl FileTransferDispatcher {
    pub(super) async fn serve_download(
        &self,
        connection_id: String,
        req: DownloadRequest,
    ) -> std::io::Result<()> {
        let path = PathBuf::from(&req.file_path);
        if !path.exists() || !path.is_file() {
            if let Err(e) = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: req.transfer_id.clone(),
                        error_code: DeskErrorCode::FILE_PATH_NOT_FOUND,
                        message: format!("File not found: {}", req.file_path),
                    }),
                )
                .await
            {
                warn!(
                    "[FileTransferDispatcher] {}: failed to emit TransferError for {}: {e}",
                    connection_id, req.transfer_id
                );
            }
            return Ok(());
        }
        let metadata = tokio::fs::metadata(&path).await?;
        let file_size = metadata.len();
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let chunk_size = FILE_TRANSFER_CHUNK_SIZE_TX;
        let total_chunks = file_size.div_ceil(chunk_size as u64);
        let mut file = tokio::fs::File::open(&path).await?;

        if !self
            .start_activity(
                &connection_id,
                &req.transfer_id,
                FileTransferDirection::Download,
                &file_name,
                file_size,
            )
            .await
        {
            self.emit_text(
                &connection_id,
                FileTransferMessage::TransferError(TransferError {
                    transfer_id: req.transfer_id.clone(),
                    error_code: DeskErrorCode::INVALID_STATE,
                    message: "Transfer id is already active on this connection".to_string(),
                }),
            )
            .await
            .ok();
            return Ok(());
        }

        if let Err(e) = self
            .emit_text(
                &connection_id,
                FileTransferMessage::DownloadResponse(DownloadResponse {
                    transfer_id: req.transfer_id.clone(),
                    file_name: file_name.clone(),
                    file_size,
                    chunk_size,
                    total_chunks,
                }),
            )
            .await
        {
            warn!(
                "[FileTransferDispatcher] {}: download {} aborted before open: {e}",
                connection_id, req.transfer_id
            );
            self.finish_activity(
                &connection_id,
                &req.transfer_id,
                FileTransferOutcome::Failed,
            )
            .await;
            return Ok(());
        }
        let mut buf = vec![0u8; chunk_size];
        let mut chunk_index: u32 = 0;
        let mut window = DownloadWindow::default();
        info!(
            "[FileTransferDispatcher] download {} starting: {} chunks",
            req.transfer_id, total_chunks
        );
        loop {
            // Check cancel flag before doing more IO.
            let cancelled = {
                let mut inner = self.inner.lock().await;
                inner
                    .cancelled_transfers
                    .remove(&TransferKey::new(&connection_id, &req.transfer_id))
            };
            if cancelled {
                info!(
                    "[FileTransferDispatcher] download {} cancelled",
                    req.transfer_id
                );
                self.finish_activity(
                    &connection_id,
                    &req.transfer_id,
                    FileTransferOutcome::Cancelled,
                )
                .await;
                return Ok(());
            }
            let iter_start = Instant::now();
            let read_start = Instant::now();
            let n = match file.read(&mut buf).await {
                Ok(value) => value,
                Err(error) => {
                    self.finish_activity(
                        &connection_id,
                        &req.transfer_id,
                        FileTransferOutcome::Failed,
                    )
                    .await;
                    return Err(error);
                }
            };
            let read_elapsed = read_start.elapsed();
            if n == 0 {
                break;
            }
            let build_start = Instant::now();
            let chunk_bytes = build_binary_chunk(&req.transfer_id, chunk_index, &buf[..n]);
            let build_elapsed = build_start.elapsed();
            // Fail-fast on file-lane closure: dropping `file` here
            // releases the OS handle promptly. Continuing to read
            // would just fill memory while no one drains.
            let emit_start = Instant::now();
            if let Err(e) = self
                .emit_binary(&connection_id, &req.transfer_id, chunk_bytes)
                .await
            {
                warn!(
                    "[FileTransferDispatcher] {}: download {} aborted at chunk {}: {e}",
                    connection_id, req.transfer_id, chunk_index
                );
                self.finish_activity(
                    &connection_id,
                    &req.transfer_id,
                    FileTransferOutcome::Failed,
                )
                .await;
                return Ok(());
            }
            let emit_elapsed = emit_start.elapsed();
            window.record(
                n as u64,
                read_elapsed,
                build_elapsed,
                emit_elapsed,
                iter_start.elapsed(),
            );
            if window.is_full() {
                if let Some(line) = window.flush_line(&req.transfer_id, "ft-metrics") {
                    info!("{line}");
                }
                window.reset();
            }
            chunk_index += 1;
            if chunk_index.is_multiple_of(YIELD_EVERY_N_CHUNKS) {
                tokio::task::yield_now().await;
            }
        }
        // Flush any trailing partial window so a small file or the
        // last few chunks of a large file still surface in the log.
        if let Some(line) = window.flush_line(&req.transfer_id, "ft-metrics") {
            info!("{line}");
        }
        if let Err(e) = self
            .emit_text(
                &connection_id,
                FileTransferMessage::TransferComplete(TransferComplete {
                    transfer_id: req.transfer_id.clone(),
                }),
            )
            .await
        {
            warn!(
                "[FileTransferDispatcher] {}: TransferComplete emit failed for {}: {e}",
                connection_id, req.transfer_id
            );
            self.finish_activity(
                &connection_id,
                &req.transfer_id,
                FileTransferOutcome::Failed,
            )
            .await;
            return Ok(());
        }
        self.finish_activity(
            &connection_id,
            &req.transfer_id,
            FileTransferOutcome::Completed,
        )
        .await;
        info!(
            "[FileTransferDispatcher] download {} completed: {} bytes, {} chunks",
            req.transfer_id, file_size, chunk_index
        );
        Ok(())
    }

    /// Emit a JSON-encoded control message over the file lane. Returns
    /// [`TransportError::Closed`] if the lane has gone away; serialization
    /// failures are programmer bugs (`FileTransferMessage` is plain
    /// serde) and are logged + dropped without surfacing as a transport
    /// error.
    pub(super) async fn emit_text(
        &self,
        connection_id: &str,
        msg: FileTransferMessage,
    ) -> Result<(), TransportError> {
        let transfer_id = transfer_id_of(&msg).map(str::to_string);
        let json = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                error!("[FileTransferDispatcher] serialize control message failed: {e}");
                return Ok(());
            }
        };
        self.emit_payload(connection_id, json, true, transfer_id)
            .await
    }

    async fn emit_binary(
        &self,
        connection_id: &str,
        transfer_id: &str,
        data: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.emit_payload(connection_id, data, false, Some(transfer_id.to_string()))
            .await
    }

    async fn emit_payload(
        &self,
        connection_id: &str,
        data: Vec<u8>,
        is_text: bool,
        transfer_id: Option<String>,
    ) -> Result<(), TransportError> {
        let payload = FileTransferPayload {
            connection_id: connection_id.to_string(),
            data,
            is_text,
            transfer_id,
        };
        // `send().await` parks on a full file lane (FILE_QUEUE_CAP = 32),
        // which is exactly the backpressure we want — see module docs.
        // It only returns `Err(Closed)` when the daemon-side receiver
        // has dropped (peer crash / shutdown), in which case callers
        // should fail-fast and release any open file handle.
        self.file_sender.send(payload).await
    }
}
