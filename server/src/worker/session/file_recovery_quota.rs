//! Blocking quota RPC for filesystem tasks. The worker event loop delivers replies.
use desk_ipc_protocol::message::*;
use std::{
    collections::HashMap,
    io,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

type Pending = Arc<Mutex<HashMap<String, mpsc::SyncSender<FileRecoveryQuotaOutcome>>>>;
#[derive(Clone)]
pub(crate) struct QuotaClient {
    sender: tokio::sync::mpsc::UnboundedSender<WorkerToService>,
    pending: Pending,
    os_user: Option<String>,
}
impl QuotaClient {
    pub(crate) fn new(sender: tokio::sync::mpsc::UnboundedSender<WorkerToService>) -> Self {
        let os_user = crate::file_recovery_service::platform_user::current().ok();
        Self {
            sender,
            pending: Arc::new(Mutex::new(HashMap::new())),
            os_user,
        }
    }
    /// Run only on a blocking filesystem task, never the worker event loop.
    pub(crate) fn request(
        &self,
        command: FileRecoveryQuotaCommand,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        self.request_with_timeout(command, Duration::from_secs(10))
    }
    fn request_with_timeout(
        &self,
        command: FileRecoveryQuotaCommand,
        timeout: Duration,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        Self::policy_response(self.raw_request_with_timeout(command, timeout)?)
    }
    fn raw_request_with_timeout(
        &self,
        command: FileRecoveryQuotaCommand,
        timeout: Duration,
    ) -> io::Result<FileRecoveryQuotaOutcome> {
        let os_user = self.os_user.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "device quota unavailable on this platform",
            )
        })?;
        let request_id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| io::Error::other("quota requests unavailable"))?;
            if pending.len() >= 32 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "device quota request capacity exhausted",
                ));
            }
            pending.insert(request_id.clone(), tx);
        }
        let sent = self
            .sender
            .send(WorkerToService::FileRecoveryQuotaRequested(
                FileRecoveryQuotaRequest {
                    request_id: request_id.clone(),
                    os_user,
                    command,
                },
            ));
        let result = if sent.is_ok() {
            rx.recv_timeout(timeout).map_err(|error| {
                io::Error::new(
                    match error {
                        mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                        mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::BrokenPipe,
                    },
                    "device quota response unavailable; file was not changed",
                )
            })
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "device quota transport unavailable; file was not changed",
            ))
        };
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&request_id);
        }
        result
    }
    fn policy_response(
        outcome: FileRecoveryQuotaOutcome,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        match outcome {
            FileRecoveryQuotaOutcome::Epoch { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected quota epoch reply",
            )),
            FileRecoveryQuotaOutcome::Applied {
                retention_days,
                max_bytes,
                used_bytes,
                reserved_bytes,
            } => {
                let policy = desk_file_recovery::Policy {
                    retention_days,
                    max_bytes,
                };
                policy.validate()?;
                Ok((policy, used_bytes, reserved_bytes))
            }
            FileRecoveryQuotaOutcome::CapacityExceeded => Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "device file backup capacity exhausted; file was not changed",
            )),
            FileRecoveryQuotaOutcome::IdentityChanged => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution user changed; file was not changed",
            )),
            FileRecoveryQuotaOutcome::InvalidRequest => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "device quota request rejected; file was not changed",
            )),
            FileRecoveryQuotaOutcome::StorageUnavailable => Err(io::Error::other(
                "device quota storage unavailable; file was not changed",
            )),
        }
    }
    pub(crate) fn policy(&self) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        self.request(FileRecoveryQuotaCommand::Read)
    }
    pub(crate) fn set_policy(
        &self,
        policy: desk_file_recovery::Policy,
    ) -> io::Result<(desk_file_recovery::Policy, u64, u64)> {
        policy.validate()?;
        self.request(FileRecoveryQuotaCommand::SetPolicy {
            retention_days: policy.retention_days,
            max_bytes: policy.max_bytes,
        })
    }
    pub(crate) fn reserve(&self, record: &desk_file_recovery::Record) -> io::Result<()> {
        self.request(FileRecoveryQuotaCommand::Reserve {
            identity: record_identity(record),
            bytes: record.bytes,
            execution_deadline_ms: (chrono::Utc::now().timestamp_millis().max(1) as u64)
                .saturating_add(30_000),
        })
        .map(|_| ())
    }
    pub(crate) fn settle(&self, record: &desk_file_recovery::Record) -> io::Result<()> {
        self.request(FileRecoveryQuotaCommand::Settle {
            identity: record_identity(record),
            bytes: record.bytes,
        })
        .map(|_| ())
    }
    pub(crate) fn reconcile_settlements(&self, vault: &mut desk_file_recovery::LockedVault) {
        for record in vault.pending_quota_settlement().into_iter().take(32) {
            let result = self
                .settle(&record)
                .and_then(|_| vault.acknowledge_quota_settlement(&record.scope, &record.id));
            if let Err(error) = result {
                tracing::warn!(error_kind = ?error.kind(), "File recovery quota settlement pending; retained for retry without replaying mutation");
                break;
            }
        }
    }
    pub(crate) fn release(&self, record: &desk_file_recovery::Record) -> io::Result<()> {
        self.request(FileRecoveryQuotaCommand::Release {
            identity: record_identity(record),
            retained_index_bytes: desk_file_recovery::LockedVault::retained_index_bytes(record)?,
        })
        .map(|_| ())
    }
    pub(crate) fn release_namespace(
        &self,
        namespace: &str,
        execution_epoch: u64,
    ) -> io::Result<()> {
        self.request(FileRecoveryQuotaCommand::ReleaseNamespace {
            namespace: namespace.into(),
            execution_epoch,
        })
        .map(|_| ())
    }
    pub(crate) fn complete(&self, reply: FileRecoveryQuotaReply) {
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(sender) = pending.remove(&reply.request_id) {
                let _ = sender.send(reply.outcome);
            }
        }
    }
}

fn record_identity(record: &desk_file_recovery::Record) -> FileRecoveryQuotaIdentity {
    FileRecoveryQuotaIdentity {
        execution_epoch: record.storage_epoch,
        authority: record.scope.authority.clone(),
        device: record.scope.device.clone(),
        owner: record.scope.owner.clone(),
        conversation: record.conversation.clone(),
        operation: record.operation.clone(),
        generation: record.generation.clone(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn failed_settlement_survives_restart_and_success_is_not_repeated() {
        let root = tempfile::tempdir().unwrap();
        let scope = desk_file_recovery::Scope {
            authority: "a".repeat(64),
            device: "d".into(),
            os_user: "501".into(),
            owner: "1".into(),
        };
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let record = vault
            .lock()
            .unwrap()
            .backup_with_reservation_in_epoch(
                desk_file_recovery::BackupRequest {
                    scope,
                    conversation: "c",
                    operation: "op",
                    generation: "g",
                    file_name: "notes.txt",
                    content: b"before",
                    metadata: b"{}",
                    now_ms: 1000,
                },
                0,
                |_| Ok(()),
            )
            .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = QuotaClient::new(sender);
        for succeeds in [false, true] {
            let client_worker = client.clone();
            let path = root.path().to_owned();
            let worker = std::thread::spawn(move || {
                let vault = desk_file_recovery::Vault::open(&path).unwrap();
                client_worker.reconcile_settlements(&mut vault.lock().unwrap());
            });
            let WorkerToService::FileRecoveryQuotaRequested(request) =
                receiver.blocking_recv().unwrap()
            else {
                panic!("quota expected")
            };
            let FileRecoveryQuotaCommand::Settle { identity, bytes } = request.command else {
                panic!("settlement expected")
            };
            assert_eq!(identity.operation, record.operation);
            assert_eq!(bytes, record.bytes);
            client.complete(FileRecoveryQuotaReply {
                request_id: request.request_id,
                outcome: if succeeds {
                    FileRecoveryQuotaOutcome::Applied {
                        retention_days: 7,
                        max_bytes: 1048576,
                        used_bytes: bytes,
                        reserved_bytes: 0,
                    }
                } else {
                    FileRecoveryQuotaOutcome::StorageUnavailable
                },
            });
            worker.join().unwrap();
            assert_eq!(
                vault.lock().unwrap().pending_quota_settlement().len(),
                usize::from(!succeeds)
            );
        }
        client.reconcile_settlements(&mut vault.lock().unwrap());
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            vault
                .lock()
                .unwrap()
                .export(&record.scope, &record.id, 1001)
                .unwrap()
                .1,
            b"before"
        );
    }
    #[test]
    fn matching_reply_completes_and_timed_out_requests_discard_late_replies() {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = QuotaClient::new(sender);
        let request_client = client.clone();
        let worker =
            std::thread::spawn(move || request_client.request(FileRecoveryQuotaCommand::Read));
        let WorkerToService::FileRecoveryQuotaRequested(request) =
            receiver.blocking_recv().unwrap()
        else {
            panic!("quota expected")
        };
        client.complete(FileRecoveryQuotaReply {
            request_id: "different".into(),
            outcome: FileRecoveryQuotaOutcome::IdentityChanged,
        });
        assert_eq!(client.pending.lock().unwrap().len(), 1);
        client.complete(FileRecoveryQuotaReply {
            request_id: request.request_id,
            outcome: FileRecoveryQuotaOutcome::Applied {
                retention_days: 7,
                max_bytes: 1048576,
                used_bytes: 3,
                reserved_bytes: 0,
            },
        });
        assert_eq!(
            worker.join().unwrap().unwrap(),
            (
                desk_file_recovery::Policy {
                    retention_days: 7,
                    max_bytes: 1048576
                },
                3,
                0
            )
        );
        assert_eq!(
            client
                .request_with_timeout(FileRecoveryQuotaCommand::Read, Duration::from_millis(1))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let WorkerToService::FileRecoveryQuotaRequested(late) = receiver.blocking_recv().unwrap()
        else {
            panic!("quota expected")
        };
        client.complete(FileRecoveryQuotaReply {
            request_id: late.request_id,
            outcome: FileRecoveryQuotaOutcome::Applied {
                retention_days: 7,
                max_bytes: 1048576,
                used_bytes: 4,
                reserved_bytes: 0,
            },
        });
        assert!(client.pending.lock().unwrap().is_empty());
    }
    #[test]
    fn epoch_coordinator_preserves_user_epoch_and_decodes_acknowledgments() {
        use desk_file_recovery::EpochCoordinator;
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let client = QuotaClient::new(sender);
        let mut worker_client = client.clone();
        assert_eq!(
            worker_client.begin("wrong-user", 7).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(receiver.try_recv().is_err());
        let user = client.os_user.clone().unwrap();
        let expected_user = user.clone();
        let worker = std::thread::spawn(move || {
            let advanced = worker_client.begin(&user, 7).unwrap();
            assert_eq!(advanced.epoch, 8);
            assert!(advanced.cleanup_pending);
            let finished = worker_client.finish(&user, 8).unwrap();
            assert_eq!(finished.epoch, 8);
            assert!(!finished.cleanup_pending);
        });
        for begin in [true, false] {
            let WorkerToService::FileRecoveryQuotaRequested(request) =
                receiver.blocking_recv().unwrap()
            else {
                panic!("quota request expected");
            };
            assert_eq!(request.os_user, expected_user);
            match request.command {
                FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch } if begin => {
                    assert_eq!(expected_epoch, 7)
                }
                FileRecoveryQuotaCommand::FinishEpochCleanup { execution_epoch } if !begin => {
                    assert_eq!(execution_epoch, 8)
                }
                _ => panic!("incorrect cleanup sequence"),
            }
            client.complete(FileRecoveryQuotaReply {
                request_id: request.request_id,
                outcome: FileRecoveryQuotaOutcome::Epoch {
                    execution_epoch: 8,
                    cleanup_pending: begin,
                },
            });
        }
        worker.join().unwrap();
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn disconnected_transport_does_not_leave_pending_requests() {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        let client = QuotaClient::new(sender);
        assert_eq!(
            client
                .request(FileRecoveryQuotaCommand::Read)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(client.pending.lock().unwrap().is_empty());
    }
}

impl QuotaClient {
    fn epoch_request(
        &self,
        os_user: &str,
        command: FileRecoveryQuotaCommand,
    ) -> io::Result<desk_file_recovery::quota::EpochStatus> {
        if self.os_user.as_deref() != Some(os_user) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution user changed",
            ));
        }
        match self.raw_request_with_timeout(command, Duration::from_secs(10))? {
            FileRecoveryQuotaOutcome::Epoch {
                execution_epoch,
                cleanup_pending,
            } => Ok(desk_file_recovery::quota::EpochStatus {
                epoch: execution_epoch,
                cleanup_pending,
            }),
            other => {
                Self::policy_response(other)?;
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "expected quota epoch reply",
                ))
            }
        }
    }
}
impl desk_file_recovery::EpochCoordinator for QuotaClient {
    fn begin(
        &mut self,
        os_user: &str,
        expected_epoch: u64,
    ) -> io::Result<desk_file_recovery::quota::EpochStatus> {
        self.epoch_request(
            os_user,
            FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch },
        )
    }
    fn finish(
        &mut self,
        os_user: &str,
        execution_epoch: u64,
    ) -> io::Result<desk_file_recovery::quota::EpochStatus> {
        self.epoch_request(
            os_user,
            FileRecoveryQuotaCommand::FinishEpochCleanup { execution_epoch },
        )
    }
}
