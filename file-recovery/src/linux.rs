//! Linux handle-relative filesystem operations. No weaker syscall fallback.
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

mod metadata;
pub use metadata::{capture_metadata, copy_metadata, make_private};

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Resolve an existing entry beneath an already-authorized directory handle.
/// Symlinks, magic links and mount crossings are all rejected by the kernel.
pub fn open_beneath(directory: &File, path: &Path, directory_only: bool) -> io::Result<File> {
    open_beneath_flags(directory, path, directory_only, 0)
}

/// Text transaction reads must not change the atime stored in their metadata fence.
/// No fallback is permitted when the kernel denies O_NOATIME.
pub fn open_text_beneath(directory: &File, path: &Path) -> io::Result<File> {
    let file = open_beneath_flags(directory, path, false, libc::O_NOATIME)?;
    validate_source(&file)?;
    Ok(file)
}

fn open_beneath_flags(
    directory: &File,
    path: &Path,
    directory_only: bool,
    extra: i32,
) -> io::Result<File> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(super::invalid("a relative authorized path is required"));
    }
    let path =
        CString::new(path.as_os_str().as_bytes()).map_err(|_| super::invalid("invalid path"))?;
    let how = OpenHow {
        flags: (extra
            | libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if directory_only { libc::O_DIRECTORY } else { 0 }) as u64,
        mode: 0,
        // RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV.
        resolve: 0x08 | 0x04 | 0x01,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    let stat = file.metadata()?;
    if !(if directory_only {
        stat.is_dir()
    } else {
        stat.is_file()
    }) {
        return Err(super::invalid("unsupported filesystem entry"));
    }
    Ok(file)
}

/// Only an unelevated owner may mutate a single-link ordinary source file.
pub fn validate_source(file: &File) -> io::Result<()> {
    let uid = unsafe { libc::geteuid() };
    let stat = file.metadata()?;
    if uid == 0
        || uid != unsafe { libc::getuid() }
        || !stat.is_file()
        || stat.uid() != uid
        || stat.nlink() != 1
        || stat.mode() & 0o7000 != 0
    {
        return Err(super::invalid(
            "text mutation requires an owned single-link regular file without special mode bits",
        ));
    }
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut fs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Explicit local filesystem scope; a successful rename alone does not
    // establish the durability semantics of remote or userspace filesystems.
    if !matches!(fs.f_type as u64, 0xef53 | 0x58465342 | 0x9123683e) {
        return Err(super::invalid(
            "text mutation requires a supported local ext4, XFS or Btrfs filesystem",
        ));
    }
    Ok(())
}

pub fn exchange(directory: &File, source: &str, staged: &str) -> io::Result<()> {
    rename(directory, source, staged, libc::RENAME_EXCHANGE)
}

pub fn move_no_replace(directory: &File, source: &str, destination: &str) -> io::Result<()> {
    rename(directory, source, destination, libc::RENAME_NOREPLACE)
}

fn rename(directory: &File, source: &str, destination: &str, flags: u32) -> io::Result<()> {
    fn name(value: &str) -> io::Result<CString> {
        if value.is_empty() || value == "." || value == ".." || value.contains('/') {
            return Err(super::invalid("rename requires a single entry name"));
        }
        CString::new(value).map_err(|_| super::invalid("invalid entry name"))
    }
    let source = name(source)?;
    let destination = name(destination)?;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory.as_raw_fd(),
            source.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            flags,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn beneath_rejects_symlinks_and_parent_escape() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("text"), b"hello").unwrap();
        std::os::unix::fs::symlink("text", root.path().join("link")).unwrap();
        let directory = File::open(root.path()).unwrap();
        assert!(open_beneath(&directory, Path::new("text"), false).is_ok());
        assert!(open_beneath(&directory, Path::new("link"), false).is_err());
        assert!(open_beneath(&directory, Path::new(".."), true).is_err());
    }
    #[test]
    fn noatime_reads_preserve_metadata_even_when_relatime_would_update_it() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("text");
        std::fs::write(&path, b"content").unwrap();
        let file = File::open(&path).unwrap();
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
        file.set_times(std::fs::FileTimes::new().set_accessed(old))
            .unwrap();
        let before = capture_metadata(&file, "text").unwrap();
        let directory = File::open(root.path()).unwrap();
        let mut opened =
            open_beneath_flags(&directory, Path::new("text"), false, libc::O_NOATIME).unwrap();
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"content");
        assert_eq!(capture_metadata(&opened, "text").unwrap(), before);
    }

    #[test]
    fn no_replace_never_overwrites_and_exchange_swaps_entries() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a"), b"original").unwrap();
        std::fs::write(root.path().join("b"), b"staged").unwrap();
        let directory = File::open(root.path()).unwrap();
        assert!(move_no_replace(&directory, "a", "b").is_err());
        assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"staged");
        exchange(&directory, "a", "b").unwrap();
        assert_eq!(std::fs::read(root.path().join("a")).unwrap(), b"staged");
        assert_eq!(std::fs::read(root.path().join("b")).unwrap(), b"original");
    }
}
