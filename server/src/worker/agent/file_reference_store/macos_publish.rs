//! macOS create-new publication; report when the destination first exists.
use super::*;

pub(super) fn publish(
    directory: &ObjectRef,
    file_name: &str,
    content_bytes: &[u8],
    published: &mut bool,
) -> Result<CreatedTextArtifact, AgentError> {
    publish_with_limit(
        directory,
        file_name,
        content_bytes,
        4 * 1024 * 1024,
        published,
    )
}

pub(super) fn publish_with_limit(
    directory: &ObjectRef,
    file_name: &str,
    content_bytes: &[u8],
    max_bytes: usize,
    published: &mut bool,
) -> Result<CreatedTextArtifact, AgentError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

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
            "artifact name is not one safe macOS leaf component",
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
    let stored = resolve(directory)?;
    let selected = open_verified(&stored.path)?;
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
                leaf.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        *published = true;
        let mut created = unsafe { File::from_raw_fd(raw) };
        created.write_all(content_bytes)?;
        created.sync_all()?;
        let created_identity = unix_file_identity(&created)?;
        drop(created);
        #[cfg(test)]
        run_artifact_after_close_hook();
        if unix_file_identity(&selected.handle)? != parent_identity {
            return Err(std::io::Error::other(
                "target parent identity changed during artifact creation",
            ));
        }
        let mut verified = open_relative_unix(&selected.handle, &leaf)?;
        if unix_file_identity(&verified)? != created_identity {
            return Err(std::io::Error::other(
                "artifact identity changed before read-back verification",
            ));
        }
        let mut bytes = Vec::new();
        verified.read_to_end(&mut bytes)?;
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
        let file = issue_durable_artifact(&stored.path.join(file_name))
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
