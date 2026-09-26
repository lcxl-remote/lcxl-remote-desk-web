//! Serialize system service changes across GUI and command-line installers.
use std::{
    fs::{File, OpenOptions, TryLockError},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

#[derive(Debug)]
pub struct ServiceOperationBusy;
impl std::fmt::Display for ServiceOperationBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Another system service operation is in progress")
    }
}
impl std::error::Error for ServiceOperationBusy {}

pub(super) fn acquire() -> super::ServiceResult<File> {
    lock(Path::new("/run/lock/lcxl-remote-desk-service.lock"), 0)
}

fn lock(path: &Path, owner: u32) -> super::ServiceResult<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != owner || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Untrusted service operation lock",
        )
        .into());
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(ServiceOperationBusy.into()),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
    // Never unlink: other processes must continue locking the same inode.
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contention_and_release_use_the_same_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("service.lock");
        let uid = unsafe { libc::geteuid() };
        let first = lock(&path, uid).unwrap();
        assert!(lock(&path, uid).unwrap_err().is::<ServiceOperationBusy>());
        drop(first);
        assert!(lock(&path, uid).is_ok());
    }
    #[test]
    fn links_and_incorrect_ownership_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        let uid = unsafe { libc::geteuid() };
        drop(lock(&target, uid).unwrap());
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(lock(&link, uid).is_err());
        assert!(lock(&target, uid.wrapping_add(1)).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::hard_link(&target, &link).unwrap();
        assert!(lock(&target, uid).is_err());
    }
}
