//! Index replacement is restricted to an exclusively locked private directory.
use super::*;
use ::windows::Wdk::Storage::FileSystem::{
    FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexWriteOutcome {
    NotPublished,
    OutcomeUnknown,
}

#[derive(Debug)]
pub struct IndexWriteError {
    pub outcome: IndexWriteOutcome,
    source: io::Error,
}
impl std::fmt::Display for IndexWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "private index write {:?}: {}", self.outcome, self.source)
    }
}
impl std::error::Error for IndexWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl PrivateDirectoryLock {
    /// Replaces only the internal index. All participating readers and writers
    /// must take this directory's OS lock; this is not a user-file replacement API.
    pub fn write_index(&self, bytes: &[u8]) -> Result<(), IndexWriteError> {
        self.write_index_with_limit(bytes, crate::MAX_LEDGER_BYTES * 2)
    }

    /// Bounds logical bytes for the old index, retained pending indexes and
    /// the new staging file at peak publication. Other vault material must be
    /// reserved separately by the caller; this is not a filesystem block quota.
    pub fn write_index_with_limit(
        &self,
        bytes: &[u8],
        index_budget: u64,
    ) -> Result<(), IndexWriteError> {
        self.publish_index(bytes, index_budget, || Ok(()), || Ok(()))
    }

    fn publish_index(
        &self,
        bytes: &[u8],
        index_budget: u64,
        before_publish: impl FnOnce() -> io::Result<()>,
        after_publish: impl FnOnce() -> io::Result<()>,
    ) -> Result<(), IndexWriteError> {
        let mut attempted = self.write_uncertain.get();
        let mut staged: Option<File> = None;
        let result = (|| {
            if attempted {
                return Err(io::Error::other("private index publication is unresolved"));
            }
            if bytes.len() as u64 > crate::MAX_LEDGER_BYTES {
                return Err(crate::invalid("private index exceeds its write bound"));
            }
            self.validate()?;
            let expected = self.index_identity()?;
            let pending = self.pending_index_inventory()?;
            let old_bytes = expected.map_or(0, |(_, size)| size);
            let peak = old_bytes
                .checked_add(pending.bytes)
                .and_then(|size| size.checked_add(bytes.len() as u64))
                .ok_or_else(|| crate::invalid("private index quota overflow"))?;
            if pending.files.len() >= inventory::MAX_PENDING || peak > index_budget {
                return Err(crate::invalid("private index storage quota exceeded"));
            }
            let name = temporary_name()?;
            staged = Some(open_relative(
                &self._directory,
                &name,
                &self.user,
                OpenKind::Temporary,
            )?);
            let file = staged.as_mut().unwrap();
            file_identity(file, FileKind::File)?;
            security::validate_private_file(file, &self.user)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            before_publish()?;
            self.validate()?;
            security::validate_private_file(file, &self.user)?;
            if self.index_identity()? != expected {
                self.write_uncertain.set(true);
                return Err(io::Error::other("private index identity changed"));
            }
            // A failed native call cannot establish non-publication. Preserve
            // material and block further writes through this guard on any error.
            attempted = true;
            replace_index(file, &self._directory)?;
            after_publish()?;
            self._directory.sync_all()?;
            self.validate()?;
            security::validate_private_file(file, &self.user)?;
            Ok(())
        })();
        if let Err(source) = result {
            if attempted {
                self.write_uncertain.set(true);
            } else if let Some(file) = &staged {
                // Only this invocation's still-unpublished handle can be removed.
                // A cleanup failure retains the private material for recovery.
                let _ = delete_unpublished(file, &self.user);
            }
            return Err(IndexWriteError {
                outcome: if attempted {
                    IndexWriteOutcome::OutcomeUnknown
                } else {
                    IndexWriteOutcome::NotPublished
                },
                source,
            });
        }
        Ok(())
    }

    fn index_identity(&self) -> io::Result<Option<(super::super::FileIdentity, u64)>> {
        match open_relative(
            &self._directory,
            "index.json",
            &self.user,
            OpenKind::ReadFile,
        ) {
            Ok(file) => {
                let identity = file_identity(&file, FileKind::File)?;
                security::validate_private_file(&file, &self.user)?;
                let size = file.metadata()?.len();
                if size > crate::MAX_LEDGER_BYTES {
                    return Err(crate::invalid("private index exceeds its read bound"));
                }
                Ok(Some((identity, size)))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, OpenOptions},
        os::windows::fs::OpenOptionsExt,
    };

    fn directory(path: &std::path::Path) -> File {
        OpenOptions::new()
            .read(true)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(path)
            .unwrap()
    }

    #[test]
    fn destination_change_removes_only_own_unpublished_temporary() {
        let root = tempfile::tempdir().unwrap();
        let parent = directory(root.path());
        let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
        let lock = vault.try_lock().unwrap().unwrap();
        lock.write_index(b"old").unwrap();
        lock.create_new("other", b"other writer").unwrap();
        let error = lock
            .publish_index(
                b"must not publish",
                crate::MAX_LEDGER_BYTES * 2,
                || {
                    fs::rename(
                        root.path().join("vault/other"),
                        root.path().join("vault/index.json"),
                    )
                },
                || Ok(()),
            )
            .unwrap_err();
        assert_eq!(error.outcome, IndexWriteOutcome::NotPublished);
        assert_eq!(lock.read("index.json", 100).unwrap(), b"other writer");
        assert_eq!(fs::read_dir(root.path().join("vault")).unwrap().count(), 2);
        assert!(lock.write_index(b"must reload first").is_err());
    }

    #[test]
    fn injected_failure_after_publication_never_deletes_new_index_or_retries() {
        let root = tempfile::tempdir().unwrap();
        let parent = directory(root.path());
        let vault = PrivateDirectory::open_or_create(&parent, "vault").unwrap();
        let lock = vault.try_lock().unwrap().unwrap();
        lock.write_index(b"old").unwrap();
        let id = "a".repeat(64);
        let backup = format!("{id}.body");
        lock.create_new(&backup, b"must retain backup").unwrap();
        let error = lock
            .publish_index(
                b"published",
                crate::MAX_LEDGER_BYTES * 2,
                || Ok(()),
                || Err(io::Error::other("injected post-publication failure")),
            )
            .unwrap_err();
        assert_eq!(error.outcome, IndexWriteOutcome::OutcomeUnknown);
        assert_eq!(lock.read("index.json", 100).unwrap(), b"published");
        assert_eq!(fs::read_dir(root.path().join("vault")).unwrap().count(), 3);
        assert!(lock.write_index(b"must not replay").is_err());
        assert!(lock.create_new("must-not-exist", b"x").is_err());
        assert!(lock.remove_record_material(&id).is_err());
        assert_eq!(lock.read(&backup, 100).unwrap(), b"must retain backup");
    }
}

fn temporary_name() -> io::Result<String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
        .map_err(|_| io::Error::other("private temporary sequence exhausted"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    // FILE_CREATE, not name unpredictability, guarantees non-overwrite.
    Ok(format!(
        "index-pending-{}-{now:x}-{sequence:x}",
        std::process::id()
    ))
}

fn replace_index(file: &File, parent: &File) -> io::Result<()> {
    let encoded = leaf_name("index.json")?;
    let size = size_of::<FILE_RENAME_INFORMATION>() + encoded.len() * 2 + 2;
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = HANDLE(parent.as_raw_handle());
        (*info).FileNameLength = (encoded.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast(),
            encoded.len(),
        );
        NtSetInformationFile(
            HANDLE(file.as_raw_handle()),
            &mut status,
            info.cast(),
            size as u32,
            FileRenameInformation,
        )
    };
    if result.0 < 0 {
        return Err(io::Error::other(format!(
            "private index rename NTSTATUS {:#x}",
            result.0
        )));
    }
    Ok(())
}

fn delete_unpublished(file: &File, user: &str) -> io::Result<()> {
    file_identity(file, FileKind::File)?;
    security::validate_private_file(file, user)?;
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(io::Error::other)
}
