use super::*;

impl FileTransferDispatcher {
    pub(super) async fn handle_text(&self, payload: FileTransferPayload) {
        let s = match std::str::from_utf8(&payload.data) {
            Ok(s) => s,
            Err(e) => {
                error!(
                    "[FileTransferDispatcher] {}: control payload not UTF-8: {e}",
                    payload.connection_id
                );
                return;
            }
        };
        let msg: FileTransferMessage = match serde_json::from_str(s) {
            Ok(m) => m,
            Err(e) => {
                error!(
                    "[FileTransferDispatcher] {}: control JSON decode failed: {e}",
                    payload.connection_id
                );
                return;
            }
        };
        match msg {
            FileTransferMessage::DownloadRequest(req) => {
                let dispatcher = self.clone();
                let connection_id = payload.connection_id.clone();
                let transfer_id = req.transfer_id.clone();
                tokio::spawn(async move {
                    if let Err(e) = dispatcher.serve_download(connection_id.clone(), req).await {
                        error!("[FileTransferDispatcher] download error: {e}");
                        dispatcher
                            .finish_activity(
                                &connection_id,
                                &transfer_id,
                                FileTransferOutcome::Failed,
                            )
                            .await;
                    }
                });
            }
            FileTransferMessage::UploadRequest(req) => {
                if let Err(e) = self.accept_upload(payload.connection_id.clone(), req).await {
                    error!("[FileTransferDispatcher] upload accept error: {e}");
                }
            }
            FileTransferMessage::TransferComplete(complete) => {
                let key = TransferKey::new(&payload.connection_id, &complete.transfer_id);
                let state = {
                    let mut inner = self.inner.lock().await;
                    inner.upload_states.remove(&key)
                };
                if let Some(mut state) = state {
                    let received_chunks = state.received_chunks;
                    let flush_error = state.file.flush().await.err();
                    let is_complete = flush_error.is_none()
                        && state.received_chunks == state.total_chunks
                        && state.received_bytes == state.expected_bytes;
                    let failure_message = match flush_error {
                        Some(error) => format!("Failed to flush upload: {error}"),
                        None => format!(
                            "Upload size mismatch: expected {} bytes in {} chunks, received {} bytes in {} chunks",
                            state.expected_bytes,
                            state.total_chunks,
                            state.received_bytes,
                            state.received_chunks
                        ),
                    };
                    let file_path = state.file_path.clone();
                    drop(state);
                    info!(
                        "[FileTransferDispatcher] upload {} completed, received {} chunks",
                        complete.transfer_id, received_chunks
                    );
                    if is_complete {
                        self.finish_activity(
                            &payload.connection_id,
                            &complete.transfer_id,
                            FileTransferOutcome::Completed,
                        )
                        .await;
                    } else {
                        let _ = tokio::fs::remove_file(file_path).await;
                        let _ = self
                            .emit_text(
                                &payload.connection_id,
                                FileTransferMessage::TransferError(TransferError {
                                    transfer_id: complete.transfer_id.clone(),
                                    message: failure_message,
                                }),
                            )
                            .await;
                        self.finish_activity(
                            &payload.connection_id,
                            &complete.transfer_id,
                            FileTransferOutcome::Failed,
                        )
                        .await;
                    }
                }
            }
            FileTransferMessage::TransferCancel(cancel) => {
                info!(
                    "[FileTransferDispatcher] {}: cancel transfer_id={}",
                    payload.connection_id, cancel.transfer_id
                );
                let key = TransferKey::new(&payload.connection_id, &cancel.transfer_id);
                let removed_upload = {
                    let mut inner = self.inner.lock().await;
                    inner.cancelled_transfers.insert(key.clone());
                    inner.upload_states.remove(&key)
                };
                if let Some(state) = removed_upload {
                    let path = state.file_path.clone();
                    drop(state);
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        warn!(
                            "[FileTransferDispatcher] failed to remove cancelled upload file {}: {e}",
                            path.display()
                        );
                    }
                }
                self.finish_activity(
                    &payload.connection_id,
                    &cancel.transfer_id,
                    FileTransferOutcome::Cancelled,
                )
                .await;
            }
            other => {
                debug!(
                    "[FileTransferDispatcher] {}: ignoring browser-side message variant {:?}",
                    payload.connection_id, other
                );
            }
        }
    }

    pub(super) async fn handle_binary(&self, payload: FileTransferPayload) {
        let iter_start = Instant::now();
        let (transfer_id, chunk_index, chunk_data) = match parse_binary_chunk(&payload.data) {
            Some(value) => value,
            None => {
                warn!(
                    "[FileTransferDispatcher] {}: binary chunk too short ({} bytes)",
                    payload.connection_id,
                    payload.data.len()
                );
                return;
            }
        };
        let transfer_id = transfer_id.to_string();
        let connection_id = payload.connection_id.clone();
        let key = TransferKey::new(&connection_id, &transfer_id);
        let chunk_data = chunk_data.to_vec();
        let chunk_len = chunk_data.len() as u64;
        let lock_start = Instant::now();
        let mut inner = self.inner.lock().await;
        let lock_elapsed = lock_start.elapsed();
        let Some(state) = inner.upload_states.get_mut(&key) else {
            warn!(
                "[FileTransferDispatcher] {}: chunk for unknown transfer {}",
                connection_id, transfer_id
            );
            return;
        };
        let write_start = Instant::now();
        if let Err(error) = state.file.write_all(&chunk_data).await {
            error!(
                "[FileTransferDispatcher] {}: write chunk {} for {} failed: {error}",
                connection_id, chunk_index, transfer_id
            );
            let failed = inner.upload_states.remove(&key);
            drop(inner);
            if let Some(failed) = failed {
                let _ = tokio::fs::remove_file(failed.file_path).await;
            }
            self.finish_activity(&connection_id, &transfer_id, FileTransferOutcome::Failed)
                .await;
            return;
        }
        let write_elapsed = write_start.elapsed();
        state.received_chunks += 1;
        state.received_bytes += chunk_len;
        state
            .metrics
            .record(chunk_len, lock_elapsed, write_elapsed, iter_start.elapsed());
        if state.metrics.is_full() {
            if let Some(line) = state.metrics.flush_line(&transfer_id, "ft-metrics") {
                info!("{line}");
            }
            state.metrics.reset();
        }
        if state.received_chunks < state.total_chunks {
            return;
        }
        let Some(mut state) = inner.upload_states.remove(&key) else {
            return;
        };
        let flush_result = state.file.flush().await;
        if let Some(line) = state.metrics.flush_line(&transfer_id, "ft-metrics") {
            info!("{line}");
        }
        let file_path = state.file_path.clone();
        let expected_bytes = state.expected_bytes;
        let received_bytes = state.received_bytes;
        drop(state);
        drop(inner);
        if let Err(error) = flush_result {
            error!(
                "[FileTransferDispatcher] {}: flush of completed upload {} failed: {error}",
                connection_id, transfer_id
            );
            let _ = tokio::fs::remove_file(file_path).await;
            self.finish_activity(&connection_id, &transfer_id, FileTransferOutcome::Failed)
                .await;
            return;
        }
        if received_bytes != expected_bytes {
            let message = format!(
                "Upload size mismatch: expected {expected_bytes} bytes, received {received_bytes} bytes"
            );
            warn!("[FileTransferDispatcher] {connection_id}: {message}");
            let _ = tokio::fs::remove_file(file_path).await;
            let _ = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: transfer_id.clone(),
                        message,
                    }),
                )
                .await;
            self.finish_activity(&connection_id, &transfer_id, FileTransferOutcome::Failed)
                .await;
            return;
        }
        if let Err(error) = self
            .emit_text(
                &connection_id,
                FileTransferMessage::TransferComplete(TransferComplete {
                    transfer_id: transfer_id.clone(),
                }),
            )
            .await
        {
            warn!("[FileTransferDispatcher] upload {transfer_id} complete ack drop: {error}");
            self.finish_activity(&connection_id, &transfer_id, FileTransferOutcome::Failed)
                .await;
            return;
        }
        self.finish_activity(&connection_id, &transfer_id, FileTransferOutcome::Completed)
            .await;
        info!(
            "[FileTransferDispatcher] upload {} completed successfully",
            transfer_id
        );
    }

    async fn accept_upload(
        &self,
        connection_id: String,
        req: UploadRequest,
    ) -> std::io::Result<()> {
        let target_dir = PathBuf::from(&req.target_dir);
        if !target_dir.exists() || !target_dir.is_dir() {
            if let Err(error) = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: req.transfer_id.clone(),
                        message: format!("Target directory not found: {}", req.target_dir),
                    }),
                )
                .await
            {
                warn!(
                    "[FileTransferDispatcher] {}: failed to emit TransferError for {}: {error}",
                    connection_id, req.transfer_id
                );
            }
            return Ok(());
        }
        let file_name = sanitized_file_name(&req.file_name);
        let file_path = target_dir.join(&file_name);
        if !self
            .start_activity(
                &connection_id,
                &req.transfer_id,
                FileTransferDirection::Upload,
                &file_name,
                req.file_size,
            )
            .await
        {
            self.emit_text(
                &connection_id,
                FileTransferMessage::TransferError(TransferError {
                    transfer_id: req.transfer_id.clone(),
                    message: "Transfer id is already active on this connection".to_string(),
                }),
            )
            .await
            .ok();
            return Ok(());
        }
        let file = match tokio::fs::File::create(&file_path).await {
            Ok(file) => file,
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
        let state = UploadState {
            file,
            file_path: file_path.clone(),
            total_chunks: req.total_chunks,
            received_chunks: 0,
            expected_bytes: req.file_size,
            received_bytes: 0,
            metrics: UploadWindow::default(),
        };
        let key = TransferKey::new(&connection_id, &req.transfer_id);
        let state_inserted = {
            let mut inner = self.inner.lock().await;
            if inner.active_connections.contains(&connection_id) && inner.activities.contains(&key)
            {
                inner.upload_states.insert(key.clone(), state);
                true
            } else {
                false
            }
        };
        if !state_inserted {
            let _ = tokio::fs::remove_file(&file_path).await;
            return Ok(());
        }
        if let Err(error) = self
            .emit_text(
                &connection_id,
                FileTransferMessage::UploadResponse(UploadResponse {
                    transfer_id: req.transfer_id.clone(),
                    accepted: true,
                    message: None,
                }),
            )
            .await
        {
            warn!(
                "[FileTransferDispatcher] {}: UploadResponse emit failed for {}: {error}",
                connection_id, req.transfer_id
            );
            let removed = {
                let mut inner = self.inner.lock().await;
                inner.upload_states.remove(&key)
            };
            if let Some(state) = removed {
                let _ = tokio::fs::remove_file(state.file_path).await;
            }
            self.finish_activity(
                &connection_id,
                &req.transfer_id,
                FileTransferOutcome::Failed,
            )
            .await;
            return Ok(());
        }
        info!(
            "[FileTransferDispatcher] upload {} started ({} bytes, {} chunks)",
            req.transfer_id, req.file_size, req.total_chunks
        );
        Ok(())
    }
}
