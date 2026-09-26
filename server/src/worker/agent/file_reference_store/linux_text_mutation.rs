//! Handle-relative Linux text changes with retained recovery material.
//!
//! Submission errors retain the transaction because the outcome may be unknown.
//! Successful submissions are verified before reporting a confirmed change.
//! Authorization and expected-version checks happen before the commit.
use super::*;
use std::ffi::{CStr, CString};
use std::io::{Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

pub(crate) use super::text_recovery_context::RecoveryContext;

#[cfg(test)]
fn test_context(_directory: &ObjectRef) -> RecoveryContext {
    let fixture = std::sync::Arc::new(tempfile::tempdir().unwrap());
    let data_root = fixture.path().to_path_buf();
    RecoveryContext {
        _test_data: Some(fixture),
        execution_epoch: 0,
        data_root,
        quota: None,
        scope: desk_file_recovery::Scope {
            authority: "test".into(),
            device: "device".into(),
            os_user: "test-user".into(),
            owner: "owner".into(),
        },
        conversation_id: "conversation".into(),
        operation_id: uuid::Uuid::new_v4().to_string(),
        generation: "generation".into(),
    }
}

pub(crate) fn execute(
    target: &ObjectRef,
    action: &desk_agent_protocol::computer_use::FilePatchAction,
    recovery_context: &RecoveryContext,
    commit_guard: impl FnOnce() -> Result<(), AgentError>,
) -> Result<
    (
        desk_agent_protocol::computer_use::TextFileMutationOutput,
        &'static str,
    ),
    AgentError,
> {
    use desk_agent_protocol::computer_use::{
        CreatedFileArtifactOutput, FilePatchAction, TextFileChange, TextFileMutationOutput,
    };
    action.validate_text_mutation().map_err(|_| conflict())?;
    let (directory, expected, change) = match action {
        FilePatchAction::UpdateText {
            directory,
            expected_sha256,
            change,
        } => (
            directory,
            expected_sha256,
            match change {
                TextFileChange::ReplaceAll { content_utf8 } => TextChange::ReplaceAll(content_utf8),
                TextFileChange::ReplaceOnce { before, after } => {
                    TextChange::ReplaceOnce { before, after }
                }
            },
        ),
        FilePatchAction::DeleteText {
            directory,
            expected_sha256,
        } => (directory, expected_sha256, TextChange::Trash),
        _ => return Err(conflict()),
    };
    let receipt = mutate_text_managed(
        TextMutationRequest {
            directory,
            target,
            expected_sha256: expected,
            change,
        },
        || {},
        || {},
        commit_guard,
        recovery_context,
    )?;
    let output = TextFileMutationOutput {
        operation: if matches!(action, FilePatchAction::UpdateText { .. }) {
            desk_agent_protocol::computer_use::TextFileMutationOperation::Update
        } else {
            desk_agent_protocol::computer_use::TextFileMutationOperation::Delete
        },
        original: target.clone(),
        original_file_name: receipt.original_file_name,
        original_size_bytes: receipt.original_size_bytes,
        original_sha256: receipt.original_sha256,
        recovery: receipt.recovery,
        verified: receipt.verified,
        updated_file: receipt.file.map(|file| CreatedFileArtifactOutput {
            content: desk_agent_protocol::data_lineage::ContentRef::Artifact {
                artifact_id: file.file.token.clone(),
                sha256: file.sha256.clone(),
                size_bytes: file.byte_len,
                media_type: "text/plain;charset=utf-8".into(),
            },
            file: file.file,
            file_name: file.file_name,
            media_type: "text/plain;charset=utf-8".into(),
            size_bytes: file.byte_len,
            digest_sha256: file.sha256,
        }),
    };
    Ok((output, receipt.message))
}

pub(super) enum TextChange<'a> {
    ReplaceAll(&'a str),
    ReplaceOnce { before: &'a str, after: &'a str },
    Trash,
}

pub(super) struct TextMutationReceipt {
    pub original_file_name: String,
    pub original_size_bytes: u64,
    pub verified: bool,
    pub recovery: desk_agent_protocol::computer_use::FileRecoveryDescriptor,
    #[cfg(test)]
    pub backup_path: PathBuf,
    #[cfg(test)]
    _test_data: Option<std::sync::Arc<tempfile::TempDir>>,
    pub file: Option<CreatedTextArtifact>,
    pub original_sha256: String,
    pub message: &'static str,
}

fn conflict() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "text file identity or complete content version changed; read again and request new approval",
        false,
    )
}

fn complete_text(file: &File) -> Result<Vec<u8>, AgentError> {
    desk_file_recovery::linux::validate_source(file)
        .map_err(|e| io_error("validate Linux text source", e))?;
    let metadata = file.metadata().map_err(|_| conflict())?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_TEXT_READ_BYTES as u64 {
        return Err(conflict());
    }
    let before = unix_file_identity(file).map_err(|_| conflict())?;
    let mut handle = file.try_clone().map_err(|_| conflict())?;
    handle.seek(SeekFrom::Start(0)).map_err(|_| conflict())?;
    let mut bytes = Vec::new();
    handle
        .take(MAX_TEXT_READ_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| conflict())?;
    if bytes.len() > MAX_TEXT_READ_BYTES as usize
        || bytes.contains(&0)
        || std::str::from_utf8(&bytes).is_err()
        || unix_file_identity(file).map_err(|_| conflict())? != before
    {
        return Err(conflict());
    }
    Ok(bytes)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn create_private_file(parent: &File, name: &CStr) -> Result<File, AgentError> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io_error(
            "create private text recovery file",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(test)]
fn mutate_text(
    directory: &ObjectRef,
    target: &ObjectRef,
    expected_sha256: &str,
    change: TextChange<'_>,
) -> Result<TextMutationReceipt, AgentError> {
    mutate_text_inner(directory, target, expected_sha256, change, || {})
}

#[cfg(test)]
fn mutate_text_inner(
    directory: &ObjectRef,
    target: &ObjectRef,
    expected_sha256: &str,
    change: TextChange<'_>,
    before_commit: impl FnOnce(),
) -> Result<TextMutationReceipt, AgentError> {
    mutate_text_with_hooks(
        directory,
        target,
        expected_sha256,
        change,
        before_commit,
        || {},
        || Ok(()),
    )
}

#[cfg(test)]
fn mutate_text_with_hooks(
    directory: &ObjectRef,
    target: &ObjectRef,
    expected_sha256: &str,
    change: TextChange<'_>,
    before_commit: impl FnOnce(),
    after_commit: impl FnOnce(),
    commit_guard: impl FnOnce() -> Result<(), AgentError>,
) -> Result<TextMutationReceipt, AgentError> {
    mutate_text_managed(
        TextMutationRequest {
            directory,
            target,
            expected_sha256,
            change,
        },
        before_commit,
        after_commit,
        commit_guard,
        &test_context(directory),
    )
}

struct TextMutationRequest<'a> {
    directory: &'a ObjectRef,
    target: &'a ObjectRef,
    expected_sha256: &'a str,
    change: TextChange<'a>,
}

fn mutate_text_managed(
    request: TextMutationRequest<'_>,
    before_commit: impl FnOnce(),
    after_commit: impl FnOnce(),
    commit_guard: impl FnOnce() -> Result<(), AgentError>,
    recovery_context: &RecoveryContext,
) -> Result<TextMutationReceipt, AgentError> {
    mutate_text_with_submit(
        request,
        before_commit,
        after_commit,
        commit_guard,
        recovery_context,
        |parent, leaf, recovery, destination, flags| {
            if unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    parent.as_raw_fd(),
                    leaf.as_ptr(),
                    recovery.as_raw_fd(),
                    destination.as_ptr(),
                    flags,
                )
            } == 0
            {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        },
    )
}

fn mutate_text_with_submit(
    request: TextMutationRequest<'_>,
    before_commit: impl FnOnce(),
    after_commit: impl FnOnce(),
    commit_guard: impl FnOnce() -> Result<(), AgentError>,
    recovery_context: &RecoveryContext,
    submit: impl FnOnce(&File, &CStr, &File, &CStr, u32) -> std::io::Result<()>,
) -> Result<TextMutationReceipt, AgentError> {
    let TextMutationRequest {
        directory,
        target,
        expected_sha256,
        change,
    } = request;
    if directory.object_kind != ObjectKind::Directory
        || target.object_kind != ObjectKind::File
        || expected_sha256.len() != 64
        || !expected_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(conflict());
    }
    let root = resolve(directory)?;
    let source = resolve(target)?;
    let parent = open_verified(&root.path)?;
    if !parent.metadata.is_dir()
        || parent.identity != root.identity
        || open_verified(source.path.parent().ok_or_else(conflict)?)?.identity != parent.identity
    {
        return Err(conflict());
    }
    let file_name = source
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(conflict)?;
    if file_name.is_empty()
        || file_name.len() > 200
        || file_name.chars().any(|c| c.is_control() || c == '/')
    {
        return Err(conflict());
    }
    let leaf = CString::new(file_name).map_err(|_| conflict())?;
    let file = desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new(file_name))
        .map_err(|_| conflict())?;
    if unix_file_identity(&file).map_err(|_| conflict())? != source.identity {
        return Err(conflict());
    }
    let original = complete_text(&file)?;
    if digest(&original) != expected_sha256 {
        return Err(conflict());
    }
    let replacement = match change {
        TextChange::Trash => None,
        TextChange::ReplaceAll(text) => Some(text.as_bytes().to_vec()),
        TextChange::ReplaceOnce { before, after } => {
            let text = std::str::from_utf8(&original).map_err(|_| conflict())?;
            // Include overlapping occurrences: "aaa" contains two possible
            // "aa" targets, even though match_indices reports only one.
            if before.is_empty()
                || text
                    .char_indices()
                    .filter(|(offset, _)| text[*offset..].starts_with(before))
                    .take(2)
                    .count()
                    != 1
            {
                return Err(conflict());
            }
            Some(text.replacen(before, after, 1).into_bytes())
        }
    };
    if replacement
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_TEXT_READ_BYTES as usize || bytes.contains(&0))
    {
        return Err(conflict());
    }
    let vault = desk_file_recovery::Vault::open(&recovery_context.data_root)
        .map_err(|e| io_error("open private file backups", e))?;
    let mut vault = vault
        .lock()
        .map_err(|e| io_error("lock private file backups", e))?;
    vault
        .observe_system_clock()
        .map_err(|e| io_error("observe backup clock", e))?;
    if let Some(quota) = &recovery_context.quota {
        vault
            .maintain_epoch_indexes(
                &recovery_context.scope.os_user,
                &mut quota.clone(),
                usize::MAX,
            )
            .map_err(|e| io_error("resume backup index cleanup", e))?;
        let (policy, _, _) = quota
            .policy()
            .map_err(|e| io_error("read device backup policy", e))?;
        if vault.policy() != &policy {
            vault
                .set_policy(policy)
                .map_err(|e| io_error("apply device backup policy", e))?;
        }
    }
    vault
        .require_execution_epoch(recovery_context.execution_epoch)
        .map_err(|e| {
            io_error(
                "validate backup execution epoch; request a new operation",
                e,
            )
        })?;
    vault
        .cleanup(Utc::now().timestamp_millis().max(1) as u64)
        .map_err(|e| io_error("maintain private file backups", e))?;
    let metadata =
        desk_file_recovery::linux::capture_metadata(&file, &source.path.to_string_lossy())
            .map_err(|e| io_error("save original file metadata", e))?;
    let backup_request = desk_file_recovery::BackupRequest {
        scope: recovery_context.scope.clone(),
        conversation: &recovery_context.conversation_id,
        operation: &recovery_context.operation_id,
        generation: &recovery_context.generation,
        file_name,
        content: &original,
        metadata: &metadata,
        now_ms: Utc::now().timestamp_millis().max(1) as u64,
    };
    let saved = match &recovery_context.quota {
        Some(quota) => vault.backup_with_reservation_in_epoch(
            backup_request,
            recovery_context.execution_epoch,
            |record| quota.reserve(record),
        ),
        None => {
            #[cfg(test)]
            {
                vault.backup(backup_request)
            }
            #[cfg(not(test))]
            {
                Err(std::io::Error::other(
                    "device backup quota is unavailable; file was not changed",
                ))
            }
        }
    }
    .map_err(|e| io_error("save private file backup; target not changed", e))?;
    let mut submission_attempted = false;
    let mut verified_commit = false;
    let result = (|| {
        let recovery_name = vault
            .plan_transaction(
                &recovery_context.scope,
                &saved.id,
                &root.path.to_string_lossy(),
                parent.metadata.dev(),
                parent.metadata.ino(),
                file.metadata()
                    .map_err(|e| io_error("identify original file", e))?
                    .ino(),
            )
            .map_err(|e| io_error("register file transaction", e))?;
        let recovery_name = CString::new(recovery_name).map_err(|_| conflict())?;
        if unsafe { libc::mkdirat(parent.handle.as_raw_fd(), recovery_name.as_ptr(), 0o700) } != 0 {
            return Err(io_error(
                "create file transaction directory",
                std::io::Error::last_os_error(),
            ));
        }
        let recovery = desk_file_recovery::linux::open_beneath(
            &parent.handle,
            Path::new(recovery_name.to_str().map_err(|_| conflict())?),
            true,
        )
        .map_err(|e| io_error("open file transaction directory", e))?;
        desk_file_recovery::linux::make_private(&recovery)
            .map_err(|e| io_error("protect file transaction directory", e))?;
        let directory_inode = recovery
            .metadata()
            .map_err(|e| io_error("identify file transaction", e))?
            .ino();
        vault
            .register_transaction(&recovery_context.scope, &saved.id, directory_inode, None)
            .map_err(|e| io_error("register file transaction identity", e))?;
        let original_name = c"original";
        let stage_name = c"replacement";
        let mut staged = replacement
            .as_ref()
            .map(|_| create_private_file(&recovery, stage_name))
            .transpose()?;
        if let (Some(staged), Some(bytes)) = (staged.as_mut(), replacement.as_ref()) {
            vault
                .register_transaction(
                    &recovery_context.scope,
                    &saved.id,
                    directory_inode,
                    Some(
                        staged
                            .metadata()
                            .map_err(|e| io_error("identify staged file", e))?
                            .ino(),
                    ),
                )
                .map_err(|e| io_error("register staged file identity", e))?;
            // Register the inode before writing: a partial write/sync failure
            // must remain identifiable to the guarded transaction cleanup.
            staged
                .write_all(bytes)
                .and_then(|_| staged.sync_all())
                .map_err(|cause| io_error("write text recovery file", cause))?;
            desk_file_recovery::linux::copy_metadata(&file, staged)
                .map_err(|e| io_error("preserve Linux file metadata", e))?;
            staged
                .set_modified(std::time::SystemTime::now())
                .and_then(|_| staged.sync_all())
                .map_err(|cause| io_error("sync text metadata", cause))?;
        }
        before_commit();
        let current =
            desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new(file_name))
                .map_err(|_| conflict())?;
        if unix_file_identity(&current).map_err(|_| conflict())? != source.identity
            || digest(&complete_text(&current)?) != expected_sha256
            || open_verified(&root.path)?.identity != parent.identity
        {
            return Err(conflict());
        }
        let (destination, flags) = if staged.is_some() {
            (stage_name, libc::RENAME_EXCHANGE)
        } else {
            (original_name, libc::RENAME_NOREPLACE)
        };
        recovery
            .sync_all()
            .and_then(|_| parent.handle.sync_all())
            .map_err(|e| io_error("persist Linux transaction directory", e))?;
        vault
            .transition(
                &recovery_context.scope,
                &saved.id,
                desk_file_recovery::ChangeState::CommitIntent,
            )
            .map_err(|e| io_error("save file commit intent", e))?;
        let current =
            desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new(file_name))
                .map_err(|_| conflict())?;
        if unix_file_identity(&current).map_err(|_| conflict())? != source.identity
            || digest(&complete_text(&current)?) != expected_sha256
            || desk_file_recovery::linux::capture_metadata(&current, &source.path.to_string_lossy())
                .map_err(|e| io_error("recheck Linux metadata", e))?
                != metadata
        {
            return Err(conflict());
        }
        commit_guard()?;
        submission_attempted = true;
        if let Err(cause) = submit(&parent.handle, &leaf, &recovery, destination, flags) {
            // A storage error does not prove that neither directory was changed.
            // Keep the durable intent and all recovery identities for reconciliation.
            tracing::warn!(error_kind = ?cause.kind(), "text commit returned without a known outcome");
            return Ok(TextMutationReceipt {
                original_file_name: file_name.into(),
                original_size_bytes: original.len() as u64,
                verified: false,
                recovery: desk_agent_protocol::computer_use::FileRecoveryDescriptor {
                    recovery_id: saved.id.clone(),
                    created_at_unix_ms: saved.created_at_ms,
                    expires_at_unix_ms: saved.expires_at_ms,
                    cleanup_pending: true,
                },
                #[cfg(test)]
                _test_data: recovery_context._test_data.clone(),
                #[cfg(test)]
                backup_path: recovery_context
                    .data_root
                    .join("file-recovery")
                    .join(format!("{}.body", saved.id)),
                file: None,
                original_sha256: expected_sha256.into(),
                message: "text commit outcome is unknown; recovery transaction retained; do not replay",
            });
        }
        let displaced = desk_file_recovery::linux::open_text_beneath(
            &recovery,
            Path::new(destination.to_str().unwrap()),
        );
        let source_matches = displaced.as_ref().ok().is_some_and(|old| {
            old.metadata().is_ok_and(|stat| {
                stat.ino() == source.identity.secondary
                    && stat.dev() == source.identity.primary
                    && stat.nlink() == 1
            }) && complete_text(old).is_ok_and(|bytes| digest(&bytes) == expected_sha256)
        });
        let target_matches = match &replacement {
            Some(bytes) => {
                desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new(file_name))
                    .is_ok_and(|published| {
                        published.metadata().is_ok_and(|stat| {
                            staged.as_ref().unwrap().metadata().is_ok_and(|stage| {
                                stat.ino() == stage.ino() && stat.dev() == stage.dev()
                            })
                        }) && complete_text(&published).is_ok_and(|current| current == *bytes)
                    })
            }
            None => {
                matches!(desk_file_recovery::linux::open_text_beneath(&parent.handle, Path::new(file_name)), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
            }
        };
        verified_commit = source_matches
            && target_matches
            && parent.handle.sync_all().is_ok()
            && recovery.sync_all().is_ok();
        if verified_commit {
            verified_commit = vault
                .transition(
                    &recovery_context.scope,
                    &saved.id,
                    desk_file_recovery::ChangeState::Succeeded,
                )
                .is_ok();
        }
        // An uncertain commit must not publish a reference with the intended digest.
        // Registration failure cannot undo an already verified filesystem change.
        let updated = replacement.as_ref().filter(|_| verified_commit).and_then(|bytes| {
        let reference = (|| {
            let handle = staged.as_ref().unwrap().try_clone()
                .map_err(|cause| io_error("clone updated file handle", cause))?;
            let metadata = handle.metadata()
                .map_err(|cause| io_error("identify updated file", cause))?;
            let identity = unix_file_identity(&handle)
                .map_err(|cause| io_error("identify updated file", cause))?;
            issue_opened_with_lifetime(
                &root.path.join(file_name),
                OpenedFile { handle, metadata, identity },
                DURABLE_ARTIFACT_REF_TTL_SECS,
                true,
            )
        })();
        match reference {
            Ok(reference) => Some(CreatedTextArtifact {
                file: reference,
                file_name: file_name.into(),
                byte_len: bytes.len() as u64,
                sha256: digest(bytes),
            }),
            Err(cause) => {
                tracing::warn!(error_kind = ?cause.kind, "text change succeeded but reference registration failed");
                None
            }
        }
    });
        after_commit();
        for directory in [&parent.handle, &recovery] {
            if let Err(cause) = directory.sync_all() {
                tracing::warn!(error_kind = ?cause.kind(), "text change succeeded but directory sync failed");
            }
        }
        Ok(TextMutationReceipt {
            original_file_name: file_name.into(),
            original_size_bytes: original.len() as u64,
            verified: verified_commit,
            recovery: desk_agent_protocol::computer_use::FileRecoveryDescriptor {
                recovery_id: saved.id.clone(),
                created_at_unix_ms: saved.created_at_ms,
                expires_at_unix_ms: saved.expires_at_ms,
                cleanup_pending: false,
            },
            #[cfg(test)]
            _test_data: recovery_context._test_data.clone(),
            #[cfg(test)]
            backup_path: recovery_context
                .data_root
                .join("file-recovery")
                .join(format!("{}.body", saved.id)),
            file: updated,
            original_sha256: expected_sha256.into(),
            message: if !verified_commit {
                "text operation was submitted but could not be verified; recovery transaction retained; do not replay"
            } else if staged.is_some() {
                "text update verified; prior version saved in device-private backup storage"
            } else {
                "selected file deleted; prior version saved in device-private backup storage"
            },
        })
    })();
    if !submission_attempted {
        let _ = vault.transition(
            &recovery_context.scope,
            &saved.id,
            desk_file_recovery::ChangeState::Aborted,
        );
    }
    let settled = if !submission_attempted || verified_commit {
        vault
            .settle_transaction(&recovery_context.scope, &saved.id)
            .unwrap_or(false)
    } else {
        false
    };
    if let Some(quota) = &recovery_context.quota {
        quota.reconcile_settlements(&mut vault);
    }
    result.map(|mut receipt| {
        receipt.recovery.cleanup_pending = !settled;
        receipt
    })
}

#[cfg(test)]
#[path = "linux_text_mutation/tests.rs"]
mod tests;
