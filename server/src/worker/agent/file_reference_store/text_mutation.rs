//! Handle-relative macOS text changes with retained recovery material.
//!
//! Successful filesystem calls determine the mutation result. References are
//! issued from the staged handle, without rereading either side of the commit.
//! Authorization and expected-version checks happen before the commit.
use super::*;
use std::ffi::{CStr, CString};
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
        directory,
        target,
        expected,
        change,
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

fn create_private_file(parent: &File, name: &CStr, bytes: &[u8]) -> Result<File, AgentError> {
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
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|cause| io_error("write text recovery file", cause))?;
    Ok(file)
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
        directory,
        target,
        expected_sha256,
        change,
        before_commit,
        after_commit,
        commit_guard,
        &test_context(directory),
    )
}

fn mutate_text_managed(
    directory: &ObjectRef,
    target: &ObjectRef,
    expected_sha256: &str,
    change: TextChange<'_>,
    before_commit: impl FnOnce(),
    after_commit: impl FnOnce(),
    commit_guard: impl FnOnce() -> Result<(), AgentError>,
    recovery_context: &RecoveryContext,
) -> Result<TextMutationReceipt, AgentError> {
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
    validate_macos_leaf(file_name)?;
    let leaf = CString::new(file_name).map_err(|_| conflict())?;
    let file = open_relative_unix(&parent.handle, &leaf).map_err(|_| conflict())?;
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
            Some(super::text_replace::replace_once(text, before, after)?)
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
        desk_file_recovery::macos::capture_metadata(&file, &source.path.to_string_lossy())
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
    let mut committed = false;
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
        let recovery = open_relative_unix(&parent.handle, &recovery_name)
            .map_err(|e| io_error("open file transaction directory", e))?;
        desk_file_recovery::macos::make_private(&recovery)
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
        let staged = replacement
            .as_ref()
            .map(|bytes| create_private_file(&recovery, stage_name, bytes))
            .transpose()?;
        if let Some(staged) = &staged {
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
            // Preserve permissions, ACLs and xattrs through open descriptors, not a
            // second pathname copy. In particular, do not drop quarantine metadata.
            if unsafe {
                libc::fcopyfile(
                    file.as_raw_fd(),
                    staged.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            } != 0
            {
                return Err(io_error(
                    "preserve text file metadata",
                    std::io::Error::last_os_error(),
                ));
            }
            staged
                .set_modified(std::time::SystemTime::now())
                .and_then(|_| staged.sync_all())
                .map_err(|cause| io_error("sync text metadata", cause))?;
        }
        before_commit();
        let current = open_relative_unix(&parent.handle, &leaf).map_err(|_| conflict())?;
        if unix_file_identity(&current).map_err(|_| conflict())? != source.identity
            || digest(&complete_text(&current)?) != expected_sha256
            || open_verified(&root.path)?.identity != parent.identity
        {
            return Err(conflict());
        }
        let (destination, flags) = if staged.is_some() {
            (stage_name, libc::RENAME_SWAP)
        } else {
            (original_name, libc::RENAME_EXCL)
        };
        commit_guard()?;
        vault
            .transition(
                &recovery_context.scope,
                &saved.id,
                desk_file_recovery::ChangeState::CommitIntent,
            )
            .map_err(|e| io_error("save file commit intent", e))?;
        if unsafe {
            libc::renameatx_np(
                parent.handle.as_raw_fd(),
                leaf.as_ptr(),
                recovery.as_raw_fd(),
                destination.as_ptr(),
                flags,
            )
        } != 0
        {
            return Err(io_error(
                "commit recoverable text change",
                std::io::Error::last_os_error(),
            ));
        }
        committed = true;
        if let Err(e) = vault.transition(
            &recovery_context.scope,
            &saved.id,
            desk_file_recovery::ChangeState::Succeeded,
        ) {
            tracing::warn!(error_kind = ?e.kind(), "file commit succeeded but its journal needs recovery");
        }
        // Reference registration and maintenance cannot undo a successful syscall.
        // Metadata from the already-open staged handle identifies a future read;
        // it is not compared with the target to verify this mutation.
        let updated = replacement.as_ref().and_then(|bytes| {
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
            verified: true,
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
            message: if staged.is_some() {
                "text update succeeded; prior version saved in device-private backup storage; no post-write readback was performed"
            } else {
                "selected file deleted; prior version saved in device-private backup storage"
            },
        })
    })();
    if !committed {
        let _ = vault.transition(
            &recovery_context.scope,
            &saved.id,
            desk_file_recovery::ChangeState::Aborted,
        );
    }
    let settled = vault
        .settle_transaction(&recovery_context.scope, &saved.id)
        .unwrap_or(false);
    if let Some(quota) = &recovery_context.quota {
        quota.reconcile_settlements(&mut vault);
    }
    result.map(|mut receipt| {
        receipt.recovery.cleanup_pending = !settled;
        receipt
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_recovery_epoch_stops_before_backup_or_file_commit() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        let directory = issue(root.path()).unwrap();
        let file = issue(&path).unwrap();
        let mut context = test_context(&directory);
        context.execution_epoch = 1;
        let result = mutate_text_managed(
            &directory,
            &file,
            &digest(b"old"),
            TextChange::ReplaceAll("new"),
            || panic!("must not reach commit"),
            || {},
            || Ok(()),
            &context,
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
        assert!(vault.lock().unwrap().list(&context.scope, None).is_empty());
    }

    #[test]
    #[ignore = "requires RECOVERY_CROSS_VOLUME_ROOT on a different volume from the system temporary directory"]
    fn native_cross_volume_update_and_delete_export_old_versions() {
        use std::io::{Read, Seek, SeekFrom, Write};
        let _guard = file_store_test_lock();
        let base =
            std::env::var("RECOVERY_CROSS_VOLUME_ROOT").expect("cross-volume target root required");
        for deleting in [false, true] {
            let root = tempfile::tempdir_in(&base).unwrap();
            let path = root.path().join("notes.txt");
            std::fs::write(&path, b"original-content").unwrap();
            let mut old_handle = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let directory = issue(root.path()).unwrap();
            let target = issue(&path).unwrap();
            let context = test_context(&directory);
            assert_ne!(
                std::fs::metadata(root.path()).unwrap().dev(),
                std::fs::metadata(&context.data_root).unwrap().dev(),
                "must exercise two actual volumes"
            );
            let change = if deleting {
                TextChange::Trash
            } else {
                TextChange::ReplaceAll("replacement")
            };
            let receipt = mutate_text_managed(
                &directory,
                &target,
                &digest(b"original-content"),
                change,
                || {},
                || {},
                || Ok(()),
                &context,
            )
            .unwrap();
            assert!(receipt.verified);
            assert!(!receipt.recovery.cleanup_pending);
            assert_eq!(
                std::fs::read_dir(root.path()).unwrap().count(),
                usize::from(!deleting)
            );
            if deleting {
                assert!(!path.exists());
            } else {
                assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
            }
            // A writer retaining the old inode cannot change the private backup.
            old_handle.seek(SeekFrom::Start(0)).unwrap();
            old_handle.write_all(b"external-writer!").unwrap();
            old_handle.sync_all().unwrap();
            assert_eq!(
                std::fs::read(&receipt.backup_path).unwrap(),
                b"original-content"
            );
            let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
            let locked = vault.lock().unwrap();
            let package = locked
                .export_package(
                    &context.scope,
                    &receipt.recovery.recovery_id,
                    Utc::now().timestamp_millis() as u64,
                )
                .unwrap();
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(package)).unwrap();
            let mut text = String::new();
            archive
                .by_name("before.txt")
                .unwrap()
                .read_to_string(&mut text)
                .unwrap();
            assert_eq!(text, "original-content");
            assert_eq!(archive.len(), 2);
            if !deleting {
                assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
            }
        }
    }

    #[test]
    #[ignore = "requires disposable DYLD interposition runner from pocs/poc-file-recovery-storage"]
    fn injected_backup_io_failure_preserves_native_target() {
        let _guard = file_store_test_lock();
        let base = std::env::var("RECOVERY_FAULT_ROOT").expect("fault root required");
        for deleting in [false, true] {
            let root = tempfile::tempdir_in(&base).unwrap();
            let data = tempfile::tempdir_in(&base).unwrap();
            let path = root.path().join("notes.txt");
            std::fs::write(&path, b"original-content").unwrap();
            let directory = issue(root.path()).unwrap();
            let target = issue(&path).unwrap();
            let mut context = test_context(&directory);
            context.data_root = data.path().to_owned();
            let change = if deleting {
                TextChange::Trash
            } else {
                TextChange::ReplaceAll("replacement")
            };
            let result = mutate_text_managed(
                &directory,
                &target,
                &digest(b"original-content"),
                change,
                || panic!("failed backup must not reach commit preparation"),
                || panic!("failed backup must not commit"),
                || Ok(()),
                &context,
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"original-content");
            assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
            let vault = desk_file_recovery::Vault::open(data.path()).unwrap();
            let mut locked = vault.lock().unwrap();
            let rows = locked.list(&context.scope, Some(&context.conversation_id));
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].change, desk_file_recovery::ChangeState::Aborted);
            assert_ne!(rows[0].material, desk_file_recovery::MaterialState::Saved);
            locked
                .recover_interrupted(Utc::now().timestamp_millis() as u64)
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"original-content");
        }
    }

    #[test]
    fn backup_body_or_metadata_write_failure_blocks_update_and_delete() {
        let _guard = file_store_test_lock();
        for suffix in ["body", "metadata"] {
            for deleting in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let path = root.path().join("notes.txt");
                std::fs::write(&path, "old").unwrap();
                let directory = issue(root.path()).unwrap();
                let file = issue(&path).unwrap();
                let context = test_context(&directory);
                let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
                let id = digest(
                    &serde_json::to_vec(&(
                        &context.scope,
                        &context.conversation_id,
                        &context.operation_id,
                    ))
                    .unwrap(),
                );
                std::fs::create_dir(
                    context
                        .data_root
                        .join("file-recovery")
                        .join(format!("{id}.{suffix}")),
                )
                .unwrap();
                let change = if deleting {
                    TextChange::Trash
                } else {
                    TextChange::ReplaceAll("new")
                };
                let result = mutate_text_managed(
                    &directory,
                    &file,
                    &digest(b"old"),
                    change,
                    || panic!("backup failure must stop before commit preparation"),
                    || panic!("backup failure must not commit"),
                    || Ok(()),
                    &context,
                );
                assert!(result.is_err());
                assert_eq!(std::fs::read(&path).unwrap(), b"old");
                assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
                let records = vault
                    .lock()
                    .unwrap()
                    .list(&context.scope, Some(&context.conversation_id));
                assert_eq!(records.len(), 1);
                assert_eq!(records[0].change, desk_file_recovery::ChangeState::Aborted);
                assert_ne!(
                    records[0].material,
                    desk_file_recovery::MaterialState::Saved
                );
            }
        }
    }

    #[test]
    fn commit_intent_write_failure_keeps_original_for_update_and_delete() {
        let _guard = file_store_test_lock();
        for deleting in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("notes.txt");
            std::fs::write(&path, "old").unwrap();
            let directory = issue(root.path()).unwrap();
            let file = issue(&path).unwrap();
            let context = test_context(&directory);
            let index = context.data_root.join("file-recovery/index.json");
            let held = context.data_root.join("held-index.json");
            let change = if deleting {
                TextChange::Trash
            } else {
                TextChange::ReplaceAll("new")
            };
            let result = mutate_text_managed(
                &directory,
                &file,
                &digest(b"old"),
                change,
                || {
                    std::fs::rename(&index, &held).unwrap();
                    std::fs::create_dir(&index).unwrap();
                },
                || panic!("commit intent failure must not commit"),
                || Ok(()),
                &context,
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), b"old");
            std::fs::remove_dir(&index).unwrap();
            std::fs::rename(&held, &index).unwrap();
            let vault = desk_file_recovery::Vault::open(&context.data_root).unwrap();
            let mut locked = vault.lock().unwrap();
            let records = locked.list(&context.scope, Some(&context.conversation_id));
            assert_eq!(records.len(), 1);
            assert_eq!(
                locked
                    .export(&context.scope, &records[0].id, records[0].created_at_ms)
                    .unwrap()
                    .1,
                b"old"
            );
            locked
                .recover_interrupted(Utc::now().timestamp_millis() as u64)
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"old");
        }
    }

    #[test]
    fn update_and_recoverable_delete_preserve_original_bytes() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "你好 old").unwrap();
        let directory = issue(root.path()).unwrap();
        let file = issue(&path).unwrap();
        let updated = mutate_text(
            &directory,
            &file,
            &digest("你好 old".as_bytes()),
            TextChange::ReplaceOnce {
                before: "old",
                after: "new",
            },
        )
        .unwrap();
        assert!(updated.verified);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "你好 new");
        assert_eq!(
            std::fs::read_to_string(updated.backup_path).unwrap(),
            "你好 old"
        );
        let created = updated.file.unwrap();
        let deleted = mutate_text(
            &directory,
            &created.file,
            &created.sha256,
            TextChange::Trash,
        )
        .unwrap();
        assert!(deleted.verified);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(deleted.backup_path).unwrap(),
            "你好 new"
        );
    }

    #[test]
    fn conflicts_links_and_ambiguous_replacements_do_not_overwrite() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "repeat repeat").unwrap();
        let directory = issue(root.path()).unwrap();
        let file = issue(&path).unwrap();
        let hash = digest(b"repeat repeat");
        assert!(
            mutate_text(
                &directory,
                &file,
                &hash,
                TextChange::ReplaceOnce {
                    before: "repeat",
                    after: "new"
                }
            )
            .is_err()
        );
        let conflict = mutate_text_inner(
            &directory,
            &file,
            &hash,
            TextChange::ReplaceAll("new"),
            || {
                std::fs::write(&path, "external change").unwrap();
            },
        );
        assert!(conflict.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external change");
        let fresh = issue(&path).unwrap();
        std::fs::hard_link(&path, root.path().join("alias.txt")).unwrap();
        assert!(
            mutate_text(
                &directory,
                &fresh,
                &digest(b"external change"),
                TextChange::Trash
            )
            .is_err()
        );
        assert!(path.exists());
    }

    #[test]
    fn empty_updates_and_utf8_byte_limits_are_exact() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let oversized = "界".repeat(MAX_TEXT_READ_BYTES as usize / 3 + 1);
        assert!(
            mutate_text(
                &directory,
                &target,
                &digest(b"old"),
                TextChange::ReplaceAll(&oversized)
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let receipt = mutate_text(
            &directory,
            &target,
            &digest(b"old"),
            TextChange::ReplaceAll(""),
        )
        .unwrap();
        assert!(receipt.verified);
        assert_eq!(receipt.original_sha256, digest(b"old"));
        let artifact = receipt.file.unwrap();
        assert_eq!(artifact.byte_len, 0);
        assert_eq!(artifact.sha256, digest(b""));
        assert!(std::fs::read(&path).unwrap().is_empty());
        assert!(receipt.message.contains("no post-write readback"));
    }

    #[test]
    fn overlapping_matches_and_binary_text_are_rejected_before_writes() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "aaa").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        assert!(
            mutate_text(
                &directory,
                &target,
                &digest(b"aaa"),
                TextChange::ReplaceOnce {
                    before: "aa",
                    after: "b"
                }
            )
            .is_err()
        );
        assert!(
            mutate_text(
                &directory,
                &target,
                &digest(b"aaa"),
                TextChange::ReplaceAll("binary\0payload")
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"aaa");
    }

    #[test]
    fn replacement_preserves_permissions_and_extended_attributes() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let source = File::open(&path).unwrap();
        let value = b"retained metadata";
        assert_eq!(
            unsafe {
                libc::fsetxattr(
                    source.as_raw_fd(),
                    c"com.lcxl.text-test".as_ptr(),
                    value.as_ptr().cast(),
                    value.len(),
                    0,
                    0,
                )
            },
            0
        );
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let receipt = mutate_text(
            &directory,
            &target,
            &digest(b"old"),
            TextChange::ReplaceAll("new"),
        )
        .unwrap();
        assert!(receipt.verified);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o640);
        let result = File::open(&path).unwrap();
        let mut actual = [0u8; 64];
        let size = unsafe {
            libc::fgetxattr(
                result.as_raw_fd(),
                c"com.lcxl.text-test".as_ptr(),
                actual.as_mut_ptr().cast(),
                actual.len(),
                0,
                0,
            )
        };
        assert_eq!(size, value.len() as isize);
        assert_eq!(&actual[..size as usize], value);
    }

    #[test]
    fn exact_native_dispatch_rechecks_commit_guard_and_returns_typed_recovery() {
        use desk_agent_protocol::computer_use::{FilePatchAction, TextFileChange};
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let action = FilePatchAction::UpdateText {
            directory,
            expected_sha256: digest(b"old"),
            change: TextFileChange::ReplaceAll {
                content_utf8: "new".into(),
            },
        };
        assert!(
            execute(
                &target,
                &action,
                &test_context(&issue(root.path()).unwrap()),
                || Err(conflict())
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let context = test_context(&issue(root.path()).unwrap());
        let (receipt, _) = execute(&target, &action, &context, || Ok(())).unwrap();
        receipt.validate_for(&target, &action).unwrap();
        assert!(receipt.verified);
        assert_eq!(
            std::fs::read(
                context
                    .data_root
                    .join("file-recovery")
                    .join(format!("{}.body", receipt.recovery.recovery_id))
            )
            .unwrap(),
            b"old"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn later_external_write_does_not_reclassify_successful_commit() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "approved old").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let receipt = mutate_text_with_hooks(
            &directory,
            &target,
            &digest(b"approved old"),
            TextChange::ReplaceAll("new"),
            || {},
            || {
                std::fs::write(&path, "external after commit").unwrap();
            },
            || Ok(()),
        )
        .unwrap();
        assert!(receipt.verified);
        assert!(receipt.file.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), b"external after commit");
        assert_eq!(std::fs::read(receipt.backup_path).unwrap(), b"approved old");
        assert!(receipt.message.contains("no post-write readback"));
        assert!(
            mutate_text(
                &directory,
                &receipt.file.unwrap().file,
                &digest(b"new"),
                TextChange::ReplaceAll("later"),
            )
            .is_err()
        );
    }

    #[test]
    fn reference_persistence_failure_does_not_turn_committed_update_into_failure() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        let previous = {
            let mut state = store().lock().unwrap();
            state
                .durable_registry_path
                .replace(path.join("registry.json"))
        };
        let result = mutate_text(
            &directory,
            &target,
            &digest(b"old"),
            TextChange::ReplaceAll("new"),
        );
        store().lock().unwrap().durable_registry_path = previous;
        let receipt = result.unwrap();
        assert!(receipt.verified);
        assert!(receipt.file.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(receipt.backup_path).unwrap(), b"old");
    }

    #[test]
    fn directory_mismatch_and_symlink_replacement_never_mutate_target() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = root.path().join("notes.txt");
        let other = outside.path().join("notes.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::write(&other, "outside").unwrap();
        let directory = issue(root.path()).unwrap();
        let target = issue(&path).unwrap();
        assert!(
            mutate_text(
                &issue(outside.path()).unwrap(),
                &target,
                &digest(b"old"),
                TextChange::Trash
            )
            .is_err()
        );
        let result = mutate_text_inner(
            &directory,
            &target,
            &digest(b"old"),
            TextChange::Trash,
            || {
                std::fs::rename(&path, root.path().join("preserved.txt")).unwrap();
                std::os::unix::fs::symlink(&other, &path).unwrap();
            },
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(&other).unwrap(), b"outside");
        assert_eq!(
            std::fs::read(root.path().join("preserved.txt")).unwrap(),
            b"old"
        );
    }
}
