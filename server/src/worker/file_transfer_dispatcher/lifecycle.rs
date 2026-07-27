use super::*;

impl FileTransferDispatcher {
    pub fn new(
        file_sender: Arc<dyn EventSender<FileTransferPayload>>,
        policy: Arc<PolicyAccess>,
        hub: Arc<HostControlHub>,
        connection_ceilings: ConnectionCeilingStore,
        activity_sender: mpsc::UnboundedSender<WorkerToService>,
    ) -> Self {
        Self {
            inner: Arc::new(TokioMutex::new(DispatcherInner::new())),
            file_sender,
            policy,
            hub,
            connection_ceilings,
            activity_sender,
        }
    }

    pub(super) async fn start_activity(
        &self,
        connection_id: &str,
        transfer_id: &str,
        direction: FileTransferDirection,
        file_name: &str,
        total_bytes: u64,
    ) -> bool {
        let key = TransferKey::new(connection_id, transfer_id);
        let inserted = {
            let mut inner = self.inner.lock().await;
            inner.activities.insert(key)
        };
        if !inserted {
            return false;
        }
        let _ = self
            .activity_sender
            .send(WorkerToService::FileTransferStarted(
                FileTransferStartedPayload {
                    connection_id: connection_id.to_string(),
                    transfer_id: transfer_id.to_string(),
                    direction,
                    file_name: sanitized_file_name(file_name),
                    total_bytes,
                },
            ));
        true
    }

    pub(super) async fn finish_activity(
        &self,
        connection_id: &str,
        transfer_id: &str,
        outcome: FileTransferOutcome,
    ) -> bool {
        let key = TransferKey::new(connection_id, transfer_id);
        let removed = {
            let mut inner = self.inner.lock().await;
            inner.activities.remove(&key)
        };
        if !removed {
            return false;
        }
        let _ = self
            .activity_sender
            .send(WorkerToService::FileTransferFinished(
                FileTransferFinishedPayload {
                    connection_id: connection_id.to_string(),
                    transfer_id: transfer_id.to_string(),
                    outcome,
                },
            ));
        true
    }

    /// Resolve the `allow_file_transfer` decision for a connection.
    ///
    /// A cached answer is reused only while the capability it was decided under
    /// is still the one in force; an operator changing `allow_file_transfer`
    /// makes every cached answer a miss without anything having to reach in and
    /// clear this map.
    ///
    /// Two concurrent commands on a fresh connection can both miss the cache.
    /// They share one dialog — the hub keys in-flight prompts by connection and
    /// capability — so the user answers once and both callers receive it.
    async fn permission_for(&self, connection_id: &str) -> bool {
        let capability = SecurityPermissionType::FileTransfer;
        {
            let generation = self.policy.capability(capability).generation;
            let inner = self.inner.lock().await;
            if let Some(cached) = inner.permission_cache.get(connection_id)
                && cached.is_current(generation)
            {
                return cached.approved;
            }
        }
        // Meet the connection's grant ceiling with the global so a redeemed-grant
        // session is capped; owner connections carry no ceiling. The global and
        // its stamp are read together, after the ceiling, so the answer is
        // decided and filed under the same policy.
        let ceiling = self.connection_ceilings.get(connection_id).await;
        let state = self.policy.capability(capability);
        let allow_transfer = crate::model::security_approval::effective_permission(
            ceiling.as_ref(),
            state.permission,
            |c| c.allow_file_transfer,
        );
        let resolved = resolve_permission(
            &self.policy,
            &self.hub,
            allow_transfer,
            state.generation,
            capability,
            Some(connection_id.to_string()),
            // Capped grant / code-session: honor the prompt but never persist it to
            // the owner's global allow_file_transfer.
            ceiling.is_some(),
        )
        .await;
        if let Some(decided_at) = resolved.cacheable_at {
            let mut inner = self.inner.lock().await;
            // The connection can end while the prompt is up, and `stop_connection`
            // clears this cache. Writing the answer afterwards would leave an
            // entry behind for a connection that is already gone.
            if inner.active_connections.contains(connection_id) {
                inner.permission_cache.insert(
                    connection_id.to_string(),
                    CachedDecision {
                        approved: resolved.approved,
                        decided_at,
                    },
                );
            }
        }
        resolved.approved
    }

    /// Add a connection to the active set. Subsequent file-lane
    /// commands for this `connection_id` will be processed; commands
    /// for inactive connections are dropped.
    pub async fn start_connection(&self, payload: &StartMediaPayload) {
        let mut inner = self.inner.lock().await;
        let inserted = inner
            .active_connections
            .insert(payload.connection_id.clone());
        if !inserted {
            debug!(
                "[FileTransferDispatcher] {}: duplicate StartMedia",
                payload.connection_id
            );
        }
        info!(
            "[FileTransferDispatcher] {}: subscribed (active_count={})",
            payload.connection_id,
            inner.active_connections.len()
        );
    }

    /// Remove a connection and terminate every transfer it owns.
    pub async fn stop_connection(&self, payload: &StopMediaPayload) {
        let (paths, finished) = {
            let mut inner = self.inner.lock().await;
            inner.active_connections.remove(&payload.connection_id);
            inner.permission_cache.remove(&payload.connection_id);
            let transfer_keys: Vec<TransferKey> = inner
                .activities
                .iter()
                .filter(|key| key.connection_id == payload.connection_id)
                .cloned()
                .collect();
            let mut paths = Vec::new();
            let mut finished = Vec::new();
            for key in transfer_keys {
                if let Some(state) = inner.upload_states.remove(&key) {
                    paths.push(state.file_path);
                } else {
                    inner.cancel_download(&key);
                }
                if inner.activities.remove(&key) {
                    finished.push(FileTransferFinishedPayload {
                        connection_id: key.connection_id,
                        transfer_id: key.transfer_id,
                        outcome: FileTransferOutcome::Cancelled,
                    });
                }
            }
            (paths, finished)
        };
        for path in paths {
            if let Err(error) = tokio::fs::remove_file(&path).await {
                debug!(
                    "[FileTransferDispatcher] failed to remove partial upload {}: {error}",
                    path.display()
                );
            }
        }
        for event in finished {
            let _ = self
                .activity_sender
                .send(WorkerToService::FileTransferFinished(event));
        }
    }

    /// Drop every connection and terminate every in-flight transfer.
    pub async fn shutdown(&self) {
        let (paths, finished) = {
            let mut inner = self.inner.lock().await;
            let paths = inner
                .upload_states
                .drain()
                .map(|(_, state)| state.file_path)
                .collect::<Vec<_>>();
            let finished = inner
                .activities
                .drain()
                .map(|key| FileTransferFinishedPayload {
                    connection_id: key.connection_id,
                    transfer_id: key.transfer_id,
                    outcome: FileTransferOutcome::Cancelled,
                })
                .collect::<Vec<_>>();
            inner.active_connections.clear();
            inner.live_downloads.clear();
            inner.cancelled_transfers.clear();
            inner.permission_cache.clear();
            (paths, finished)
        };
        for path in paths {
            let _ = tokio::fs::remove_file(path).await;
        }
        for event in finished {
            let _ = self
                .activity_sender
                .send(WorkerToService::FileTransferFinished(event));
        }
    }

    pub async fn active_transfer_count(&self) -> u32 {
        self.inner
            .lock()
            .await
            .activities
            .len()
            .min(u32::MAX as usize) as u32
    }

    /// Apply an incoming file-transfer command. The bytes are either
    /// a JSON `FileTransferMessage` (control frame, `is_text=true`)
    /// or a binary chunk header + payload (`is_text=false`).
    pub async fn handle_command(&self, payload: FileTransferPayload) {
        // Liveness gate: ignore commands for connections we have not
        // been told about. The daemon DC router can race a stop, and
        // we do not want a stray packet to allocate state on a
        // closed-out connection.
        {
            let inner = self.inner.lock().await;
            if !inner.active_connections.contains(&payload.connection_id) {
                debug!(
                    "[FileTransferDispatcher] {}: command for inactive connection — dropping",
                    payload.connection_id
                );
                return;
            }
        }
        // Permission gate: file transfer is on its own access category
        // (`allow_file_transfer`), independent of `accept_control` which
        // governs mouse/keyboard. The daemon's DC router used to gate
        // `file_transfer_event` on `accept_control`; that was wrong —
        // the file management UI in the browser opens a fresh WebRTC
        // connection that has never requested control, so every
        // download/upload would be silently dropped at the daemon.
        // The daemon now passes file_transfer through unconditionally
        // and we do the actual permission check here.
        if !self.permission_for(&payload.connection_id).await {
            // A denied command still has to be answered. The browser has
            // already put the transfer on screen and has no other way to learn
            // it was refused, so dropping the command here is what pinned the
            // progress bar at 0% until the tab was closed.
            match inbound_transfer_id(&payload) {
                Some(transfer_id) => {
                    warn!(
                        "[FileTransferDispatcher] {}: permission denied — refusing transfer {}",
                        payload.connection_id, transfer_id
                    );
                    if let Err(e) = self
                        .emit_text(
                            &payload.connection_id,
                            FileTransferMessage::TransferError(TransferError {
                                transfer_id: transfer_id.clone(),
                                error_code: DeskErrorCode::PERMISSION_ERROR,
                                message: "File transfer is not permitted on this connection"
                                    .to_string(),
                            }),
                        )
                        .await
                    {
                        warn!(
                            "[FileTransferDispatcher] {}: failed to refuse transfer {}: {e}",
                            payload.connection_id, transfer_id
                        );
                    }
                }
                None => {
                    self.reject_unattributable_frame(
                        &payload.connection_id,
                        "permission denied and the frame carries no transfer id",
                    )
                    .await;
                }
            }
            return;
        }
        if payload.is_text {
            self.handle_text(payload).await;
        } else {
            self.handle_binary(payload).await;
        }
    }

    /// React to a daemon-side `dc.send` failure
    /// ([`ServiceToWorker::FileTransferSendFailed`]). The daemon has
    /// already logged the wire error at an appropriate severity; here
    /// we tear down the matching transfer state and tell the browser
    /// what happened. The browser listens for `TransferError` and
    /// surfaces it as a toast / cancels its progress bar.
    ///
    /// Abort scope:
    ///
    /// - `Some(transfer_id)` — abort just that transfer. Downloads
    ///   stop emitting chunks on the next loop iteration via
    ///   `cancelled_transfers`; uploads release any in-flight file
    ///   handle and remove the partial file from disk so it doesn't
    ///   orphan.
    /// - `None` — fall back to aborting every in-flight upload + every
    ///   download for `connection_id`. Used when the daemon could not
    ///   attribute the failure to a specific transfer (legacy payload
    ///   without `transfer_id`).
    ///
    /// The browser-facing `TransferError` message intentionally
    /// includes the daemon's `kind` + `error` string so a user-visible
    /// toast can distinguish "PacketTooLarge — please update the
    /// server" from "TransportClosed — connection dropped". The
    /// `chunk_index` (when present) goes into the message body for
    /// easier log correlation against the worker's `ft-metrics` line.
    pub async fn handle_send_failed(&self, payload: FileTransferSendFailedPayload) {
        let FileTransferSendFailedPayload {
            connection_id,
            transfer_id,
            chunk_index,
            kind,
            error,
        } = payload;
        let kind_label = match kind {
            FileTransferSendErrorKind::PacketTooLarge => "PacketTooLarge",
            FileTransferSendErrorKind::TransportClosed => "TransportClosed",
            FileTransferSendErrorKind::Other => "Other",
        };
        let message = match chunk_index {
            Some(idx) => format!("daemon dc.send failed [{kind_label}] at chunk {idx}: {error}"),
            None => format!("daemon dc.send failed [{kind_label}]: {error}"),
        };
        // Collect every transfer_id we need to abort. With a specific
        // transfer_id this is just `[id]`; with None we snapshot every
        // active upload + every active download for this connection.
        let aborted_keys = match transfer_id.as_deref() {
            Some(tid) => vec![TransferKey::new(&connection_id, tid)],
            None => self.active_keys_for(&connection_id).await,
        };
        warn!(
            "[FileTransferDispatcher] {}: send failure [{kind_label}] — aborting \
             {} transfer(s); {}",
            connection_id,
            aborted_keys.len(),
            error
        );
        self.abort_transfers(
            &connection_id,
            aborted_keys,
            DeskErrorCode::SYSTEM_ERROR,
            &message,
        )
        .await;
    }

    /// Tell the browser one transfer has ended, and why.
    ///
    /// The browser keeps a transfer on screen until it hears an ending. A host
    /// that abandons one without saying so leaves it there until the watchdog
    /// gives up, which turns a precise filesystem error into a generic timeout.
    /// Emitting can itself fail when the lane is what broke — nothing more can
    /// be done about that, and the browser's own transport timeout covers it.
    pub(super) async fn fail_transfer(
        &self,
        connection_id: &str,
        transfer_id: &str,
        error_code: DeskErrorCode,
        message: String,
    ) {
        if let Err(e) = self
            .emit_text(
                connection_id,
                FileTransferMessage::TransferError(TransferError {
                    transfer_id: transfer_id.to_string(),
                    error_code,
                    message,
                }),
            )
            .await
        {
            debug!(
                "[FileTransferDispatcher] {connection_id}: could not report the end of \
                 {transfer_id}: {e}"
            );
        }
    }

    /// Every transfer currently in flight on a connection.
    pub(super) async fn active_keys_for(&self, connection_id: &str) -> Vec<TransferKey> {
        self.inner
            .lock()
            .await
            .activities
            .iter()
            .filter(|key| key.connection_id == connection_id)
            .cloned()
            .collect()
    }

    /// Tear down a set of transfers and tell the browser why.
    ///
    /// Shared by the daemon send-failure path and the protocol-error path,
    /// which need the same three steps: stop the download loops, release and
    /// delete partial uploads, then emit one `TransferError` per transfer. The
    /// last step is what lets the browser settle — a transfer abandoned without
    /// a reply stays on its progress bar forever.
    pub(super) async fn abort_transfers(
        &self,
        connection_id: &str,
        keys: Vec<TransferKey>,
        error_code: DeskErrorCode,
        message: &str,
    ) {
        {
            let mut inner = self.inner.lock().await;
            for key in &keys {
                inner.cancel_download(key);
            }
        }
        // For uploads: release the file handle and remove the partial
        // file so it doesn't orphan on disk. Mirrors the cleanup the
        // TransferCancel arm already does.
        let mut upload_paths_to_remove: Vec<std::path::PathBuf> = Vec::new();
        {
            let mut inner = self.inner.lock().await;
            for key in &keys {
                if let Some(state) = inner.upload_states.remove(key) {
                    upload_paths_to_remove.push(state.file_path.clone());
                }
            }
        }
        for path in upload_paths_to_remove {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                debug!(
                    "[FileTransferDispatcher] failed to remove partial upload {} after \
                     abort: {e}",
                    path.display()
                );
            }
        }
        // Surface the failure back to the browser for each transfer.
        // If the file lane itself is what's broken these emits may
        // fail too — that's expected; the browser's SCTP timeout will
        // eventually surface the disconnect on its own.
        for key in &keys {
            if let Err(e) = self
                .emit_text(
                    connection_id,
                    FileTransferMessage::TransferError(TransferError {
                        transfer_id: key.transfer_id.clone(),
                        error_code,
                        message: message.to_string(),
                    }),
                )
                .await
            {
                debug!(
                    "[FileTransferDispatcher] {}: emit TransferError for {} after abort \
                     also failed: {e}",
                    connection_id, key.transfer_id
                );
            }
        }
        for key in &keys {
            self.finish_activity(
                &key.connection_id,
                &key.transfer_id,
                FileTransferOutcome::Failed,
            )
            .await;
        }
    }

    /// Answer a frame whose `transfer_id` could not be recovered.
    ///
    /// Malformed JSON or a truncated binary header cannot be replied to
    /// individually — there is no id to put in the reply. Dropping it silently
    /// is what strands the sender, so every transfer the connection has open is
    /// failed instead: those ids the browser *can* match, and a peer sending
    /// unparseable frames has already lost the guarantee that the rest of its
    /// stream will arrive intact.
    pub(super) async fn reject_unattributable_frame(&self, connection_id: &str, reason: &str) {
        let keys = self.active_keys_for(connection_id).await;
        warn!(
            "[FileTransferDispatcher] {}: unattributable file-transfer frame ({reason}) — \
             failing {} in-flight transfer(s)",
            connection_id,
            keys.len(),
        );
        if keys.is_empty() {
            return;
        }
        self.abort_transfers(
            connection_id,
            keys,
            DeskErrorCode::INVALID_PARAMS,
            &format!("File transfer protocol error: {reason}"),
        )
        .await;
    }
}
