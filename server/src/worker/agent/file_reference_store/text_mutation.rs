//! Handle-relative macOS text changes with retained recovery material.
//!
//! renameatx_np swaps directory entries atomically, not conditionally on a
//! content digest. We check both sides of the commit and never erase the old
//! entry or blindly roll back across an uncooperative writer. A post-commit
//! mismatch is explicitly uncertain, with recovery material retained.
use super::*;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;

pub(crate) fn execute(
    target: &ObjectRef,
    action: &desk_agent_protocol::computer_use::FilePatchAction,
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
    let receipt = mutate_text_with_hooks(
        directory,
        target,
        expected,
        change,
        || {},
        || {},
        commit_guard,
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
        recovery_path: receipt.recovery_path.to_string_lossy().into_owned(),
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
    pub recovery_path: PathBuf,
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

fn mutate_text_with_hooks(
    directory: &ObjectRef,
    target: &ObjectRef,
    expected_sha256: &str,
    change: TextChange<'_>,
    before_commit: impl FnOnce(),
    after_commit: impl FnOnce(),
    commit_guard: impl FnOnce() -> Result<(), AgentError>,
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
    // Recovery is deliberately on the same filesystem, under the approved root.
    // Failure never falls back to unlink/permanent deletion.
    let recovery_name =
        CString::new(format!(".assistant-recovery-{}", uuid::Uuid::new_v4())).unwrap();
    if unsafe { libc::mkdirat(parent.handle.as_raw_fd(), recovery_name.as_ptr(), 0o700) } != 0 {
        return Err(io_error(
            "create text recovery directory",
            std::io::Error::last_os_error(),
        ));
    }
    let recovery = open_relative_unix(&parent.handle, &recovery_name)
        .map_err(|cause| io_error("open text recovery directory", cause))?;
    let recovery_path = root.path.join(recovery_name.to_str().unwrap());
    let saved_name = c"approved-before.txt";
    let original_name = c"original";
    let stage_name = c"replacement";
    // An independent before-image is retained even if another writer still
    // holds an open descriptor to the inode that will be moved here.
    let _saved = create_private_file(&recovery, saved_name, &original)?;
    let staged = replacement
        .as_ref()
        .map(|bytes| create_private_file(&recovery, stage_name, bytes))
        .transpose()?;
    if let Some(staged) = &staged {
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
        let source_metadata = file.metadata().map_err(|_| conflict())?;
        let stage_metadata = staged.metadata().map_err(|_| conflict())?;
        if source_metadata.mode() != stage_metadata.mode()
            || source_metadata.uid() != stage_metadata.uid()
            || source_metadata.gid() != stage_metadata.gid()
        {
            return Err(conflict());
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
    // From this point onward, errors are not evidence of nonexecution. Keep
    // both before-images and report an uncertain commit rather than retrying.
    after_commit();
    let uncertain = || TextMutationReceipt {
        original_file_name: file_name.into(),
        original_size_bytes: original.len() as u64,
        verified: false,
        recovery_path: recovery_path.clone(),
        file: None,
        original_sha256: expected_sha256.into(),
        message: "text change crossed the commit point but verification conflicted; recovery copies were retained; do not retry automatically",
    };
    let old = match open_relative_unix(&recovery, destination) {
        Ok(file) => file,
        Err(_) => return Ok(uncertain()),
    };
    let old_identity = match unix_file_identity(&old) {
        Ok(identity) => identity,
        Err(_) => return Ok(uncertain()),
    };
    if old_identity.primary != source.identity.primary
        || old_identity.secondary != source.identity.secondary
    {
        return Ok(uncertain());
    }
    if complete_text(&old)
        .map(|bytes| digest(&bytes))
        .ok()
        .as_deref()
        != Some(expected_sha256)
    {
        return Ok(uncertain());
    }
    if parent.handle.sync_all().is_err()
        || recovery.sync_all().is_err()
        || open_verified(&root.path).map(|opened| opened.identity).ok()
            != Some(parent.identity.clone())
    {
        return Ok(uncertain());
    }
    let updated = if let Some(bytes) = replacement {
        let published = match open_verified(&root.path.join(file_name)) {
            Ok(file) => file,
            Err(_) => return Ok(uncertain()),
        };
        let staged = staged.as_ref().unwrap();
        let staged_identity = match unix_file_identity(staged) {
            Ok(identity) => identity,
            Err(_) => return Ok(uncertain()),
        };
        if published.identity != staged_identity
            || complete_text(&published.handle).ok().as_deref() != Some(bytes.as_slice())
        {
            return Ok(uncertain());
        }
        let reference = match issue_opened_with_lifetime(
            &root.path.join(file_name),
            published,
            DURABLE_ARTIFACT_REF_TTL_SECS,
            true,
        ) {
            Ok(reference) => reference,
            Err(_) => return Ok(uncertain()),
        };
        Some(CreatedTextArtifact {
            file: reference,
            file_name: file_name.into(),
            byte_len: bytes.len() as u64,
            sha256: digest(&bytes),
        })
    } else {
        None
    };
    Ok(TextMutationReceipt {
        original_file_name: file_name.into(),
        original_size_bytes: original.len() as u64,
        verified: true,
        recovery_path,
        file: updated,
        original_sha256: expected_sha256.into(),
        message: if staged.is_some() {
            "text updated and read back; prior version retained in recovery directory"
        } else {
            "selected file moved to recovery directory; no permanent deletion"
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            std::fs::read_to_string(updated.recovery_path.join("approved-before.txt")).unwrap(),
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
            std::fs::read_to_string(deleted.recovery_path.join("original")).unwrap(),
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
        assert!(receipt.message.contains("read back"));
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
        assert!(execute(&target, &action, || Err(conflict())).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let (receipt, _) = execute(&target, &action, || Ok(())).unwrap();
        receipt.validate_for(&target, &action).unwrap();
        assert!(receipt.verified);
        assert_eq!(
            std::fs::read(Path::new(&receipt.recovery_path).join("approved-before.txt")).unwrap(),
            b"old"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
    }

    #[test]
    fn post_commit_conflict_is_unknown_and_retains_approved_before_image() {
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
        assert!(!receipt.verified);
        assert!(receipt.file.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"external after commit");
        assert_eq!(
            std::fs::read(receipt.recovery_path.join("approved-before.txt")).unwrap(),
            b"approved old"
        );
        assert!(receipt.message.contains("do not retry"));
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
