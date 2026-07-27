//! Durable, atomic file replacement.
//!
//! A plain [`std::fs::write`] truncates the target before it writes, so a full
//! disk, a killed process or a partial write can leave an empty or half-written
//! file where a complete older one used to be — "the write failed, so the file
//! on disk is still the previous value" simply does not hold. Every write here
//! goes to a temporary file in the same directory, is flushed to stable storage
//! and only then replaces the target in a single step, so a concurrent reader
//! observes either the previous contents or the new ones and never a fragment.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Which permissions the replacement file carries.
///
/// The replacement is a *different* file that takes the target's name, so the
/// permissions are chosen here rather than inherited from the target the way an
/// in-place write would inherit them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    /// Readable and writable by the owner alone (`0o600` on unix), whatever the
    /// target carried before. For a file that one privileged process owns
    /// outright.
    OwnerOnly,
    /// Keep the permissions the target already carries, falling back to
    /// owner-only when creating it. Required where processes running as
    /// different users read the same file: a host writing its configuration as
    /// SYSTEM / root must not narrow it out of reach of a later run as the
    /// desktop user.
    ///
    /// Permissions only. Ownership follows whoever wrote the replacement, which
    /// an in-place write would have left alone — so on unix a privileged writer
    /// takes over a file a less privileged one created, and a mode that only
    /// granted the owner would stop reaching them. That is not a case this host
    /// has: the two identities only diverge under the Windows service, and
    /// `ReplaceFileW` carries the target's ACL across on its own.
    Preserve,
}

/// Write `contents` to `path` so that the target is only ever the complete old
/// value or the complete new one.
pub fn durable_atomic_write(path: &Path, contents: &[u8], mode: FileMode) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    // Same directory, so the replacement below stays within one filesystem; a
    // unique suffix so concurrent writers cannot adopt each other's partial file.
    let temporary = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));

    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);

        // Only ever widens back to what the target already allowed: the
        // temporary starts owner-only, and a target that does not exist yet
        // leaves it there. On Windows `ReplaceFileW` carries the target's ACL
        // over on its own, and `set_permissions` there only reaches the
        // read-only bit, so this stays unix-only.
        #[cfg(unix)]
        if mode == FileMode::Preserve
            && let Ok(existing) = fs::metadata(path)
        {
            fs::set_permissions(&temporary, existing.permissions())?;
        }

        replace_file(&temporary, path)?;
        sync_parent_directory(parent)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temporary, target)
}

#[cfg(target_os = "windows")]
fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACEFILE_WRITE_THROUGH, ReplaceFileW,
    };
    use windows::core::PCWSTR;

    let temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let temporary_ptr = PCWSTR(temporary_wide.as_ptr());
    let target_ptr = PCWSTR(target_wide.as_ptr());

    let result = unsafe {
        if target.exists() {
            ReplaceFileW(
                target_ptr,
                temporary_ptr,
                PCWSTR::null(),
                REPLACEFILE_WRITE_THROUGH,
                None,
                None,
            )
        } else {
            MoveFileExW(temporary_ptr, target_ptr, MOVEFILE_WRITE_THROUGH)
        }
    };
    result.map_err(io::Error::other)
}

#[cfg(not(any(unix, target_os = "windows")))]
fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    fs::rename(temporary, target)
}

/// Persist the directory entry itself. Without this the rename can still be
/// lost to a power failure even though the file's own contents were synced.
#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_replaces_the_previous_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.toml");
        durable_atomic_write(&path, b"old", FileMode::OwnerOnly).unwrap();

        durable_atomic_write(&path, b"new", FileMode::OwnerOnly).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    }

    /// The temporary is an implementation detail; leaving one behind would
    /// accumulate junk next to the file on every save.
    #[test]
    fn nothing_is_left_beside_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.toml");

        durable_atomic_write(&path, b"contents", FileMode::OwnerOnly).unwrap();

        let entries: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("state.toml")]);
    }

    /// A failure has to leave the previous value intact — that is the whole
    /// point of not truncating first. Writing into a path whose parent is a
    /// regular file cannot succeed, so it stands in for a disk that fills up.
    #[test]
    fn a_failed_write_leaves_the_target_and_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.toml");
        durable_atomic_write(&path, b"old", FileMode::OwnerOnly).unwrap();
        // `state.toml` is a file, so `state.toml/nested` has no valid parent.
        let unwritable = path.join("nested");

        assert!(durable_atomic_write(&unwritable, b"new", FileMode::OwnerOnly).is_err());

        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    }

    #[test]
    fn a_new_file_is_owner_only_under_either_mode() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let directory = tempfile::tempdir().unwrap();
            for (name, mode) in [
                ("owner.toml", FileMode::OwnerOnly),
                ("preserve.toml", FileMode::Preserve),
            ] {
                let path = directory.path().join(name);
                durable_atomic_write(&path, b"secret", mode).unwrap();
                let bits = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(bits, 0o600, "{name} should not be readable by others");
            }
        }
    }

    /// The configuration file is written by whichever role is running — the
    /// service daemon as SYSTEM / root, a portable host as the desktop user.
    /// Replacing it must not narrow it out of the other's reach.
    #[test]
    fn preserve_keeps_permissions_a_previous_writer_established() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("config.toml");
            durable_atomic_write(&path, b"first", FileMode::Preserve).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

            durable_atomic_write(&path, b"second", FileMode::Preserve).unwrap();

            let bits = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(bits, 0o644);
            assert_eq!(fs::read_to_string(&path).unwrap(), "second");
        }
    }

    /// The state a single privileged process owns goes the other way: each
    /// write puts it back to owner-only regardless of what it found.
    #[test]
    fn owner_only_narrows_a_widened_file_back() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("state.toml");
            durable_atomic_write(&path, b"first", FileMode::OwnerOnly).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();

            durable_atomic_write(&path, b"second", FileMode::OwnerOnly).unwrap();

            let bits = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(bits, 0o600);
        }
    }
}
