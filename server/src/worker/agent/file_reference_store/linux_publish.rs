//! Linux create-new publication with explicit uncertain-outcome tracking.
use super::*;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub(super) fn publish(
    directory: &ObjectRef,
    file_name: &str,
    content_bytes: &[u8],
    publication_attempted: &mut bool,
) -> Result<CreatedTextArtifact, AgentError> {
    publish_with_limit(
        directory,
        file_name,
        content_bytes,
        4 * 1024 * 1024,
        publication_attempted,
    )
}

pub(super) fn publish_with_limit(
    directory: &ObjectRef,
    file_name: &str,
    content_bytes: &[u8],
    max_bytes: usize,
    publication_attempted: &mut bool,
) -> Result<CreatedTextArtifact, AgentError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    struct PendingStage<'a> {
        directory: &'a File,
        name: &'a std::ffi::CStr,
        armed: bool,
    }

    impl Drop for PendingStage<'_> {
        fn drop(&mut self) {
            if self.armed {
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
                }
            }
        }
    }

    if directory.object_kind != ObjectKind::Directory {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "artifact creation requires one selected directory reference",
            false,
        ));
    }
    if content_bytes.len() > max_bytes {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            format!("artifact content exceeds the {max_bytes} byte ceiling"),
            false,
        ));
    }
    if file_name.is_empty()
        || file_name.len() > 200
        || matches!(file_name, "." | "..")
        || file_name
            .chars()
            .any(|character| character.is_control() || character == '/')
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "artifact name is not one safe Linux leaf component",
            false,
        ));
    }
    let leaf = CString::new(file_name).map_err(|_| {
        error(
            AgentErrorKind::InvalidInput,
            "artifact name contains an invalid NUL byte",
            false,
        )
    })?;
    let stage_name = CString::new(format!(".lrd-artifact-{}", uuid::Uuid::new_v4()))
        .expect("UUID artifact stage name has no NUL");
    let stored = resolve(directory)?;
    // Opening an exchanged FIFO must not wait for a writer.
    let selected_handle = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&stored.path)
        .map_err(|cause| io_error("open artifact directory", cause))?;
    let selected = OpenedFile {
        metadata: selected_handle
            .metadata()
            .map_err(|cause| io_error("inspect artifact directory", cause))?,
        identity: unix_file_identity(&selected_handle)
            .map_err(|cause| io_error("identify artifact directory", cause))?,
        handle: selected_handle,
    };
    if selected.identity != stored.identity || !selected.metadata.is_dir() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected directory changed after reference issuance",
            false,
        ));
    }

    let result = (|| -> std::io::Result<CreatedTextArtifact> {
        let parent_identity = unix_file_identity(&selected.handle)?;
        let raw = unsafe {
            libc::openat(
                selected.handle.as_raw_fd(),
                stage_name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut pending_stage = PendingStage {
            directory: &selected.handle,
            name: &stage_name,
            armed: true,
        };
        let mut created = unsafe { File::from_raw_fd(raw) };
        created.write_all(content_bytes)?;
        created.sync_all()?;
        let staged_identity = unix_file_identity(&created)?;

        *publication_attempted = true;
        let renamed = unsafe {
            libc::renameat2(
                selected.handle.as_raw_fd(),
                stage_name.as_ptr(),
                selected.handle.as_raw_fd(),
                leaf.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if renamed != 0 {
            let cause = std::io::Error::last_os_error();
            if cause.raw_os_error() == Some(libc::EEXIST) {
                // NOREPLACE rejected an existing destination before publication.
                *publication_attempted = false;
            } else {
                // Preserve staging when a storage error leaves publication uncertain.
                pending_stage.armed = false;
            }
            return Err(cause);
        }
        pending_stage.armed = false;
        selected.handle.sync_all()?;
        let published =
            desk_file_recovery::linux::open_beneath(&selected.handle, Path::new(file_name), false)?;
        let created_identity = unix_file_identity(&published)?;
        if created_identity.primary != staged_identity.primary
            || created_identity.secondary != staged_identity.secondary
            || published.metadata()?.nlink() != 1
        {
            return Err(std::io::Error::other(
                "published artifact no longer names the staged inode",
            ));
        }
        drop(created);
        drop(published);
        #[cfg(test)]
        run_artifact_after_close_hook();
        if unix_file_identity(&selected.handle)? != parent_identity {
            return Err(std::io::Error::other(
                "target parent identity changed during artifact creation",
            ));
        }
        let mut verified =
            desk_file_recovery::linux::open_beneath(&selected.handle, Path::new(file_name), false)?;
        if unix_file_identity(&verified)? != created_identity {
            return Err(std::io::Error::other(
                "artifact identity changed before read-back verification",
            ));
        }
        let mut bytes = Vec::new();
        Read::by_ref(&mut verified)
            .take(content_bytes.len() as u64 + 1)
            .read_to_end(&mut bytes)?;
        if unix_file_identity(&verified)? != created_identity {
            return Err(std::io::Error::other(
                "artifact identity changed during read-back verification",
            ));
        }
        if bytes != content_bytes {
            return Err(std::io::Error::other(
                "artifact read-back differs from requested bytes",
            ));
        }
        // Issue from the verified handle; reopening the pathname would introduce
        // another unbounded special-file open and an unrelated identity race.
        let metadata = verified.metadata()?;
        if metadata.nlink() != 1 || unix_file_identity(&verified)? != created_identity {
            return Err(std::io::Error::other(
                "artifact changed before reference issuance",
            ));
        }
        let file = issue_opened_with_lifetime(
            &stored.path.join(file_name),
            OpenedFile {
                handle: verified,
                metadata,
                identity: created_identity,
            },
            DURABLE_ARTIFACT_REF_TTL_SECS,
            true,
        )
        .map_err(|error| std::io::Error::other(error.message))?;
        Ok(CreatedTextArtifact {
            file,
            file_name: file_name.to_string(),
            byte_len: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        })
    })();
    result.map_err(|cause| io_error("create verified artifact", cause))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::ComputerActionResultClass;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn converted_documents_keep_their_larger_limit_and_uncertain_publication_class() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let directory = issue(root.path()).unwrap();
        let bytes = vec![b'x'; 4 * 1024 * 1024 + 1];
        let rejected = publication::create_text_artifact(
            &directory,
            "text.txt",
            std::str::from_utf8(&bytes).unwrap(),
        )
        .err()
        .expect("text limit must remain four MiB");
        assert_eq!(rejected.class, ComputerActionResultClass::Failed);
        assert!(!root.path().join("text.txt").exists());
        let created =
            publication::create_document_artifact(&directory, "converted.pdf", &bytes).unwrap();
        assert_eq!(created.byte_len, bytes.len() as u64);
        assert_eq!(
            std::fs::read(root.path().join("converted.pdf")).unwrap(),
            bytes
        );

        let path = root.path().join("uncertain.pdf");
        let replacement = root.path().join("replacement.pdf");
        std::fs::write(&replacement, b"approved document").unwrap();
        set_artifact_after_close_hook(Box::new(move || {
            std::fs::rename(replacement, path).unwrap();
        }));
        let failure = publication::create_document_artifact(
            &directory,
            "uncertain.pdf",
            b"approved document",
        )
        .err()
        .expect("replacement inode must not verify a document publication");
        assert_eq!(failure.class, ComputerActionResultClass::OutcomeUnknown);
    }

    #[test]
    fn selected_directory_replaced_with_fifo_fails_before_publication() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let selected = root.path().join("selected");
        std::fs::create_dir(&selected).unwrap();
        let directory = issue(&selected).unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&selected, &moved).unwrap();
        let name = std::ffi::CString::new(selected.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let failure = publication::create_text_artifact(&directory, "artifact.txt", "approved")
            .err()
            .expect("substituted directory must be rejected");
        assert_eq!(failure.class, ComputerActionResultClass::Failed);
        assert_eq!(std::fs::read_dir(moved).unwrap().count(), 0);
    }

    #[test]
    fn same_bytes_in_replacement_inode_do_not_verify_publication() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let directory = issue(root.path()).unwrap();
        let path = root.path().join("artifact.txt");
        let replacement = root.path().join("replacement.txt");
        std::fs::write(&replacement, b"approved").unwrap();
        set_artifact_after_close_hook(Box::new(move || {
            std::fs::rename(replacement, path).unwrap();
        }));
        let failure = publication::create_text_artifact(&directory, "artifact.txt", "approved")
            .err()
            .expect("a different inode must not verify the submitted artifact");
        assert_eq!(failure.class, ComputerActionResultClass::OutcomeUnknown);
        assert_eq!(
            std::fs::read(root.path().join("artifact.txt")).unwrap(),
            b"approved"
        );
    }

    #[test]
    fn fifo_substitution_before_readback_returns_unknown_without_opening_stream() {
        let _guard = file_store_test_lock();
        let root = tempfile::tempdir().unwrap();
        let directory = issue(root.path()).unwrap();
        let path = root.path().join("artifact.txt");
        let replaced = path.clone();
        set_artifact_after_close_hook(Box::new(move || {
            std::fs::remove_file(&replaced).unwrap();
            let name = std::ffi::CString::new(replaced.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        }));
        let result = publication::create_text_artifact(&directory, "artifact.txt", "approved");
        let failure = result
            .err()
            .expect("FIFO must not produce an artifact receipt");
        assert_eq!(failure.class, ComputerActionResultClass::OutcomeUnknown);
        use std::os::unix::fs::FileTypeExt;
        assert!(
            std::fs::symlink_metadata(path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }
}
