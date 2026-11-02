use super::*;
use std::collections::VecDeque;
use tokio::sync::Notify;

struct PendingRequest {
    request: TransferRequest,
    cancelled: Arc<AtomicBool>,
}

pub(super) enum TransferRequest {
    Upload(UploadRequest),
    Download(DownloadRequest),
}

impl TransferRequest {
    fn id(&self) -> &str {
        match self {
            Self::Upload(request) => &request.transfer_id,
            Self::Download(request) => &request.transfer_id,
        }
    }
}

pub(super) struct ApprovalQueue {
    requests: VecDeque<PendingRequest>,
    pub(super) changed: Arc<Notify>,
}

impl FileTransferDispatcher {
    /// One owner per connection keeps request order and outlives any one consumer.
    pub(super) async fn queue_request(&self, connection: String, request: TransferRequest) {
        let id = request.id().to_owned();
        let key = TransferKey::new(&connection, &id);
        let start = {
            let mut inner = self.inner.lock().await;
            if inner.pending_requests.contains_key(&key)
                || inner.activities.contains(&key)
                || inner
                    .approval_queues
                    .get(&connection)
                    .is_some_and(|queue| queue.requests.len() >= 64)
            {
                drop(inner);
                self.fail_transfer(
                    &connection,
                    &id,
                    DeskErrorCode::INVALID_STATE,
                    "Transfer is already active or the pending queue is full".into(),
                )
                .await;
                return;
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            inner.pending_requests.insert(key, cancelled.clone());
            let start = !inner.approval_queues.contains_key(&connection);
            let queue = inner
                .approval_queues
                .entry(connection.clone())
                .or_insert_with(|| ApprovalQueue {
                    requests: VecDeque::new(),
                    changed: Arc::new(Notify::new()),
                });
            queue
                .requests
                .push_back(PendingRequest { request, cancelled });
            queue.changed.notify_one();
            start
        };
        if start {
            let dispatcher = self.clone();
            tokio::spawn(async move {
                dispatcher.drain_requests(connection).await;
            });
        }
    }

    async fn pending_upload_states(&self, connection: &str, phase: UploadPhase) {
        let ids = {
            let inner = self.inner.lock().await;
            inner
                .approval_queues
                .get(connection)
                .map(|queue| {
                    queue
                        .requests
                        .iter()
                        .filter_map(|request| {
                            if request.cancelled.load(Ordering::SeqCst) {
                                return None;
                            }
                            match &request.request {
                                TransferRequest::Upload(req) => Some(req.transfer_id.clone()),
                                _ => None,
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        for id in ids {
            let _ = self
                .emit_text(
                    connection,
                    FileTransferMessage::UploadStateChanged(UploadStateChanged {
                        transfer_id: id,
                        phase,
                    }),
                )
                .await;
        }
    }

    async fn drain_requests(&self, connection: String) {
        let changed = self.inner.lock().await.approval_queues[&connection]
            .changed
            .clone();
        let mut decision = Box::pin(self.permission_for(&connection, true));
        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        let mut waited = false;
        let approved = loop {
            tokio::select! {
                biased;
                approved = &mut decision => break approved,
                _ = changed.notified() => {},
                _ = heartbeat.tick() => {},
            }
            let mut inner = self.inner.lock().await;
            let live = inner.active_connections.contains(&connection)
                && inner.approval_queues[&connection]
                    .requests
                    .iter()
                    .any(|req| !req.cancelled.load(Ordering::SeqCst));
            if !live {
                // Keep the queue slot until the old shared-prompt owner is dropped.
                self.hub
                    .cancel_pending_capability(&connection, SecurityPermissionType::FileTransfer);
                drop(decision);
                inner.approval_queues.remove(&connection);
                inner
                    .pending_requests
                    .retain(|key, _| key.connection_id != connection);
                return;
            }
            drop(inner);
            waited = true;
            self.pending_upload_states(&connection, UploadPhase::AwaitingApproval)
                .await;
        };
        if approved && waited {
            self.pending_upload_states(&connection, UploadPhase::Preparing)
                .await;
        }
        loop {
            let request = {
                let mut inner = self.inner.lock().await;
                let next = inner
                    .approval_queues
                    .get_mut(&connection)
                    .and_then(|queue| queue.requests.pop_front());
                if next.is_none() {
                    inner.approval_queues.remove(&connection);
                }
                next
            };
            let Some(request) = request else { return };
            let id = request.request.id().to_owned();
            let key = TransferKey::new(&connection, &id);
            let token = request.cancelled.clone();
            if !request.cancelled.load(Ordering::SeqCst) {
                if !approved {
                    self.fail_transfer(
                        &connection,
                        &id,
                        DeskErrorCode::PERMISSION_ERROR,
                        "File transfer is not permitted on this connection".into(),
                    )
                    .await;
                } else {
                    match request.request {
                        TransferRequest::Upload(req) => {
                            let _ = self
                                .accept_upload(connection.clone(), req, Some(request.cancelled))
                                .await;
                        }
                        TransferRequest::Download(req) => {
                            self.start_download(connection.clone(), req, Some(request.cancelled))
                                .await
                        }
                    }
                }
            }
            let mut inner = self.inner.lock().await;
            if inner
                .pending_requests
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &token))
            {
                inner.pending_requests.remove(&key);
            }
        }
    }
}
