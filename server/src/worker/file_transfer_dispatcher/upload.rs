use super::*;

/// Why a fully received upload could not be put in place.
struct CommitFailure {
    code: DeskErrorCode,
    message: String,
    preserve_staging: bool,
}

impl FileTransferDispatcher {
    async fn downloads_directory(&self) -> std::io::Result<PathBuf> {
        #[cfg(test)]
        if let Some(directory) = &self.download_directory {
            return Ok(directory.clone());
        }
        tokio::task::spawn_blocking(super::download_directory::current)
            .await
            .map_err(std::io::Error::other)?
    }
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
                self.start_download(payload.connection_id, req, None).await;
            }
            FileTransferMessage::UploadRequest(req) => {
                if let Err(e) = self
                    .accept_upload(payload.connection_id.clone(), req, None)
                    .await
                {
                    error!("[FileTransferDispatcher] upload accept error: {e}");
                }
            }
            FileTransferMessage::TransferComplete(complete) => {
                let key = TransferKey::new(&payload.connection_id, &complete.transfer_id);
                let upload = self.inner.lock().await.upload_states.remove(&key);
                if let Some(upload) = upload {
                    let mut state = upload.state.lock().await;
                    self.finish_upload(
                        &payload.connection_id,
                        &complete.transfer_id,
                        &upload,
                        &mut state,
                    )
                    .await;
                }
            }
            FileTransferMessage::TransferCancel(cancel) => {
                info!(
                    "[FileTransferDispatcher] {}: cancel transfer_id={}",
                    payload.connection_id, cancel.transfer_id
                );
                let key = TransferKey::new(&payload.connection_id, &cancel.transfer_id);
                let (known, removed_upload) = {
                    let mut inner = self.inner.lock().await;
                    let known = inner.upload_states.contains_key(&key)
                        || inner.pending_requests.contains_key(&key)
                        || inner.live_downloads.contains(&key);
                    inner.cancel_download(&key);
                    (known, inner.take_upload(&key))
                };
                if !known {
                    return;
                }
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
            self.fail_transfer(
                &connection_id,
                &transfer_id,
                DeskErrorCode::FILE_SAVE_UNCONFIRMED,
                "Upload state is no longer available; verify the remote folder before retrying"
                    .into(),
            )
            .await;
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
        }
        if let Some(line) = state.metrics.flush_line(&transfer_id, "ft-metrics") {
            info!("{line}");
        }
        self.finish_upload(&connection_id, &transfer_id, &upload, &mut state)
            .await;
    }

    async fn finish_upload(
        &self,
        connection_id: &str,
        transfer_id: &str,
        upload: &Upload,
        state: &mut tokio::sync::MutexGuard<'_, UploadState>,
    ) {
        let outcome = if state.received_chunks == state.total_chunks
            && state.received_bytes == state.expected_bytes
        {
            self.commit_upload(upload, state).await
        } else {
            Err(CommitFailure {
                code: DeskErrorCode::INVALID_STATE,
                message: format!(
                    "Upload size mismatch: expected {} bytes in {} chunks, received {} bytes in {} chunks",
                    state.expected_bytes,
                    state.total_chunks,
                    state.received_bytes,
                    state.received_chunks
                ),
                preserve_staging: false,
            })
        };
        self.inner
            .lock()
            .await
            .upload_destinations
            .remove(&upload.destination);
        match outcome {
            Ok(destination) => {
                let _ = self
                    .emit_text(
                        connection_id,
                        FileTransferMessage::TransferComplete(TransferComplete {
                            transfer_id: transfer_id.to_owned(),
                            resolved_file_name: destination
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned()),
                            resolved_target_dir: destination
                                .parent()
                                .map(|dir| dir.to_string_lossy().into_owned()),
                        }),
                    )
                    .await;
                // A failed reply cannot undo a successful publication.
                self.finish_activity(connection_id, transfer_id, FileTransferOutcome::Completed)
                    .await;
            }
            Err(failure) => {
                if !failure.preserve_staging {
                    Self::discard_partial(upload, state).await;
                }
                self.fail_transfer(connection_id, transfer_id, failure.code, failure.message)
                    .await;
                self.finish_activity(connection_id, transfer_id, FileTransferOutcome::Failed)
                    .await;
            }
        }
    }

    /// Sync the complete stream, then publish it while retaining the destination claim.
    async fn commit_upload(
        &self,
        upload: &Upload,
        state: &mut tokio::sync::MutexGuard<'_, UploadState>,
    ) -> Result<PathBuf, CommitFailure> {
        let before_publish = |what: &str, error: std::io::Error| CommitFailure {
            code: DeskErrorCode::SYSTEM_ERROR,
            message: format!("Failed to {what} upload: {error}"),
            preserve_staging: false,
        };
        state
            .file
            .flush()
            .await
            .map_err(|e| before_publish("flush", e))?;
        state
            .file
            .sync()
            .await
            .map_err(|e| before_publish("store", e))?;
        state.file = Box::new(tokio::io::sink());
        let parent = upload.destination.parent().expect("upload has a directory");
        for suffix in 0..10_000 {
            let destination = if suffix == 0 {
                upload.destination.clone()
            } else {
                parent.join(upload_name(&upload.source_name, suffix))
            };
            if suffix > 0 && destination == upload.destination {
                continue;
            }
            let extra_claim = destination != upload.destination;
            if extra_claim
                && !self
                    .inner
                    .lock()
                    .await
                    .upload_destinations
                    .insert(destination.clone())
            {
                continue;
            }
            let staging = upload.staging.clone();
            let target = destination.clone();
            let no_replace = upload.no_replace;
            let result = tokio::task::spawn_blocking(move || {
                if no_replace {
                    crate::durable_file::durable_publish_new(&staging, &target)
                } else {
                    crate::durable_file::durable_replace(&staging, &target)
                        .map_err(crate::durable_file::PublishError::Uncertain)
                }
            })
            .await
            .unwrap_or_else(|error| {
                Err(crate::durable_file::PublishError::Uncertain(
                    std::io::Error::other(error),
                ))
            });
            if extra_claim {
                self.inner
                    .lock()
                    .await
                    .upload_destinations
                    .remove(&destination);
            }
            match result {
                Ok(()) => return Ok(destination),
                Err(error) if no_replace && error.is_collision() => continue,
                Err(error) => {
                    return Err(CommitFailure {
                        code: if error.is_uncertain() {
                            DeskErrorCode::FILE_SAVE_UNCONFIRMED
                        } else {
                            DeskErrorCode::SYSTEM_ERROR
                        },
                        message: format!("Failed to publish upload: {error}"),
                        preserve_staging: error.is_uncertain(),
                    });
                }
            }
        }
        Err(CommitFailure {
            code: DeskErrorCode::SYSTEM_ERROR,
            message: "No available upload filename".into(),
            preserve_staging: false,
        })
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

    pub(super) async fn accept_upload(
        &self,
        connection_id: String,
        req: UploadRequest,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> std::io::Result<()> {
        let cancelled = || {
            cancellation
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::SeqCst))
        };
        if cancelled() {
            return Ok(());
        }
        let key = TransferKey::new(&connection_id, &req.transfer_id);
        let file_name = sanitized_file_name(&req.file_name);
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
            self.fail_transfer(
                &connection_id,
                &req.transfer_id,
                DeskErrorCode::INVALID_STATE,
                "Transfer id is already active on this connection".into(),
            )
            .await;
            return Ok(());
        }
        let prepared = self
            .prepare_upload(&key, &req, &file_name, cancellation.as_deref())
            .await;
        match prepared {
            Ok(Some(upload)) => {
                let destination = &upload.destination;
                let directory = destination.parent().expect("upload has a directory");
                if cancelled() || upload.is_cancelled() {
                    let mut state = upload.state.lock().await;
                    Self::discard_partial(&upload, &mut state).await;
                    return Ok(());
                }
                if self
                    .emit_text(
                        &connection_id,
                        FileTransferMessage::UploadResponse(UploadResponse {
                            transfer_id: req.transfer_id.clone(),
                            accepted: true,
                            message: None,
                            resolved_target_dir: directory.to_string_lossy().into_owned(),
                            resolved_file_name: destination
                                .file_name()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                        }),
                    )
                    .await
                    .is_ok()
                {
                    if upload.is_cancelled() {
                        let mut state = upload.state.lock().await;
                        Self::discard_partial(&upload, &mut state).await;
                    }
                    return Ok(());
                }
                let taken = self.inner.lock().await.take_upload(&key).is_some();
                if upload.is_cancelled() {
                    let mut state = upload.state.lock().await;
                    Self::discard_partial(&upload, &mut state).await;
                }
                if !taken {
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.fail_transfer(
                    &connection_id,
                    &req.transfer_id,
                    match error.kind() {
                        std::io::ErrorKind::NotFound => DeskErrorCode::FILE_PATH_NOT_FOUND,
                        std::io::ErrorKind::AlreadyExists => DeskErrorCode::INVALID_STATE,
                        _ => DeskErrorCode::SYSTEM_ERROR,
                    },
                    format!("Could not prepare upload: {error}"),
                )
                .await;
            }
        }
        self.finish_activity(
            &connection_id,
            &req.transfer_id,
            FileTransferOutcome::Failed,
        )
        .await;
        Ok(())
    }
    async fn prepare_upload(
        &self,
        key: &TransferKey,
        req: &UploadRequest,
        file_name: &str,
        cancellation: Option<&AtomicBool>,
    ) -> std::io::Result<Option<Arc<Upload>>> {
        let cancelled = || cancellation.is_some_and(|flag| flag.load(Ordering::SeqCst));
        let no_replace = req.target_location == Some(UploadLocation::Downloads);
        let directory = match (&req.target_dir, req.target_location) {
            (None, Some(UploadLocation::Downloads)) => self.downloads_directory().await?,
            (Some(path), None) if !path.is_empty() => PathBuf::from(path),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "exactly one upload target is required",
                ));
            }
        };
        let directory = tokio::fs::canonicalize(directory).await?;
        if !tokio::fs::metadata(&directory).await?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "upload target is not a directory",
            ));
        }
        let mut destination = None;
        for suffix in 0..if no_replace { 10_000 } else { 1 } {
            let path = directory.join(upload_name(file_name, suffix));
            let occupied = match tokio::fs::symlink_metadata(&path).await {
                Ok(metadata) => {
                    if !no_replace && metadata.is_dir() {
                        return Err(std::io::Error::other("upload destination is a directory"));
                    }
                    true
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            if no_replace && occupied {
                continue;
            }
            let mut inner = self.inner.lock().await;
            if cancelled()
                || !inner.active_connections.contains(&key.connection_id)
                || !inner.activities.contains(key)
            {
                return Ok(None);
            }
            if inner.upload_destinations.insert(path.clone()) {
                destination = Some(path);
                break;
            }
        }
        let destination = destination.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "Another upload is already writing this destination",
            )
        })?;
        let staging = directory.join(format!(".{file_name}.{}.part", uuid::Uuid::new_v4()));
        let file = match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .await
        {
            Ok(file) => file,
            Err(error) => {
                self.inner
                    .lock()
                    .await
                    .upload_destinations
                    .remove(&destination);
                return Err(error);
            }
        };
        let upload = Arc::new(Upload {
            cancelled: AtomicBool::new(false),
            destination: destination.clone(),
            staging: staging.clone(),
            no_replace,
            source_name: file_name.to_owned(),
            state: TokioMutex::new(UploadState {
                file: Box::new(file),
                total_chunks: req.total_chunks,
                received_chunks: 0,
                expected_bytes: req.file_size,
                received_bytes: 0,
                metrics: UploadWindow::default(),
            }),
        });
        {
            let mut inner = self.inner.lock().await;
            if cancelled()
                || !inner.active_connections.contains(&key.connection_id)
                || !inner.activities.contains(key)
            {
                inner.upload_destinations.remove(&destination);
                drop(inner);
                drop(upload);
                let _ = tokio::fs::remove_file(staging).await;
                return Ok(None);
            }
            inner.pending_requests.remove(key);
            inner.upload_states.insert(key.clone(), upload.clone());
        }
        Ok(Some(upload))
    }
}

fn upload_name(name: &str, suffix: usize) -> String {
    if suffix == 0 {
        return name.to_owned();
    }
    let path = std::path::Path::new(name);
    match (
        path.file_stem().and_then(|s| s.to_str()),
        path.extension().and_then(|s| s.to_str()),
    ) {
        (Some(stem), Some(extension)) => format!("{stem} ({suffix}).{extension}"),
        _ => format!("{name} ({suffix})"),
    }
}
