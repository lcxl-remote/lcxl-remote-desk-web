//! The shared device ledger is opened only under the daemon's data root.
use super::*;
use desk_file_recovery::quota::{DeviceQuota, QuotaKey};
use desk_ipc_protocol::message::*;

impl WorkerManager {
    pub async fn handle_file_recovery_quota(
        &self,
        worker_key: Option<&WorkerKey>,
        incarnation: WorkerIncarnation,
        request: FileRecoveryQuotaRequest,
    ) {
        let (reply_tx, os_user) = {
            let inner = self.inner.lock().await;
            let worker = match worker_key {
                Some(key) => inner.resident_workers.get(key),
                None if !self.uses_session_targeting() => inner.active_worker.as_ref(),
                _ => None,
            };
            let Some(worker) = worker.filter(|worker| worker.incarnation == incarnation) else {
                return;
            };
            let os_user = match worker_key {
                None => {
                    #[cfg(unix)]
                    {
                        Some(unsafe { libc::geteuid() }.to_string())
                    }
                    #[cfg(not(unix))]
                    {
                        None::<String>
                    }
                }
                Some(key) => {
                    #[cfg(target_os = "linux")]
                    {
                        self.session_shell_registration(&key.session)
                            .map(|registration| registration.process_identity.uid.to_string())
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        let _ = key;
                        None::<String>
                    }
                }
            };
            (worker.ipc_tx.clone(), os_user)
        };
        let Some(os_user) = os_user.filter(|user| user == &request.os_user) else {
            let _ = reply_tx.send(ServiceToWorker::FileRecoveryQuotaReplied(
                FileRecoveryQuotaReply {
                    request_id: request.request_id,
                    outcome: FileRecoveryQuotaOutcome::IdentityChanged,
                },
            ));
            return;
        };
        let root = self.settings.read().await.paths().data_root().to_owned();
        tokio::spawn(async move {
            let request_id = request.request_id.clone();
            let outcome =
                tokio::task::spawn_blocking(move || apply(&root, &os_user, request.command))
                    .await
                    .unwrap_or(FileRecoveryQuotaOutcome::StorageUnavailable);
            // Send only to the original worker handle, never its replacement.
            let _ = reply_tx.send(ServiceToWorker::FileRecoveryQuotaReplied(
                FileRecoveryQuotaReply {
                    request_id,
                    outcome,
                },
            ));
        });
    }
}
fn apply(
    root: &std::path::Path,
    os_user: &str,
    command: FileRecoveryQuotaCommand,
) -> FileRecoveryQuotaOutcome {
    use std::io;
    let result = (|| -> io::Result<_> {
        let mut quota = DeviceQuota::open(root)?.lock()?;
        let key = |identity: FileRecoveryQuotaIdentity| {
            let mut key = QuotaKey::new(
                &desk_file_recovery::Scope {
                    authority: identity.authority,
                    device: identity.device,
                    owner: identity.owner,
                    os_user: os_user.into(),
                },
                &identity.conversation,
                &identity.operation,
                &identity.generation,
            )?;
            key.epoch = identity.execution_epoch;
            Ok::<_, io::Error>(key)
        };
        match command {
            FileRecoveryQuotaCommand::BeginEpochCleanup { expected_epoch } => {
                let state = quota.begin_epoch_cleanup(os_user, expected_epoch)?;
                return Ok(FileRecoveryQuotaOutcome::Epoch {
                    execution_epoch: state.epoch,
                    cleanup_pending: state.cleanup_pending,
                });
            }
            FileRecoveryQuotaCommand::FinishEpochCleanup { execution_epoch } => {
                let state = quota.finish_epoch_cleanup(os_user, execution_epoch)?;
                return Ok(FileRecoveryQuotaOutcome::Epoch {
                    execution_epoch: state.epoch,
                    cleanup_pending: state.cleanup_pending,
                });
            }
            FileRecoveryQuotaCommand::Read => (),
            FileRecoveryQuotaCommand::SetPolicy {
                retention_days,
                max_bytes,
            } => quota.set_policy(desk_file_recovery::Policy {
                retention_days,
                max_bytes,
            })?,
            FileRecoveryQuotaCommand::Reserve {
                identity,
                bytes,
                execution_deadline_ms,
            } => {
                let now = chrono::Utc::now().timestamp_millis().max(1) as u64;
                if execution_deadline_ms > now.saturating_add(60_000) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "quota deadline exceeds execution bound",
                    ));
                }
                quota.reserve(key(identity)?, bytes, execution_deadline_ms, now)?;
            }
            FileRecoveryQuotaCommand::Settle { identity, bytes } => {
                quota.settle(&key(identity)?, bytes)?
            }
            FileRecoveryQuotaCommand::Release {
                identity,
                retained_index_bytes,
            } => quota.release_with_retained_index(&key(identity)?, retained_index_bytes)?,
            FileRecoveryQuotaCommand::ReleaseNamespace {
                namespace,
                execution_epoch,
            } => quota.release_namespace_at_epoch(&namespace, os_user, execution_epoch)?,
        }
        Ok(FileRecoveryQuotaOutcome::Applied {
            retention_days: quota.policy().retention_days,
            max_bytes: quota.max_bytes(),
            used_bytes: quota.used_bytes(),
            reserved_bytes: quota.reserved_bytes(),
        })
    })();
    result.unwrap_or_else(|error| match error.kind() {
        io::ErrorKind::PermissionDenied => FileRecoveryQuotaOutcome::IdentityChanged,
        io::ErrorKind::StorageFull => FileRecoveryQuotaOutcome::CapacityExceeded,
        io::ErrorKind::InvalidInput | io::ErrorKind::AlreadyExists => FileRecoveryQuotaOutcome::InvalidRequest,
        _ => { tracing::warn!(error_kind = ?error.kind(), "Device file recovery quota operation failed"); FileRecoveryQuotaOutcome::StorageUnavailable },
    })
}
