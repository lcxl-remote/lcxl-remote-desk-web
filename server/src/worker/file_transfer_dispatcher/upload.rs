use super::*;

/// Why a fully received upload could not be put in place.
struct CommitFailure {
    code: DeskErrorCode,
    message: String,
}

/// What became of an upload request once the dispatcher tried to register it.
enum Admission {
    Accepted,
    /// The connection ended, or its transfer was cancelled, while the staging
    /// file was being created.
    ConnectionGone,
    /// Another transfer is already writing the same destination.
    DestinationBusy,
}

impl FileTransferDispatcher {
    pub(super) async fn handle_text(&self, payload: FileTransferPayload) {
        let s = match std::str::from_utf8(&payload.data) {
            Ok(s) => s,
            Err(e) => {
                error!(
                    "[FileTransferDispatcher] {}: control payload not UTF-8: {e}",
                    payload.connection_id
                );
                self.reject_unattributable_frame(
                    &payload.connection_id,
                    "control frame is not valid UTF-8",
                )
                .await;
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
                self.reject_unattributable_frame(
                    &payload.connection_id,
                    "control frame is not a known message",
                )
                .await;
                return;
            }
        };
        match msg {
            FileTransferMessage::DownloadRequest(req) => {
                let dispatcher = self.clone();
                let connection_id = payload.connection_id.clone();
                let transfer_id = req.transfer_id.clone();
                let key = TransferKey::new(&connection_id, &transfer_id);
                // Registered before this handler returns, not inside the task
                // below. A `TransferCancel` can be the very next frame on this
                // lane, and it is only recorded for a download that is
                // registered — so registering later would let a cancel that
                // arrives while the task is still opening the file be dropped,
                // and the whole file would go out to a peer that asked for it to
                // stop. The task releases this on its way out, whichever way it
                // leaves, so nothing accumulates.
                self.inner.lock().await.live_downloads.insert(key.clone());
                tokio::spawn(async move {
                    let outcome = dispatcher.serve_download(connection_id.clone(), req).await;
                    {
                        let mut inner = dispatcher.inner.lock().await;
                        inner.live_downloads.remove(&key);
                        inner.cancelled_transfers.remove(&key);
                    }
                    if let Err(e) = outcome {
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
                let upload = {
                    let mut inner = self.inner.lock().await;
                    inner.upload_states.remove(&key)
                };
                if let Some(upload) = upload {
                    self.inner
                        .lock()
                        .await
                        .upload_destinations
                        .remove(&upload.destination);
                    // The state lock, not the shared one, is what the commit
                    // runs under: it waits on the same disk a chunk write does.
                    let mut state = upload.state.lock().await;
                    let received_chunks = state.received_chunks;
                    let arrived_intact = state.received_chunks == state.total_chunks
                        && state.received_bytes == state.expected_bytes;
                    let outcome = if arrived_intact {
                        Self::commit_upload(&upload, &mut state).await
                    } else {
                        // The stream itself did not arrive; nothing to put in
                        // place. Distinct from a filesystem problem, and the
                        // browser is told which it was.
                        Err(CommitFailure {
                            code: DeskErrorCode::INVALID_STATE,
                            message: format!(
                                "Upload size mismatch: expected {} bytes in {} chunks, received {} bytes in {} chunks",
                                state.expected_bytes,
                                state.total_chunks,
                                state.received_bytes,
                                state.received_chunks
                            ),
                        })
                    };
                    if outcome.is_err() {
                        Self::discard_partial(&upload, &mut state).await;
                    }
                    drop(state);
                    info!(
                        "[FileTransferDispatcher] upload {} completed, received {} chunks",
                        complete.transfer_id, received_chunks
                    );
                    match outcome {
                        Ok(()) => {
                            self.finish_activity(
                                &payload.connection_id,
                                &complete.transfer_id,
                                FileTransferOutcome::Completed,
                            )
                            .await;
                        }
                        Err(failure) => {
                            let _ = self
                                .emit_text(
                                    &payload.connection_id,
                                    FileTransferMessage::TransferError(TransferError {
                                        transfer_id: complete.transfer_id.clone(),
                                        error_code: failure.code,
                                        message: failure.message,
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
            }
            FileTransferMessage::TransferCancel(cancel) => {
                info!(
                    "[FileTransferDispatcher] {}: cancel transfer_id={}",
                    payload.connection_id, cancel.transfer_id
                );
                let key = TransferKey::new(&payload.connection_id, &cancel.transfer_id);
                let removed_upload = {
                    let mut inner = self.inner.lock().await;
                    inner.cancel_download(&key);
                    inner.take_upload(&key)
                };
                if let Some(path) = removed_upload {
                    // A write still in flight can be holding this open, and
                    // waiting for it is exactly what a cancel must not do. The
                    // writer removes it on the way out when that happens.
                    if let Err(e) = tokio::fs::remove_file(&path).await {
                        debug!(
                            "[FileTransferDispatcher] could not remove cancelled upload file {} \
                             yet: {e}",
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
                self.reject_unattributable_frame(
                    &payload.connection_id,
                    "binary chunk header is truncated",
                )
                .await;
                return;
            }
        };
        let transfer_id = transfer_id.to_string();
        let connection_id = payload.connection_id.clone();
        let key = TransferKey::new(&connection_id, &transfer_id);
        let chunk_data = chunk_data.to_vec();
        let chunk_len = chunk_data.len() as u64;
        let lock_start = Instant::now();
        // The shared lock is taken only to find the transfer, never to write to
        // it. Everything below runs under this one upload's own lock, so a slow
        // disk delays this transfer and nothing else — in particular not the
        // stop, revoke and cancel paths, which all need the shared lock to
        // withdraw a peer's access and cannot be made to wait on a device.
        let upload = {
            let inner = self.inner.lock().await;
            inner.upload_states.get(&key).cloned()
        };
        let Some(upload) = upload else {
            warn!(
                "[FileTransferDispatcher] {}: chunk for unknown transfer {}",
                connection_id, transfer_id
            );
            return;
        };
        let mut state = upload.state.lock().await;
        let lock_elapsed = lock_start.elapsed();
        if upload.is_cancelled() {
            // Called off while this chunk queued behind an earlier one. The
            // teardown has already answered the browser and removed the file.
            debug!(
                "[FileTransferDispatcher] {}: chunk {} arrived for cancelled transfer {}",
                connection_id, chunk_index, transfer_id
            );
            return;
        }
        let write_start = Instant::now();
        if let Err(error) = state.file.write_all(&chunk_data).await {
            error!(
                "[FileTransferDispatcher] {}: write chunk {} for {} failed: {error}",
                connection_id, chunk_index, transfer_id
            );
            // Out of reach first, so no later chunk can re-open the transfer,
            // then close and remove what was written.
            let _ = self.inner.lock().await.take_upload(&key);
            Self::discard_partial(&upload, &mut state).await;
            drop(state);
            self.fail_transfer(
                &connection_id,
                &transfer_id,
                DeskErrorCode::SYSTEM_ERROR,
                format!("Write failed at chunk {chunk_index}: {error}"),
            )
            .await;
            self.finish_activity(&connection_id, &transfer_id, FileTransferOutcome::Failed)
                .await;
            return;
        }
        if upload.is_cancelled() {
            // Called off while that write was in flight. Teardown did not wait
            // for it, so removing what it wrote falls to this task.
            debug!(
                "[FileTransferDispatcher] {}: transfer {} was cancelled mid-write",
                connection_id, transfer_id
            );
            Self::discard_partial(&upload, &mut state).await;
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
        // Last chunk. Claim the transfer so nothing else can reach it, then
        // finish it off. Failing to claim means a teardown got here first.
        {
            let mut inner = self.inner.lock().await;
            if inner.upload_states.remove(&key).is_none() {
                drop(inner);
                Self::discard_partial(&upload, &mut state).await;
                return;
            }
            inner.upload_destinations.remove(&upload.destination);
        }
        if let Some(line) = state.metrics.flush_line(&transfer_id, "ft-metrics") {
            info!("{line}");
        }
        let expected_bytes = state.expected_bytes;
        let received_bytes = state.received_bytes;
        let outcome = if received_bytes == expected_bytes {
            Self::commit_upload(&upload, &mut state).await
        } else {
            Err(CommitFailure {
                code: DeskErrorCode::INVALID_STATE,
                message: format!(
                    "Upload size mismatch: expected {expected_bytes} bytes, received \
                     {received_bytes} bytes"
                ),
            })
        };
        if let Err(failure) = outcome {
            warn!(
                "[FileTransferDispatcher] {connection_id}: upload {transfer_id} not put in \
                 place: {}",
                failure.message
            );
            Self::discard_partial(&upload, &mut state).await;
            drop(state);
            self.fail_transfer(
                &connection_id,
                &transfer_id,
                failure.code,
                failure.message.clone(),
            )
            .await;
            self.finish_activity(&connection_id, &transfer_id, FileTransferOutcome::Failed)
                .await;
            return;
        }
        drop(state);
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

    /// Put a fully received upload where the browser asked for it.
    ///
    /// The bytes reach stable storage, the handle is released, and only then
    /// does the staging file take the destination's name — in one step, so a
    /// reader sees either the file that was there before or the whole new one.
    /// Anything that goes wrong leaves the destination exactly as it was; the
    /// caller discards the staging file.
    async fn commit_upload(
        upload: &Upload,
        state: &mut tokio::sync::MutexGuard<'_, UploadState>,
    ) -> Result<(), CommitFailure> {
        let system_error = |what: &str, error: std::io::Error| CommitFailure {
            code: DeskErrorCode::SYSTEM_ERROR,
            message: format!("Failed to {what} upload: {error}"),
        };
        state
            .file
            .flush()
            .await
            .map_err(|error| system_error("flush", error))?;
        state
            .file
            .sync()
            .await
            .map_err(|error| system_error("store", error))?;
        // Released before the rename: a platform can refuse to replace a file
        // that is still open, and there is nothing left to write.
        state.file = Box::new(tokio::io::sink());
        let staging = upload.staging.clone();
        let destination = upload.destination.clone();
        tokio::task::spawn_blocking(move || {
            crate::durable_file::durable_replace(&staging, &destination)
        })
        .await
        .map_err(|error| CommitFailure {
            code: DeskErrorCode::SYSTEM_ERROR,
            message: format!("Failed to put upload in place: {error}"),
        })?
        .map_err(|error| system_error("put in place", error))
    }

    /// Close a partial upload and remove the staging file it wrote.
    ///
    /// The destination is never touched: an upload that ends here never
    /// arrived, so whatever the user already had is still the current file.
    ///
    /// Closing first is not cosmetic: a platform can refuse to remove a file
    /// that is still open, which is exactly the case teardown cannot handle on
    /// its own — it does not hold the state lock and so cannot close anything.
    /// Whoever holds the file is who removes it.
    async fn discard_partial(
        upload: &Upload,
        state: &mut tokio::sync::MutexGuard<'_, UploadState>,
    ) {
        state.file = Box::new(tokio::io::sink());
        if let Err(error) = tokio::fs::remove_file(&upload.staging).await {
            debug!(
                "[FileTransferDispatcher] could not remove staged upload {}: {error}",
                upload.staging.display()
            );
        }
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
                        error_code: DeskErrorCode::FILE_PATH_NOT_FOUND,
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
        // Resolve the directory so two spellings of it cannot each claim the
        // same destination. Falls back to what was asked for if the platform
        // cannot resolve it — the claim is then weaker, not absent.
        let resolved_dir = tokio::fs::canonicalize(&target_dir)
            .await
            .unwrap_or_else(|_| target_dir.clone());
        let destination = resolved_dir.join(&file_name);
        // A destination that cannot be replaced is worth saying now. The bytes
        // go to a staging file, so nothing here would fail until the rename at
        // the very end — by which time the browser has streamed the whole file
        // at a host that was never going to accept it.
        if destination.is_dir() {
            if let Err(error) = self
                .emit_text(
                    &connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: req.transfer_id.clone(),
                        error_code: DeskErrorCode::SYSTEM_ERROR,
                        message: format!("{} is a directory", destination.display()),
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
                    error_code: DeskErrorCode::INVALID_STATE,
                    message: "Transfer id is already active on this connection".to_string(),
                }),
            )
            .await
            .ok();
            return Ok(());
        }
        // The bytes go to a file of our own in the same directory, never to the
        // one the browser named. Unique, so two uploads cannot adopt each
        // other's partial file, and `create_new` so an existing file is never
        // the thing that gets opened.
        let staging = resolved_dir.join(format!(".{file_name}.{}.part", uuid::Uuid::new_v4()));
        let file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                // The browser is about to start sending chunks at a host that
                // has nowhere to put them. Say so now rather than let it stream
                // a whole file into a transfer that was never going to land.
                self.fail_transfer(
                    &connection_id,
                    &req.transfer_id,
                    DeskErrorCode::SYSTEM_ERROR,
                    format!("Could not create {}: {error}", staging.display()),
                )
                .await;
                self.finish_activity(
                    &connection_id,
                    &req.transfer_id,
                    FileTransferOutcome::Failed,
                )
                .await;
                return Err(error);
            }
        };
        let upload = Arc::new(Upload {
            cancelled: AtomicBool::new(false),
            destination: destination.clone(),
            staging: staging.clone(),
            state: TokioMutex::new(UploadState {
                file: Box::new(file),
                total_chunks: req.total_chunks,
                received_chunks: 0,
                expected_bytes: req.file_size,
                received_bytes: 0,
                metrics: UploadWindow::default(),
            }),
        });
        let key = TransferKey::new(&connection_id, &req.transfer_id);
        // Claiming the destination and registering the transfer are one step:
        // a claim taken without a transfer to release it would lock the file
        // out for the life of the worker.
        let admission = {
            let mut inner = self.inner.lock().await;
            if !inner.active_connections.contains(&connection_id)
                || !inner.activities.contains(&key)
            {
                Admission::ConnectionGone
            } else if !inner.upload_destinations.insert(destination.clone()) {
                Admission::DestinationBusy
            } else {
                inner.upload_states.insert(key.clone(), upload);
                Admission::Accepted
            }
        };
        match admission {
            Admission::Accepted => {}
            Admission::ConnectionGone => {
                let _ = tokio::fs::remove_file(&staging).await;
                return Ok(());
            }
            Admission::DestinationBusy => {
                let _ = tokio::fs::remove_file(&staging).await;
                self.fail_transfer(
                    &connection_id,
                    &req.transfer_id,
                    DeskErrorCode::INVALID_STATE,
                    format!(
                        "Another upload is already writing {}",
                        destination.display()
                    ),
                )
                .await;
                self.finish_activity(
                    &connection_id,
                    &req.transfer_id,
                    FileTransferOutcome::Failed,
                )
                .await;
                return Ok(());
            }
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
                inner.take_upload(&key)
            };
            if let Some(path) = removed {
                let _ = tokio::fs::remove_file(path).await;
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
