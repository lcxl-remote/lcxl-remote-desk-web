use std::{fmt, io, path::Path};

/// Whether a failed publication can safely be discarded or tried at another name.
#[derive(Debug)]
pub enum PublishError {
    NotPublished(io::Error),
    Uncertain(io::Error),
}

impl PublishError {
    pub fn is_collision(&self) -> bool {
        matches!(self, Self::NotPublished(error) if error.kind() == io::ErrorKind::AlreadyExists)
    }

    pub fn is_uncertain(&self) -> bool {
        matches!(self, Self::Uncertain(_))
    }

    pub(super) fn rename(error: io::Error) -> Self {
        // A remote filesystem may report failure after its server renamed the file.
        if matches!(
            error.kind(),
            io::ErrorKind::AlreadyExists | io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
        ) {
            Self::NotPublished(error)
        } else {
            Self::Uncertain(error)
        }
    }
}

impl fmt::Display for PublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPublished(error) | Self::Uncertain(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::NotPublished(error) | Self::Uncertain(error) => error,
        })
    }
}

/// Publish an already synced, closed staging file without replacing any target.
/// Both paths must have the same parent directory. No copy or link fallback is used.
pub fn durable_publish_new(temporary: &Path, target: &Path) -> Result<(), PublishError> {
    publish_with(
        temporary,
        target,
        platform::publish,
        super::sync_parent_directory,
    )
}

fn publish_with(
    temporary: &Path,
    target: &Path,
    publish: impl FnOnce(&Path, &Path) -> io::Result<()>,
    sync_directory: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), PublishError> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    if temporary.parent() != target.parent() {
        return Err(PublishError::NotPublished(io::Error::new(
            io::ErrorKind::InvalidInput,
            "staging and target must share a directory",
        )));
    }
    publish(temporary, target).map_err(PublishError::rename)?;
    sync_directory(parent).map_err(PublishError::Uncertain)
}

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;
    pub(super) fn publish(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace publication is unsupported",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn publishes_whole_file_and_preserves_both_files_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        let target = dir.path().join("target");
        fs::write(&stage, b"first").unwrap();
        durable_publish_new(&stage, &target).unwrap();
        assert!(!stage.exists());
        assert_eq!(fs::read(&target).unwrap(), b"first");
        fs::write(&stage, b"second").unwrap();
        assert!(
            durable_publish_new(&stage, &target)
                .unwrap_err()
                .is_collision()
        );
        assert_eq!(fs::read(&stage).unwrap(), b"second");
        assert_eq!(fs::read(&target).unwrap(), b"first");
    }

    #[test]
    fn directory_sync_failure_preserves_published_result_as_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        let target = dir.path().join("target");
        fs::write(&stage, b"complete").unwrap();
        let error = publish_with(&stage, &target, platform::publish, |_| {
            assert_eq!(fs::read(&target).unwrap(), b"complete");
            Err(io::Error::other("directory sync failed"))
        })
        .unwrap_err();
        assert!(error.is_uncertain());
        assert!(!error.is_collision());
        assert_eq!(fs::read(target).unwrap(), b"complete");
    }

    #[test]
    fn unsupported_publish_does_not_sync_or_remove_staging() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        fs::write(&stage, b"complete").unwrap();
        let error = publish_with(
            &stage,
            &dir.path().join("target"),
            |_, _| {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported filesystem",
                ))
            },
            |_| panic!("must not sync after a failed rename"),
        )
        .unwrap_err();
        assert!(!error.is_collision());
        assert!(!error.is_uncertain());
        assert_eq!(fs::read(stage).unwrap(), b"complete");
    }
}
