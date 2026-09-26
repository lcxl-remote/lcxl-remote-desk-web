//! Descriptor-relative state I/O; untrusted writable parents are not accepted.
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
};

fn name(bytes: &[u8]) -> io::Result<CString> {
    CString::new(bytes).map_err(|_| io::Error::other("invalid state path component"))
}

fn owned(fd: i32) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn validate_directory(file: &File, leaf: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    let uid = unsafe { libc::geteuid() };
    // A root-owned sticky ancestor such as /tmp cannot rename another user's entry.
    let sticky_ancestor = !leaf && metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
    if !metadata.is_dir()
        || metadata.nlink() == 0
        || (metadata.uid() != uid && metadata.uid() != 0)
        || (metadata.mode() & 0o022 != 0 && !sticky_ancestor)
        || (leaf && metadata.uid() != uid)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "browser state directory is not trusted for this OS user",
        ));
    }
    Ok(())
}

fn parent(path: &Path) -> io::Result<(File, CString)> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let leaf = absolute
        .file_name()
        .ok_or_else(|| io::Error::other("state file name missing"))?;
    let parent = absolute
        .parent()
        .ok_or_else(|| io::Error::other("state file parent missing"))?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut directory = owned(unsafe { libc::open(c"/".as_ptr(), flags) })?;
    validate_directory(&directory, false)?;
    if parent.components().count() > 128 {
        return Err(io::Error::other("state path is too deep"));
    }
    for component in parent.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(value) => {
                let value = name(value.as_bytes())?;
                let child =
                    owned(unsafe { libc::openat(directory.as_raw_fd(), value.as_ptr(), flags) })?;
                validate_directory(&child, false)?;
                directory = child;
            }
            _ => return Err(io::Error::other("state path traversal is not allowed")),
        }
    }
    validate_directory(&directory, true)?;
    Ok((directory, name(leaf.as_bytes())?))
}

fn open_at(parent: &File, leaf: &CStr, flags: i32) -> io::Result<File> {
    owned(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    })
}

pub(super) fn open(path: &Path, create: bool) -> io::Result<File> {
    let (directory, leaf) = parent(path)?;
    let file = open_at(
        &directory,
        &leaf,
        if create {
            libc::O_RDWR | libc::O_CREAT
        } else {
            libc::O_RDONLY
        },
    )?;
    validate_directory(&directory, true)?;
    Ok(file)
}

pub(super) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let (directory, leaf) = parent(path)?;
    let temporary = name(format!(".browser-state-{}.tmp", uuid::Uuid::new_v4()).as_bytes())?;
    let mut file = open_at(
        &directory,
        &temporary,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
    )?;
    let result = (|| {
        super::validate(&file, contents.len() as u64)?;
        file.write_all(contents)?;
        file.sync_all()?;
        super::validate(&file, contents.len() as u64)?;
        validate_directory(&directory, true)?;
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                leaf.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        directory.sync_all()?;
        validate_directory(&directory, true)
    })();
    if result.is_err() {
        // Cleanup stays anchored even if the caller's path has been renamed.
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn symlinked_and_shared_parents_are_rejected_without_permission_changes() {
        let temporary = crate::worker::agent::browser_extension_bridge::private_test_directory();
        let root = temporary.path().canonicalize().unwrap();
        let private = root.join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        write(&private.join("secret"), b"private").unwrap();
        symlink(&private, root.join("alias")).unwrap();
        assert!(open(&root.join("alias/secret"), false).is_err());
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(open(&private.join("secret"), false).is_err());
        assert!(write(&private.join("secret"), b"replacement").is_err());
        assert_eq!(std::fs::metadata(&private).unwrap().mode() & 0o777, 0o777);
        assert_eq!(std::fs::read(private.join("secret")).unwrap(), b"private");
    }

    #[test]
    fn held_parent_does_not_follow_a_replacement_path() {
        use std::io::Read;
        let temporary = crate::worker::agent::browser_extension_bridge::private_test_directory();
        let root = temporary.path().canonicalize().unwrap();
        let original = root.join("directory");
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
        write(&original.join("secret"), b"original").unwrap();
        let (held, leaf) = parent(&original.join("secret")).unwrap();
        std::fs::rename(&original, root.join("moved")).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o700)).unwrap();
        write(&original.join("secret"), b"replacement").unwrap();
        let mut bytes = Vec::new();
        open_at(&held, &leaf, libc::O_RDONLY)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"original");
    }
}
