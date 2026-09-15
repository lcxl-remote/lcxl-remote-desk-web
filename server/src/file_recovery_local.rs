//! Local OS-user backup operations, independent of the HTTP transport.
//!
//! Callers must authenticate the local OS user before opening that user's vault.
//! These operations intentionally span historical authorities; remote device
//! recovery must continue using the scoped executor in file_recovery_service.
use super::local_quota::LocalQuota;
use desk_agent_protocol::file_recovery::{
    FileRecoveryCleanupDto, FileRecoveryPageDto, FileRecoveryPolicyDto,
};
use desk_file_recovery::LockedVault;
use desk_ipc_protocol::local_file_recovery::{LocalFileRecoveryCommand, LocalFileRecoveryOutcome};
use std::io;

pub(crate) fn execute_worker(
    root: Option<&std::path::Path>,
    quota: crate::worker::session::QuotaClient,
    request: &desk_ipc_protocol::local_file_recovery::LocalFileRecoveryRequest,
) -> Result<LocalFileRecoveryOutcome, desk_agent_protocol::file_recovery::FileRecoveryFailure> {
    use desk_agent_protocol::file_recovery::FileRecoveryFailure as Failure;
    #[cfg(not(windows))]
    {
        let _ = (root, quota, request);
        Err(Failure::Unsupported)
    }
    #[cfg(windows)]
    {
        use windows::Win32::System::{
            RemoteDesktop::ProcessIdToSessionId, Threading::GetCurrentProcessId,
        };
        let mut session = 0;
        unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }
            .map_err(|_| Failure::IdentityChanged)?;
        if session == 0
            || session != request.session_id
            || super::platform_user::current().map_err(|_| Failure::IdentityChanged)?
                != request.os_user
        {
            return Err(Failure::IdentityChanged);
        }
        let now = now_ms();
        if uuid::Uuid::parse_str(&request.request_id).is_err()
            || request.deadline_unix_ms <= now
            || request.deadline_unix_ms > now.saturating_add(60_000)
        {
            return Err(Failure::InvalidRequest);
        }
        execute(
            root.ok_or(Failure::StorageUnavailable)?,
            || Ok(quota),
            request.command.clone(),
        )
        .map_err(super::storage_failure)
    }
}

pub(crate) fn execute<Q: LocalQuota>(
    root: &std::path::Path,
    quota: impl FnOnce() -> io::Result<Q>,
    command: LocalFileRecoveryCommand,
) -> io::Result<LocalFileRecoveryOutcome> {
    command
        .validate()
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    with_vault(root, quota, move |vault, quota| match command {
        LocalFileRecoveryCommand::Query { after } => {
            query(vault, quota, after.as_deref()).map(LocalFileRecoveryOutcome::Page)
        }
        LocalFileRecoveryCommand::RetryCleanup => {
            cleanup(vault, quota).map(LocalFileRecoveryOutcome::Cleanup)
        }
        LocalFileRecoveryCommand::Discard {
            recovery_id,
            conversation_id,
            confirmed,
        } => discard(vault, quota, &conversation_id, &recovery_id, confirmed)
            .map(LocalFileRecoveryOutcome::Cleanup),
        LocalFileRecoveryCommand::ConfirmClock {
            displayed_time_unix_ms,
            confirmed,
        } => confirm_clock(vault, displayed_time_unix_ms, confirmed)
            .map(LocalFileRecoveryOutcome::Cleanup),
        LocalFileRecoveryCommand::Export { recovery_id } => {
            export(vault, &recovery_id).map(LocalFileRecoveryOutcome::Export)
        }
    })
}

/// Open only the authenticated process user's private vault. The quota factory
/// runs after the vault lock, preserving the lock order used by file mutations.
/// A service worker supplies its quota RPC client; the SYSTEM daemon must never
/// call this with an interactive user's path in place of OS-user authorization.
pub(crate) fn with_vault<T, Q: LocalQuota>(
    root: &std::path::Path,
    quota: impl FnOnce() -> io::Result<Q>,
    operation: impl FnOnce(&mut LockedVault, &mut Q) -> io::Result<T>,
) -> io::Result<T> {
    let os_user = super::platform_user::current()?;
    let vault = desk_file_recovery::Vault::open(root)?;
    let mut locked = vault.try_lock()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "File backup storage is busy; retry after the active operation finishes",
        )
    })?;
    let mut quota = quota()?;
    locked.observe_system_clock()?;
    let (policy, _, _) = quota.usage()?;
    if locked.policy() != &policy {
        locked.set_policy(policy)?;
    }
    locked.maintain_epoch_indexes(&os_user, &mut quota, 64)?;
    operation(&mut locked, &mut quota)
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(1) as u64
}

fn policy(vault: &desk_file_recovery::LockedVault) -> FileRecoveryPolicyDto {
    FileRecoveryPolicyDto {
        retention_days: vault.policy().retention_days,
        max_bytes: vault.policy().max_bytes,
    }
}

pub(crate) fn query(
    vault: &mut LockedVault,
    quota: &mut impl LocalQuota,
    after: Option<&str>,
) -> io::Result<FileRecoveryPageDto> {
    let (_, used_bytes, reserved_bytes) = quota.usage()?;
    let mut records = vault.local_records(after)?;
    let more = records.len() > 100;
    records.truncate(100);
    let next_cursor = more.then(|| records.last().unwrap().id.clone());
    Ok(FileRecoveryPageDto {
        execution_epoch: vault.execution_epoch(),
        cleanup_warning: vault
            .cleanup_clock_paused()
            .then_some(desk_agent_protocol::file_recovery::FileRecoveryFailure::ClockChanged),
        clock_confirmation_time_unix_ms: vault.cleanup_clock_paused().then(now_ms),
        oldest_pending_at_unix_ms: vault.oldest_pending_created_at(None),
        oldest_pending_record: vault
            .oldest_pending_record(None, None)
            .map(|record| crate::file_recovery_service::project_record(vault, record, now_ms())),
        policy: policy(vault),
        used_bytes,
        reserved_bytes,
        next_cursor,
        records: records
            .into_iter()
            .map(|r| crate::file_recovery_service::project_record(vault, r, now_ms()))
            .collect(),
    })
}

pub(crate) fn cleanup(
    vault: &mut LockedVault,
    quota: &mut impl LocalQuota,
) -> io::Result<FileRecoveryCleanupDto> {
    let os_user = crate::file_recovery_service::platform_user::current()?;
    let unknown = vault.recover_interrupted(now_ms())?;
    for record in vault.pending_quota_settlement().into_iter().take(32) {
        quota.settle_record(&record)?;
        vault.acknowledge_quota_settlement(&record.scope, &record.id)?;
    }
    for record in vault.pending_quota_cleanup().into_iter().take(32) {
        quota.release_record(&record)?;
        vault.acknowledge_quota_cleanup(&record.scope, &record.id)?;
    }
    for namespace in vault.pending_quota_namespaces().into_iter().take(32) {
        quota.release_user_namespace(&namespace, &os_user, vault.namespace_epoch(&namespace)?)?;
        vault.acknowledge_quota_namespace(&namespace)?;
    }
    vault.maintain_epoch_indexes(&os_user, quota, 1)?;
    Ok(FileRecoveryCleanupDto {
        pending_files: vault.pending_cleanup_count(),
        unknown_outcomes: unknown.len() as u64,
    })
}

pub(crate) fn discard(
    vault: &mut LockedVault,
    quota: &mut impl LocalQuota,
    conversation_id: &str,
    recovery_id: &str,
    confirmed: bool,
) -> io::Result<FileRecoveryCleanupDto> {
    if !confirmed {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Backup discard requires confirmation",
        ));
    }
    vault.discard_local(conversation_id, recovery_id, now_ms())?;
    for record in vault
        .pending_quota_cleanup()
        .into_iter()
        .filter(|record| record.id == recovery_id)
    {
        quota.release_record(&record)?;
        vault.acknowledge_quota_cleanup(&record.scope, &record.id)?;
    }
    Ok(FileRecoveryCleanupDto {
        pending_files: vault.pending_cleanup_count(),
        unknown_outcomes: vault.unknown_outcome_count(None),
    })
}

pub(crate) fn confirm_clock(
    vault: &mut LockedVault,
    displayed_time_unix_ms: u64,
    confirmed: bool,
) -> io::Result<FileRecoveryCleanupDto> {
    if !confirmed || displayed_time_unix_ms == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Clock confirmation is required",
        ));
    }
    vault.acknowledge_clock(displayed_time_unix_ms)?;
    Ok(FileRecoveryCleanupDto {
        pending_files: vault.pending_cleanup_count(),
        unknown_outcomes: vault.unknown_outcome_count(None),
    })
}

pub(crate) fn export(vault: &mut LockedVault, recovery_id: &str) -> io::Result<Vec<u8>> {
    vault.export_local_package(recovery_id, now_ms())
}
