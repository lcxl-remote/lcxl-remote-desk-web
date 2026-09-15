//! Cleanup through Unix directory handles and registered inode identities.
use super::*;
use std::fs::File;
pub(super) fn clean(tx: &Transaction) -> io::Result<()> {
    let files = tx.identity.inodes()?;
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };
    let dir_name =
        CString::new(tx.directory.as_bytes()).map_err(|_| invalid("invalid transaction name"))?;
    if !tx.directory.starts_with(".assistant-transaction-")
        || tx.directory.contains('/')
        || tx.directory.contains('\\')
    {
        return Err(invalid("invalid transaction name"));
    }
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&tx.parent)?;
    let pm = parent.metadata()?;
    #[cfg(target_os = "macos")]
    let same_volume = match &tx.identity {
        TransactionIdentity::Macos { volume_uuid, .. } => {
            crate::macos::volume_uuid(&parent)? == *volume_uuid
        }
        _ => false,
    };
    #[cfg(not(target_os = "macos"))]
    let same_volume = pm.dev() == files.parent_device;
    if !same_volume || pm.ino() != files.parent_inode {
        return Err(invalid("transaction parent identity changed"));
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            dir_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        };
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let dm = directory.metadata()?;
    if files.directory_inode != Some(dm.ino()) || dm.dev() != pm.dev() {
        return Err(invalid(
            "transaction directory identity is unavailable or changed",
        ));
    }
    for name in [c"replacement", c"original"] {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::NotFound {
                continue;
            }
            return Err(e);
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let m = file.metadata()?;
        if !m.is_file()
            || m.dev() != pm.dev()
            || !(m.ino() == files.original_inode || Some(m.ino()) == files.staged_inode)
        {
            return Err(invalid("transaction child identity changed"));
        }
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), dir_name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    parent.sync_all()
}
