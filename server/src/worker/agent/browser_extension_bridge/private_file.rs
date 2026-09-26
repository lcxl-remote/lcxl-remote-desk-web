//! Open once, validate the opened object, then read with an enforced byte limit.
#[cfg(unix)]
mod unix;
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

pub(super) fn open(path: &Path, create: bool, limit: u64) -> io::Result<File> {
    #[cfg(windows)]
    let file = desk_file_recovery::windows::private_state::open(path, create)?;
    #[cfg(unix)]
    let file = unix::open(path, create)?;
    validate(&file, limit)?;
    Ok(file)
}

fn validate(file: &File, limit: u64) -> io::Result<()> {
    #[cfg(windows)]
    desk_file_recovery::windows::private_state::validate(file)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(io::Error::other("invalid browser bridge state file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
            || metadata.mode() & 0o7077 != 0
        {
            return Err(io::Error::other(
                "browser bridge state file is not user-private",
            ));
        }
    }
    Ok(())
}

pub(super) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(windows)]
    {
        desk_file_recovery::windows::private_state::write(
            path,
            contents,
            *uuid::Uuid::new_v4().as_bytes(),
        )
    }
    #[cfg(unix)]
    {
        unix::write(path, contents)
    }
}

pub(super) fn read(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    read_opened(open(path, false, limit)?, limit)
}

fn read_opened(mut file: File, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut file)
        .take(
            limit
                .checked_add(1)
                .ok_or_else(|| io::Error::other("invalid file limit"))?,
        )
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other(
            "browser bridge state exceeds its size bound",
        ));
    }
    validate(&file, limit)?;
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn private(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn replacement_path_cannot_change_the_opened_record() {
        let root = crate::worker::agent::browser_extension_bridge::private_test_directory();
        let path = root.path().join("state");
        private(&path, b"original");
        let opened = open(&path, false, 32).unwrap();
        std::fs::rename(&path, root.path().join("old")).unwrap();
        private(&path, b"replacement");
        assert_eq!(read_opened(opened, 32).unwrap(), b"original");
        assert_eq!(read(&path, 32).unwrap(), b"replacement");
    }

    #[test]
    fn rejects_links_shared_permissions_and_growth_after_open() {
        let root = crate::worker::agent::browser_extension_bridge::private_test_directory();
        let path = root.path().join("state");
        private(&path, b"a");
        let link = root.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(read(&link, 8).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::hard_link(&path, &link).unwrap();
        assert!(read(&path, 8).is_err());
        std::fs::remove_file(&link).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read(&path, 8).is_err());
        private(&path, b"a");
        let opened = open(&path, false, 8).unwrap();
        std::fs::write(&path, b"larger than eight").unwrap();
        assert!(read_opened(opened, 8).is_err());
    }

    #[test]
    fn non_regular_state_is_rejected_before_reading() {
        use std::os::unix::ffi::OsStrExt;
        let root = crate::worker::agent::browser_extension_bridge::private_test_directory();
        assert!(read(root.path(), 8).is_err());
        let fifo = root.path().join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        // O_NONBLOCK lets fstat reject this without waiting for a FIFO writer.
        assert!(read(&fifo, 8).is_err());
    }
}
