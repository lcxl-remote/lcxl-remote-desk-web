//! Worker-side backup management. Remote operations always retain their frozen Scope.
use desk_agent_protocol::file_recovery::*;
use desk_file_recovery::{ChangeState, LockedVault, MaterialState, Record, Scope};
use std::{io, path::Path};
#[path = "file_recovery_local.rs"]
pub(crate) mod local;
#[path = "file_recovery_local_quota.rs"]
mod local_quota;
#[path = "file_recovery_user.rs"]
pub(crate) mod platform_user;

pub(crate) fn project_record(vault: &LockedVault, r: Record, now: u64) -> FileRecoveryRecordDto {
    FileRecoveryRecordDto {
        cleanup_reason: if r.transaction.is_some() && r.cleanup_error.is_some() {
            Some("temporary_file_pending".into())
        } else if r.cleanup_error.is_some() {
            Some("storage_unavailable".into())
        } else if r.material == MaterialState::Purged && r.device_quota_pending {
            Some("quota_pending".into())
        } else if r.device_quota_managed
            && !r.device_quota_settled
            && r.material == MaterialState::Saved
        {
            Some("quota_settlement_pending".into())
        } else {
            None
        },
        cleanup_pending: (r.material == MaterialState::Purged && r.device_quota_pending)
            || r.cleanup_error.is_some()
            || r.transaction.is_some()
            || vault.is_deleted(&r.scope, &r.conversation),
        export_available: r.material == MaterialState::Saved
            && (r.expires_at_ms > now || r.change == ChangeState::OutcomeUnknown)
            && !r.discard_requested
            && !vault.is_deleted(&r.scope, &r.conversation),
        recovery_id: r.id,
        conversation_id: r.conversation,
        file_name: r.file_name,
        created_at_unix_ms: r.created_at_ms,
        expires_at_unix_ms: r.expires_at_ms,
        size_bytes: r.bytes,
        change_state: match r.change {
            ChangeState::Preparing => "preparing",
            ChangeState::BackupReady => "backup_ready",
            ChangeState::CommitIntent | ChangeState::OutcomeUnknown => "outcome_unknown",
            ChangeState::Succeeded => "succeeded",
            ChangeState::Aborted => "aborted",
        }
        .into(),
        material_state: match r.material {
            MaterialState::Preparing => "preparing",
            MaterialState::Saved => "saved",
            MaterialState::Purging => "purging",
            MaterialState::Purged => "purged",
        }
        .into(),
    }
}
fn policy(vault: &LockedVault) -> FileRecoveryPolicyDto {
    FileRecoveryPolicyDto {
        retention_days: vault.policy().retention_days,
        max_bytes: vault.policy().max_bytes,
    }
}
#[cfg(any(target_os = "macos", windows))]
pub(crate) fn execute(
    root: Option<&Path>,
    payload: &desk_ipc_protocol::message::FileRecoveryRequestPayload,
    quota: Option<&crate::worker::session::QuotaClient>,
) -> FileRecoveryReply {
    let os_user = match platform_user::current() {
        Ok(user) => user,
        Err(_) => {
            return FileRecoveryReply {
                authority: payload.authority.clone(),
                os_user: String::new(),
                outcome: FileRecoveryOutcome::Unavailable {
                    reason: FileRecoveryFailure::IdentityChanged,
                },
            };
        }
    };
    let result = (|| {
        payload
            .request
            .command
            .validate()
            .map_err(|_| FileRecoveryFailure::InvalidRequest)?;
        if payload
            .request
            .expected_authority
            .as_deref()
            .is_some_and(|id| id != payload.authority)
        {
            return Err(FileRecoveryFailure::IdentityChanged);
        }
        if payload
            .request
            .expected_os_user
            .as_deref()
            .is_some_and(|id| id != os_user)
        {
            return Err(FileRecoveryFailure::IdentityChanged);
        }
        let quota = quota.ok_or(FileRecoveryFailure::StorageUnavailable)?;
        if let FileRecoveryCommand::SetPolicy {
            retention_days,
            max_bytes,
        } = &payload.request.command
        {
            let (policy, _, _) = quota
                .set_policy(desk_file_recovery::Policy {
                    retention_days: *retention_days,
                    max_bytes: *max_bytes,
                })
                .map_err(storage_failure)?;
            return Ok(FileRecoveryOutcome::Policy {
                policy: FileRecoveryPolicyDto {
                    retention_days: policy.retention_days,
                    max_bytes: policy.max_bytes,
                },
            });
        }
        let root = root.ok_or(FileRecoveryFailure::StorageUnavailable)?;
        let vault = desk_file_recovery::Vault::open(root).map_err(storage_failure)?;
        let mut locked = vault
            .try_lock()
            .map_err(storage_failure)?
            .ok_or(FileRecoveryFailure::Busy)?;
        locked.observe_system_clock().map_err(storage_failure)?;
        let scope = Scope {
            authority: payload.authority.clone(),
            device: payload.device_id.clone(),
            os_user: os_user.clone(),
            owner: payload.actor_id.clone(),
        };
        locked
            .maintain_epoch_indexes(
                &os_user,
                &mut quota.clone(),
                if matches!(payload.request.command, FileRecoveryCommand::Query { .. }) {
                    64
                } else {
                    usize::MAX
                },
            )
            .map_err(storage_failure)?;
        let usage = if matches!(
            payload.request.command,
            FileRecoveryCommand::DeleteConversation { .. }
        ) {
            None
        } else {
            let (policy, used, reserved) = quota.policy().map_err(storage_failure)?;
            if locked.policy() != &policy {
                locked.set_policy(policy).map_err(storage_failure)?;
            }
            Some((used, reserved))
        };
        let mut outcome = execute_locked_with_quota(
            &mut locked,
            &scope,
            &payload.request.command,
            chrono::Utc::now().timestamp_millis().max(1) as u64,
            true,
        )?;
        if matches!(
            payload.request.command,
            FileRecoveryCommand::DeleteConversation { .. }
                | FileRecoveryCommand::RetryCleanup
                | FileRecoveryCommand::Discard { .. }
        ) {
            quota.reconcile_settlements(&mut locked);
            for record in locked.pending_quota_cleanup().into_iter().take(32) {
                match quota.release(&record) {
                    Ok(()) => locked
                        .acknowledge_quota_cleanup(&record.scope, &record.id)
                        .map_err(storage_failure)?,
                    Err(error) => {
                        tracing::warn!(error_kind = ?error.kind(), "File recovery quota cleanup unconfirmed; retaining retry record")
                    }
                }
            }
            for namespace in locked.pending_quota_namespaces().into_iter().take(32) {
                match quota.release_namespace(
                    &namespace,
                    locked
                        .namespace_epoch(&namespace)
                        .map_err(storage_failure)?,
                ) {
                    Ok(()) => locked
                        .acknowledge_quota_namespace(&namespace)
                        .map_err(storage_failure)?,
                    Err(error) => {
                        tracing::warn!(error_kind = ?error.kind(), "File recovery namespace quota cleanup unconfirmed; retaining retry record")
                    }
                }
            }
        }
        if matches!(payload.request.command, FileRecoveryCommand::RetryCleanup) {
            locked
                .maintain_epoch_indexes(&os_user, &mut quota.clone(), 1)
                .map_err(storage_failure)?;
        }
        match &mut outcome {
            FileRecoveryOutcome::Page { page } => {
                if let Some((used, reserved)) = usage {
                    page.used_bytes = used;
                    page.reserved_bytes = reserved;
                }
            }
            FileRecoveryOutcome::Deleted { complete } => {
                if let FileRecoveryCommand::DeleteConversation { conversation_id } =
                    &payload.request.command
                {
                    *complete = locked.cleanup_complete(&scope, conversation_id);
                }
            }
            FileRecoveryOutcome::Cleanup { report } => {
                report.pending_files = locked.pending_cleanup_count()
            }
            _ => (),
        }
        Ok(outcome)
    })();
    FileRecoveryReply {
        authority: payload.authority.clone(),
        os_user,
        outcome: result.unwrap_or_else(|reason| FileRecoveryOutcome::Unavailable { reason }),
    }
}
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) fn execute(
    _: Option<&Path>,
    payload: &desk_ipc_protocol::message::FileRecoveryRequestPayload,
    _: Option<&crate::worker::session::QuotaClient>,
) -> FileRecoveryReply {
    FileRecoveryReply {
        authority: payload.authority.clone(),
        os_user: String::new(),
        outcome: FileRecoveryOutcome::Unavailable {
            reason: FileRecoveryFailure::Unsupported,
        },
    }
}
pub(crate) fn storage_failure(error: io::Error) -> FileRecoveryFailure {
    let reason = if let Some(reason) = error
        .get_ref()
        .and_then(|error| error.downcast_ref::<desk_file_recovery::ExportError>())
    {
        match reason {
            desk_file_recovery::ExportError::Unavailable => {
                FileRecoveryFailure::MaterialUnavailable
            }
            desk_file_recovery::ExportError::Expired => FileRecoveryFailure::MaterialExpired,
            desk_file_recovery::ExportError::Cleaning => FileRecoveryFailure::MaterialCleaning,
            desk_file_recovery::ExportError::Cleaned => FileRecoveryFailure::MaterialCleaned,
            desk_file_recovery::ExportError::Preparing => FileRecoveryFailure::Busy,
        }
    } else if error
        .get_ref()
        .is_some_and(|error| error.is::<desk_file_recovery::CleanupClockError>())
    {
        FileRecoveryFailure::ClockChanged
    } else if error.kind() == io::ErrorKind::WouldBlock {
        FileRecoveryFailure::Busy
    } else {
        match error.kind() {
            io::ErrorKind::InvalidInput => FileRecoveryFailure::InvalidRequest,
            io::ErrorKind::NotFound => FileRecoveryFailure::MaterialUnavailable,
            io::ErrorKind::Unsupported => FileRecoveryFailure::Unsupported,
            _ => FileRecoveryFailure::StorageUnavailable,
        }
    };
    tracing::warn!(error_kind = ?error.kind(), ?reason, "File recovery management storage operation failed");
    reason
}
#[cfg(test)]
fn execute_locked(
    vault: &mut LockedVault,
    scope: &Scope,
    command: &FileRecoveryCommand,
    now: u64,
) -> Result<FileRecoveryOutcome, FileRecoveryFailure> {
    execute_locked_with_quota(vault, scope, command, now, false)
}
fn execute_locked_with_quota(
    vault: &mut LockedVault,
    scope: &Scope,
    command: &FileRecoveryCommand,
    now: u64,
    shared_quota: bool,
) -> Result<FileRecoveryOutcome, FileRecoveryFailure> {
    match command {
        FileRecoveryCommand::ConfirmClock {
            displayed_time_unix_ms,
            confirmed,
        } => {
            if !confirmed {
                return Err(FileRecoveryFailure::InvalidRequest);
            }
            vault
                .acknowledge_clock(*displayed_time_unix_ms)
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::InvalidInput {
                        FileRecoveryFailure::InvalidRequest
                    } else {
                        storage_failure(error)
                    }
                })?;
            Ok(FileRecoveryOutcome::Cleanup {
                report: FileRecoveryCleanupDto {
                    pending_files: vault.pending_cleanup_count(),
                    unknown_outcomes: vault.unknown_outcome_count(Some(scope)),
                },
            })
        }
        FileRecoveryCommand::Discard {
            recovery_id,
            conversation_id,
            confirmed,
        } => {
            if !confirmed {
                return Err(FileRecoveryFailure::InvalidRequest);
            }
            vault
                .discard(scope, conversation_id, recovery_id, now)
                .map_err(storage_failure)?;
            Ok(FileRecoveryOutcome::Cleanup {
                report: FileRecoveryCleanupDto {
                    pending_files: vault.pending_cleanup_count(),
                    unknown_outcomes: vault
                        .list(scope, None)
                        .iter()
                        .filter(|record| {
                            record.change == ChangeState::OutcomeUnknown
                                && record.material != MaterialState::Purged
                        })
                        .count() as u64,
                },
            })
        }
        FileRecoveryCommand::Query {
            conversation_id,
            after,
        } => {
            let oldest = vault.oldest_pending_record(Some(scope), conversation_id.as_deref());
            let mut records: Vec<_> = vault
                .list(scope, conversation_id.as_deref())
                .into_iter()
                .filter(|r| {
                    r.material != MaterialState::Purged
                        && after.as_deref().is_none_or(|id| r.id.as_str() > id)
                })
                .take(101)
                .collect();
            let more = records.len() > 100;
            records.truncate(100);
            Ok(FileRecoveryOutcome::Page {
                page: FileRecoveryPageDto {
                    execution_epoch: vault.execution_epoch(),
                    cleanup_warning: vault
                        .cleanup_clock_paused()
                        .then_some(FileRecoveryFailure::ClockChanged),
                    clock_confirmation_time_unix_ms: vault.cleanup_clock_paused().then_some(now),
                    oldest_pending_at_unix_ms: oldest.as_ref().map(|record| record.created_at_ms),
                    oldest_pending_record: oldest.map(|record| project_record(vault, record, now)),
                    policy: policy(vault),
                    used_bytes: vault.used_bytes(),
                    reserved_bytes: 0,
                    next_cursor: more.then(|| records.last().unwrap().id.clone()),
                    records: records
                        .into_iter()
                        .map(|r| project_record(vault, r, now))
                        .collect(),
                },
            })
        }
        FileRecoveryCommand::SetPolicy {
            retention_days,
            max_bytes,
        } => {
            vault
                .set_policy(desk_file_recovery::Policy {
                    retention_days: *retention_days,
                    max_bytes: *max_bytes,
                })
                .map_err(storage_failure)?;
            Ok(FileRecoveryOutcome::Policy {
                policy: policy(vault),
            })
        }
        FileRecoveryCommand::DeleteConversation { conversation_id } => {
            let foreign = vault.has_foreign_conversation(scope, conversation_id);
            if shared_quota {
                vault.delete_conversation_with_quota(scope, conversation_id, now)
            } else {
                vault.delete_conversation(scope, conversation_id, now)
            }
            .map_err(storage_failure)?;
            if foreign {
                return Err(FileRecoveryFailure::IdentityChanged);
            }
            Ok(FileRecoveryOutcome::Deleted {
                complete: vault.cleanup_complete(scope, conversation_id),
            })
        }
        FileRecoveryCommand::RetryCleanup => {
            vault.recover_interrupted(now).map_err(storage_failure)?;
            let records = vault.list(scope, None);
            Ok(FileRecoveryOutcome::Cleanup {
                report: FileRecoveryCleanupDto {
                    pending_files: records
                        .iter()
                        .filter(|r| {
                            r.transaction.is_some()
                                || r.cleanup_error.is_some()
                                || (r.material == MaterialState::Purged && r.device_quota_pending)
                        })
                        .count() as u64,
                    unknown_outcomes: records
                        .iter()
                        .filter(|r| {
                            matches!(
                                r.change,
                                ChangeState::CommitIntent | ChangeState::OutcomeUnknown
                            ) && r.material != MaterialState::Purged
                        })
                        .count() as u64,
                },
            })
        }
        FileRecoveryCommand::Export {
            recovery_id,
            conversation_id,
        } => {
            if !vault
                .list(scope, Some(conversation_id))
                .iter()
                .any(|r| &r.id == recovery_id)
            {
                return Err(FileRecoveryFailure::MaterialUnavailable);
            }
            use base64::Engine;
            let bytes = vault
                .export_package(scope, recovery_id, now)
                .map_err(storage_failure)?;
            Ok(FileRecoveryOutcome::Export {
                zip_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn management_rejects_changed_os_user_before_opening_storage() {
        let payload = desk_ipc_protocol::message::FileRecoveryRequestPayload {
            request_id: "request".into(),
            connection_id: None,
            authority: "a".repeat(64),
            actor_id: "1".into(),
            device_id: "device".into(),
            request: FileRecoveryRequest {
                expected_authority: None,
                expected_os_user: Some("different-user".into()),
                command: FileRecoveryCommand::SetPolicy {
                    retention_days: 7,
                    max_bytes: 1_048_576,
                },
            },
        };
        assert!(matches!(
            execute(None, &payload, None).outcome,
            FileRecoveryOutcome::Unavailable {
                reason: FileRecoveryFailure::IdentityChanged
            }
        ));
    }
    #[test]
    fn remote_query_uses_shared_policy_and_usage_without_exposing_other_user_records() {
        use desk_ipc_protocol::message::*;
        let root = tempfile::tempdir().unwrap();
        let mut scope = scope();
        scope.os_user = unsafe { libc::geteuid() }.to_string();
        let now = chrono::Utc::now().timestamp_millis().max(1) as u64;
        desk_file_recovery::Vault::open(root.path())
            .unwrap()
            .lock()
            .unwrap()
            .backup(desk_file_recovery::BackupRequest {
                scope: scope.clone(),
                conversation: "conversation",
                operation: "operation",
                generation: "generation",
                file_name: "notes.txt",
                content: b"before",
                metadata: b"{}",
                now_ms: now,
            })
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let client = crate::worker::session::QuotaClient::new(tx);
        let responder = client.clone();
        let path = root.path().to_owned();
        let worker = std::thread::spawn(move || {
            execute(
                Some(&path),
                &FileRecoveryRequestPayload {
                    request_id: "query".into(),
                    connection_id: None,
                    authority: scope.authority,
                    actor_id: scope.owner,
                    device_id: scope.device,
                    request: FileRecoveryRequest {
                        expected_authority: None,
                        expected_os_user: Some(scope.os_user),
                        command: FileRecoveryCommand::Query {
                            conversation_id: Some("conversation".into()),
                            after: None,
                        },
                    },
                },
                Some(&client),
            )
        });
        let WorkerToService::FileRecoveryQuotaRequested(request) = next_quota_request(&mut rx)
        else {
            panic!("quota read expected")
        };
        assert!(matches!(request.command, FileRecoveryQuotaCommand::Read));
        responder.complete(FileRecoveryQuotaReply {
            request_id: request.request_id,
            outcome: FileRecoveryQuotaOutcome::Applied {
                retention_days: 12,
                max_bytes: 1048576,
                used_bytes: 999999,
                reserved_bytes: 123456,
            },
        });
        let FileRecoveryOutcome::Page { page } = worker.join().unwrap().outcome else {
            panic!("page expected")
        };
        assert_eq!(page.policy.retention_days, 12);
        assert_eq!(page.used_bytes, 999999);
        assert_eq!(page.reserved_bytes, 123456);
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].conversation_id, "conversation");
        assert_eq!(page.records[0].expires_at_unix_ms, now + 12 * 86_400_000);
    }
    #[test]
    fn deletion_waits_for_record_and_namespace_quota_cleanup_acknowledgments() {
        use desk_ipc_protocol::message::*;
        let root = tempfile::tempdir().unwrap();
        let mut scope = scope();
        scope.os_user = unsafe { libc::geteuid() }.to_string();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        let record = locked
            .backup_with_reservation_in_epoch(
                desk_file_recovery::BackupRequest {
                    scope: scope.clone(),
                    conversation: "conversation",
                    operation: "operation",
                    generation: "generation",
                    file_name: "notes.txt",
                    content: b"before",
                    metadata: b"{}",
                    now_ms: 1,
                },
                0,
                |_| Ok(()),
            )
            .unwrap();
        locked
            .transition(&scope, &record.id, ChangeState::Aborted)
            .unwrap();
        drop(locked);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let client = crate::worker::session::QuotaClient::new(tx);
        let responder = client.clone();
        let path = root.path().to_owned();
        let payload_scope = scope.clone();
        let worker = std::thread::spawn(move || {
            execute(
                Some(&path),
                &FileRecoveryRequestPayload {
                    request_id: "delete".into(),
                    connection_id: None,
                    authority: payload_scope.authority,
                    actor_id: payload_scope.owner,
                    device_id: payload_scope.device,
                    request: FileRecoveryRequest {
                        expected_authority: None,
                        expected_os_user: Some(payload_scope.os_user),
                        command: FileRecoveryCommand::DeleteConversation {
                            conversation_id: "conversation".into(),
                        },
                    },
                },
                Some(&client),
            )
        });
        for stage in 0..2 {
            let WorkerToService::FileRecoveryQuotaRequested(request) = next_quota_request(&mut rx)
            else {
                panic!("quota cleanup expected")
            };
            match (stage, request.command) {
                (
                    0,
                    FileRecoveryQuotaCommand::Release {
                        retained_index_bytes,
                        ..
                    },
                ) => assert!(retained_index_bytes > 0),
                (1, FileRecoveryQuotaCommand::ReleaseNamespace { namespace, .. }) => {
                    assert_eq!(namespace.len(), 128)
                }
                _ => panic!("cleanup stages out of order"),
            }
            responder.complete(FileRecoveryQuotaReply {
                request_id: request.request_id,
                outcome: FileRecoveryQuotaOutcome::Applied {
                    retention_days: 7,
                    max_bytes: 1048576,
                    used_bytes: 1024,
                    reserved_bytes: 0,
                },
            });
        }
        assert!(matches!(
            worker.join().unwrap().outcome,
            FileRecoveryOutcome::Deleted { complete: true }
        ));
        let locked = vault.lock().unwrap();
        assert!(locked.cleanup_complete(&scope, "conversation"));
        assert!(locked.pending_quota_cleanup().is_empty());
        assert!(locked.pending_quota_namespaces().is_empty());
    }
    fn next_quota_request(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<desk_ipc_protocol::message::WorkerToService>,
    ) -> desk_ipc_protocol::message::WorkerToService {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                    .await
                    .expect("worker quota request timed out")
                    .expect("worker quota channel closed")
            })
    }
    fn scope() -> Scope {
        Scope {
            authority: "a".repeat(64),
            device: "device".into(),
            os_user: "501".into(),
            owner: "1".into(),
        }
    }
    #[test]
    fn clock_confirmation_rejects_missing_approval_and_stale_time_before_resuming() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        locked.cleanup(1000).unwrap();
        assert!(locked.cleanup(999).is_err());
        let scope = scope();
        assert!(matches!(
            execute_locked(
                &mut locked,
                &scope,
                &FileRecoveryCommand::ConfirmClock {
                    displayed_time_unix_ms: 1000,
                    confirmed: false
                },
                1000
            ),
            Err(FileRecoveryFailure::InvalidRequest)
        ));
        assert!(matches!(
            execute_locked(
                &mut locked,
                &scope,
                &FileRecoveryCommand::ConfirmClock {
                    displayed_time_unix_ms: 1000,
                    confirmed: true
                },
                1000
            ),
            Err(FileRecoveryFailure::InvalidRequest)
        ));
        assert!(locked.cleanup_clock_paused());
        let now = chrono::Utc::now().timestamp_millis() as u64;
        assert!(matches!(
            execute_locked(
                &mut locked,
                &scope,
                &FileRecoveryCommand::ConfirmClock {
                    displayed_time_unix_ms: now,
                    confirmed: true
                },
                now
            ),
            Ok(FileRecoveryOutcome::Cleanup { .. })
        ));
        assert!(!locked.cleanup_clock_paused());
    }
    #[test]
    fn clock_failure_is_readable_and_query_keeps_the_warning_and_oldest_record() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        let scope = scope();
        let record = locked
            .backup(desk_file_recovery::BackupRequest {
                scope: scope.clone(),
                conversation: "conversation",
                operation: "operation",
                generation: "generation",
                file_name: "notes.txt",
                content: b"before",
                metadata: b"{}",
                now_ms: 1000,
            })
            .unwrap();
        locked
            .transition(&scope, &record.id, ChangeState::CommitIntent)
            .unwrap();
        locked.recover_interrupted(1001).unwrap();
        assert!(matches!(
            execute_locked(&mut locked, &scope, &FileRecoveryCommand::RetryCleanup, 999),
            Err(FileRecoveryFailure::ClockChanged)
        ));
        let FileRecoveryOutcome::Page { page } = execute_locked(
            &mut locked,
            &scope,
            &FileRecoveryCommand::Query {
                conversation_id: None,
                after: None,
            },
            1000,
        )
        .unwrap() else {
            panic!("page expected")
        };
        assert_eq!(
            page.cleanup_warning,
            Some(FileRecoveryFailure::ClockChanged)
        );
        assert_eq!(page.oldest_pending_at_unix_ms, Some(1000));
        assert_eq!(
            page.oldest_pending_record.as_ref().unwrap().recovery_id,
            record.id
        );
        let FileRecoveryOutcome::Page { page: beyond } = execute_locked(
            &mut locked,
            &scope,
            &FileRecoveryCommand::Query {
                conversation_id: Some("conversation".into()),
                after: Some("z".into()),
            },
            1000,
        )
        .unwrap() else {
            panic!("page expected")
        };
        assert!(beyond.records.is_empty());
        assert_eq!(beyond.oldest_pending_record.unwrap().recovery_id, record.id);
        let FileRecoveryOutcome::Page { page: unrelated } = execute_locked(
            &mut locked,
            &scope,
            &FileRecoveryCommand::Query {
                conversation_id: Some("other-conversation".into()),
                after: None,
            },
            1000,
        )
        .unwrap() else {
            panic!("page expected")
        };
        assert!(unrelated.oldest_pending_record.is_none());
        assert!(unrelated.oldest_pending_at_unix_ms.is_none());

        assert!(page.records[0].export_available);
        let mut other = scope.clone();
        other.owner = "other".into();
        assert_eq!(locked.oldest_pending_created_at(Some(&other)), None);
        assert_eq!(
            storage_failure(io::Error::from(io::ErrorKind::WouldBlock)),
            FileRecoveryFailure::Busy
        );
    }
    #[test]
    fn discard_requires_confirmation_and_preserves_unknown_result() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        let scope = scope();
        let record = locked
            .backup(desk_file_recovery::BackupRequest {
                scope: scope.clone(),
                conversation: "conversation",
                operation: "operation",
                generation: "generation",
                file_name: "notes.txt",
                content: b"before",
                metadata: b"{}",
                now_ms: 1000,
            })
            .unwrap();
        locked
            .transition(&scope, &record.id, ChangeState::CommitIntent)
            .unwrap();
        locked
            .recover_interrupted(record.expires_at_ms + 1)
            .unwrap();
        assert!(
            project_record(
                &locked,
                locked.list(&scope, None)[0].clone(),
                record.expires_at_ms + 1
            )
            .export_available
        );
        let mut command = FileRecoveryCommand::Discard {
            recovery_id: record.id.clone(),
            conversation_id: "conversation".into(),
            confirmed: false,
        };
        assert!(matches!(
            execute_locked(&mut locked, &scope, &command, record.expires_at_ms + 1),
            Err(FileRecoveryFailure::InvalidRequest)
        ));
        assert!(
            locked
                .export(&scope, &record.id, record.expires_at_ms + 1)
                .is_ok()
        );
        if let FileRecoveryCommand::Discard { confirmed, .. } = &mut command {
            *confirmed = true;
        }
        assert!(matches!(
            execute_locked(&mut locked, &scope, &command, record.expires_at_ms + 1),
            Ok(FileRecoveryOutcome::Cleanup { .. })
        ));
        let retained = locked.list(&scope, None)[0].clone();
        assert_eq!(retained.change, ChangeState::OutcomeUnknown);
        assert_eq!(retained.material, MaterialState::Purged);
        assert!(!project_record(&locked, retained, record.expires_at_ms + 1).export_available);
    }
    #[test]
    fn settlement_pending_is_visible_without_claiming_material_needs_deletion() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        let scope = scope();
        let record = locked
            .backup_with_reservation_in_epoch(
                desk_file_recovery::BackupRequest {
                    scope: scope.clone(),
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
        let oldest = locked
            .oldest_pending_record(Some(&scope), Some("c"))
            .unwrap();
        assert_eq!(oldest.id, record.id);
        let projected = project_record(&locked, oldest, 1001);
        assert_eq!(
            projected.cleanup_reason.as_deref(),
            Some("quota_settlement_pending")
        );
        assert!(!projected.cleanup_pending);
        assert!(projected.export_available);
        locked
            .acknowledge_quota_settlement(&scope, &record.id)
            .unwrap();
        assert!(
            locked
                .oldest_pending_record(Some(&scope), Some("c"))
                .is_none()
        );
        let projected = project_record(&locked, locked.list(&scope, Some("c"))[0].clone(), 1001);
        assert!(projected.cleanup_reason.is_none());
        assert!(projected.export_available);
    }
    #[test]
    fn export_failures_keep_material_categories() {
        for (source, expected) in [
            (
                desk_file_recovery::ExportError::Expired,
                FileRecoveryFailure::MaterialExpired,
            ),
            (
                desk_file_recovery::ExportError::Cleaning,
                FileRecoveryFailure::MaterialCleaning,
            ),
            (
                desk_file_recovery::ExportError::Cleaned,
                FileRecoveryFailure::MaterialCleaned,
            ),
            (
                desk_file_recovery::ExportError::Preparing,
                FileRecoveryFailure::Busy,
            ),
            (
                desk_file_recovery::ExportError::Unavailable,
                FileRecoveryFailure::MaterialUnavailable,
            ),
        ] {
            assert_eq!(
                storage_failure(io::Error::new(io::ErrorKind::NotFound, source)),
                expected
            );
        }
    }
    #[test]
    fn remote_recovery_keeps_old_authorities_and_binds_exports_to_conversations() {
        let root = tempfile::tempdir().unwrap();
        let vault = desk_file_recovery::Vault::open(root.path()).unwrap();
        let mut v = vault.lock().unwrap();
        let s = scope();
        let saved = v
            .backup(desk_file_recovery::BackupRequest {
                scope: s.clone(),
                conversation: "conversation",
                operation: "op",
                generation: "g",
                file_name: "notes.txt",
                content: b"before",
                metadata: b"{}",
                now_ms: 1000,
            })
            .unwrap();
        let wrong_conversation = FileRecoveryCommand::Export {
            recovery_id: saved.id.clone(),
            conversation_id: "other".into(),
        };
        assert!(matches!(
            execute_locked(&mut v, &s, &wrong_conversation, 1001),
            Err(FileRecoveryFailure::MaterialUnavailable)
        ));
        let mut changed = s.clone();
        changed.authority = "b".repeat(64);
        assert!(matches!(
            execute_locked(
                &mut v,
                &changed,
                &FileRecoveryCommand::DeleteConversation {
                    conversation_id: "conversation".into()
                },
                1001
            ),
            Err(FileRecoveryFailure::IdentityChanged)
        ));
        assert!(v.export_package(&s, &saved.id, 1002).is_ok());
        let download = FileRecoveryCommand::Export {
            recovery_id: saved.id.clone(),
            conversation_id: "conversation".into(),
        };
        assert!(matches!(
            execute_locked(&mut v, &s, &download, 1002),
            Ok(FileRecoveryOutcome::Export { .. })
        ));
        assert!(matches!(
            execute_locked(
                &mut v,
                &s,
                &FileRecoveryCommand::DeleteConversation {
                    conversation_id: "conversation".into()
                },
                1003
            ),
            Ok(FileRecoveryOutcome::Deleted { complete: true })
        ));
        assert!(matches!(
            execute_locked(&mut v, &s, &download, 1004),
            Err(FileRecoveryFailure::MaterialUnavailable)
        ));
    }
}
