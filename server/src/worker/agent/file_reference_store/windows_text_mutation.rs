//! Windows execution of the shared exact text-change contract.
pub(crate) use super::text_recovery_context::RecoveryContext;
use super::*;
use desk_agent_protocol::computer_use::{
    CreatedFileArtifactOutput, FilePatchAction, FileRecoveryDescriptor, TextFileChange,
    TextFileMutationOperation, TextFileMutationOutput,
};
use desk_file_recovery::{
    BackupRequest, ChangeState, Vault, WindowsCommitRequest,
    windows::{FileKind, MUTATION_DIRECTORY_ACCESS, MoveOutcome, SourceSnapshot, file_identity},
};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

fn conflict() -> AgentError {
    error(
        AgentErrorKind::PermissionDenied,
        "Selected file or backup context changed; read again and request approval",
        false,
    )
}
fn now() -> u64 {
    Utc::now().timestamp_millis().max(1) as u64
}

// Adding children and flushing the directory do not require changing its
// attributes or extended attributes. Public directories may deny those rights
// while permitting safe replacement of user-owned files.
fn open_mutation_parent(path: &Path) -> Result<OpenedFile, AgentError> {
    let parent = open_verified_with_access(
        path,
        MUTATION_DIRECTORY_ACCESS.0,
        (FILE_SHARE_READ | FILE_SHARE_WRITE).0,
    )
    .map_err(|mut cause| {
        cause.message = format!(
            "open parent directory for recoverable file replacement (read/create children): {}",
            cause.message
        );
        cause
    })?;
    // Fail before backup or any rename if this filesystem cannot flush the held
    // handle. Commit still flushes after the moves and retains unknown outcomes.
    parent.handle.sync_all().map_err(|cause| {
        io_error(
            "flush parent directory before recoverable file replacement",
            cause,
        )
    })?;
    Ok(parent)
}

pub(crate) fn execute(
    target: &ObjectRef,
    action: &FilePatchAction,
    context: &RecoveryContext,
    commit_guard: impl Fn() -> Result<(), AgentError>,
) -> Result<(TextFileMutationOutput, &'static str), AgentError> {
    action.validate_text_mutation().map_err(|_| conflict())?;
    let (directory_ref, expected, operation) = match action {
        FilePatchAction::UpdateText {
            directory,
            expected_sha256,
            ..
        } => (
            directory,
            expected_sha256,
            TextFileMutationOperation::Update,
        ),
        FilePatchAction::DeleteText {
            directory,
            expected_sha256,
        } => (
            directory,
            expected_sha256,
            TextFileMutationOperation::Delete,
        ),
        _ => return Err(conflict()),
    };
    if target.object_kind != ObjectKind::File
        || directory_ref.object_kind != ObjectKind::Directory
        || crate::file_recovery_service::platform_user::current().map_err(|_| conflict())?
            != context.scope.os_user
    {
        return Err(conflict());
    }
    commit_guard()?;
    let root = resolve(directory_ref)?;
    let selected = resolve(target)?;
    let (parent_anchor, _ancestors, parent_id) =
        windows_path_anchor::open_anchored(&root.path, FileKind::Directory)?;
    let (selected_parent, _selected_ancestors, selected_parent_id) =
        windows_path_anchor::open_anchored(
            selected.path.parent().ok_or_else(conflict)?,
            FileKind::Directory,
        )?;
    if parent_anchor.identity != root.identity || parent_id != selected_parent_id {
        return Err(conflict());
    }
    drop(selected_parent);
    let parent = open_mutation_parent(&root.path)?;
    if file_identity(&parent.handle, FileKind::Directory).map_err(|_| conflict())? != parent_id {
        return Err(conflict());
    }
    let opened = open_verified(&selected.path)?;
    if opened.identity != selected.identity {
        return Err(conflict());
    }
    let expected_id = file_identity(&opened.handle, FileKind::File).map_err(|_| conflict())?;
    let name = selected
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(conflict)?
        .to_owned();
    let source = SourceSnapshot::capture(&parent.handle, &name, expected_id)
        .map_err(|cause| io_error("capture recoverable text source", cause))?;
    drop(opened);
    if source.content_sha256() != *expected {
        return Err(conflict());
    }
    let content = match action {
        FilePatchAction::UpdateText {
            change: TextFileChange::ReplaceAll { content_utf8 },
            ..
        } => Some(content_utf8.as_bytes().to_vec()),
        FilePatchAction::UpdateText {
            change: TextFileChange::ReplaceOnce { before, after },
            ..
        } => {
            let text = std::str::from_utf8(source.content()).map_err(|_| conflict())?;
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
        FilePatchAction::DeleteText { .. } => None,
        _ => return Err(conflict()),
    };
    if content
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_TEXT_READ_BYTES as usize || bytes.contains(&0))
    {
        return Err(conflict());
    }
    let quota = context.quota.as_ref().ok_or_else(conflict)?;
    let vault =
        Vault::open(&context.data_root).map_err(|e| io_error("open private file backups", e))?;
    let mut vault = vault
        .lock()
        .map_err(|e| io_error("lock private file backups", e))?;
    vault
        .observe_system_clock()
        .map_err(|e| io_error("observe backup clock", e))?;
    vault
        .maintain_epoch_indexes(&context.scope.os_user, &mut quota.clone(), usize::MAX)
        .map_err(|e| io_error("resume backup index maintenance", e))?;
    let (policy, _, _) = quota
        .policy()
        .map_err(|e| io_error("read backup quota", e))?;
    if vault.policy() != &policy {
        vault
            .set_policy(policy)
            .map_err(|e| io_error("apply backup policy", e))?;
    }
    vault
        .require_execution_epoch(context.execution_epoch)
        .map_err(|_| conflict())?;
    vault
        .cleanup(now())
        .map_err(|e| io_error("maintain private file backups", e))?;
    let metadata = source
        .metadata()
        .map_err(|e| io_error("capture file metadata", e))?;
    let saved = vault
        .backup_with_reservation_in_epoch(
            BackupRequest {
                scope: context.scope.clone(),
                conversation: &context.conversation_id,
                operation: &context.operation_id,
                generation: &context.generation,
                file_name: &name,
                content: source.content(),
                metadata: &metadata,
                now_ms: now(),
            },
            context.execution_epoch,
            |record| quota.reserve(record),
        )
        .map_err(|e| io_error("save private file backup", e))?;
    let size = source.content().len() as u64;
    let mut attempted = false;
    let result = (|| -> Result<(bool, Option<CreatedFileArtifactOutput>), AgentError> {
        vault
            .plan_windows_source_transaction(
                &context.scope,
                &saved.id,
                &root.path,
                &parent.handle,
                &source,
                now(),
            )
            .map_err(|e| io_error("register source transaction", e))?;
        let directory = vault
            .create_windows_transaction_directory(&context.scope, &saved.id, &parent.handle)
            .map_err(|e| io_error("create private transaction", e))?;
        let staged = content
            .as_ref()
            .map(|bytes| {
                source.prepare_replacement(&directory, bytes, |file| {
                    vault.register_windows_transaction(
                        &context.scope,
                        &saved.id,
                        &parent.handle,
                        directory.handle(),
                        Some(file),
                    )
                })
            })
            .transpose()
            .map_err(|e| io_error("prepare text replacement", e))?;
        let outcome = vault.commit_windows_transaction(
            WindowsCommitRequest {
                scope: &context.scope,
                id: &saved.id,
                parent: &parent.handle,
                directory: &directory,
                source: &source,
                replacement: staged.as_ref(),
                now_ms: now(),
            },
            || commit_guard().map_err(|e| std::io::Error::other(e.message)),
        );
        match outcome {
            Err(cause) if cause.outcome == MoveOutcome::NotStarted => {
                return Err(io_error(
                    "text change did not start",
                    std::io::Error::other(cause),
                ));
            }
            Err(_) => {
                attempted = true;
                return Ok((false, None));
            }
            Ok(()) => attempted = true,
        }
        let updated = if let Some(staged) = &staged {
            let opened = open_verified(&selected.path)?;
            if file_identity(&opened.handle, FileKind::File).map_err(|_| conflict())?
                != staged.identity()
            {
                return Ok((false, None));
            }
            let file = issue_opened_with_lifetime(
                &selected.path,
                opened,
                DURABLE_ARTIFACT_REF_TTL_SECS,
                true,
            )?;
            let digest = staged.content_sha256();
            Some(CreatedFileArtifactOutput {
                content: desk_agent_protocol::data_lineage::ContentRef::Artifact {
                    artifact_id: file.token.clone(),
                    sha256: digest.clone(),
                    size_bytes: staged.content().len() as u64,
                    media_type: "text/plain;charset=utf-8".into(),
                },
                file,
                file_name: name.clone(),
                media_type: "text/plain;charset=utf-8".into(),
                size_bytes: staged.content().len() as u64,
                digest_sha256: digest,
            })
        } else {
            None
        };
        Ok((true, updated))
    })();
    drop(source);
    if !attempted {
        let _ = vault.transition(&context.scope, &saved.id, ChangeState::Aborted);
    }
    let settled = vault
        .settle_transaction(&context.scope, &saved.id)
        .unwrap_or(false);
    quota.reconcile_settlements(&mut vault);
    // Reference issuance or maintenance after native commit cannot be reported
    // as a definitely failed/no-effect operation.
    let (verified, updated_file) = match result {
        Ok(result) => result,
        Err(_) if attempted => (false, None),
        Err(error) => return Err(error),
    };
    Ok((
        TextFileMutationOutput {
            operation,
            original: target.clone(),
            original_file_name: name,
            original_size_bytes: size,
            original_sha256: expected.clone(),
            recovery: FileRecoveryDescriptor {
                recovery_id: saved.id,
                created_at_unix_ms: saved.created_at_ms,
                expires_at_unix_ms: saved.expires_at_ms,
                cleanup_pending: !settled,
            },
            verified,
            updated_file,
        },
        if verified {
            "File change completed; the prior version is available in device-private recovery storage"
        } else {
            "File change outcome is unknown; recovery material has been retained and the operation will not be repeated"
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_file_recovery::windows::{PrivateDirectory, move_no_replace};
    use std::process::Command;
    use windows::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_GENERIC_WRITE};

    #[test]
    fn replacement_and_delete_without_parent_attribute_write() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("hello.py");
        std::fs::write(&path, b"before").unwrap();
        let sid = desk_file_recovery::windows::current_user_sid().unwrap();
        // No inheritance flags: only this temporary directory denies attribute
        // writes; its files remain writable, matching the Public directory case.
        let result = Command::new("icacls.exe")
            .arg(root.path())
            .arg("/deny")
            .arg(format!("*{sid}:(WA,WEA)"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            open_verified_with_access(
                root.path(),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                (FILE_SHARE_READ | FILE_SHARE_WRITE).0
            )
            .is_err()
        );
        let parent = open_mutation_parent(root.path()).unwrap();
        let parent_id = file_identity(&parent.handle, FileKind::Directory).unwrap();
        let original_id = file_identity(&File::open(&path).unwrap(), FileKind::File).unwrap();
        let source = SourceSnapshot::capture(&parent.handle, "hello.py", original_id).unwrap();
        let tx = PrivateDirectory::open_or_create(&parent.handle, "transaction").unwrap();
        let staged = source
            .prepare_replacement(&tx, b"after", |_| Ok(()))
            .unwrap();
        staged.verify_for_source(&source).unwrap();
        let tx_id = file_identity(tx.handle(), FileKind::Directory).unwrap();
        move_no_replace(
            source.handle(),
            source.identity(),
            tx.handle(),
            tx_id,
            "original",
        )
        .unwrap();
        tx.handle().sync_all().unwrap();
        move_no_replace(
            staged.handle(),
            staged.identity(),
            &parent.handle,
            parent_id,
            "hello.py",
        )
        .unwrap();
        source.finish_replacement_metadata(&staged).unwrap();
        parent.handle.sync_all().unwrap();
        assert_eq!(staged.content(), b"after");
        assert_eq!(source.content(), b"before");
        drop((source, staged, tx));
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        let id = file_identity(&File::open(&path).unwrap(), FileKind::File).unwrap();
        let source = SourceSnapshot::capture(&parent.handle, "hello.py", id).unwrap();
        let tx = PrivateDirectory::open_or_create(&parent.handle, "deletion").unwrap();
        move_no_replace(
            source.handle(),
            source.identity(),
            tx.handle(),
            file_identity(tx.handle(), FileKind::Directory).unwrap(),
            "original",
        )
        .unwrap();
        parent.handle.sync_all().unwrap();
        assert!(!path.exists());
        assert_eq!(source.content(), b"after");
    }
}
