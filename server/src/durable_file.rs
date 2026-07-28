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
    /// The owner is carried across as well, as far as the writer is permitted
    /// to — see [`carry_ownership`]. On Windows `ReplaceFileW` does both on its
    /// own by keeping the target's ACL.
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
        //
        // Ownership goes first: changing it drops the set-user/group-ID bits on
        // most unices, so the permissions have to be applied after it to survive.
        #[cfg(unix)]
        if mode == FileMode::Preserve
            && let Ok(existing) = fs::metadata(path)
        {
            carry_ownership(&temporary, &existing);
            fs::set_permissions(&temporary, existing.permissions())?;
        }

        durable_replace(&temporary, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Give the replacement the owner the target already had.
///
/// The rename below hands the target's name to a *new* inode, so without this
/// the file would belong to whoever saved last — where an in-place write left
/// the original owner alone. The case that matters is a host saving as root
/// over a file the desktop user created: every permission bit is preserved, and
/// the user still loses the file, because the bits now describe root.
///
/// Best effort. Only a privileged writer may give a file away, and an
/// unprivileged one could not have taken the file over by writing in place
/// either; refusing the save would turn a configuration it is allowed to write
/// into an error.
#[cfg(unix)]
fn carry_ownership(temporary: &Path, existing: &fs::Metadata) {
    use std::os::unix::fs::MetadataExt as _;

    let _ = std::os::unix::fs::chown(temporary, Some(existing.uid()), Some(existing.gid()));
}

/// Give `temporary` the name `target` in a single step, and persist the
/// directory entry that says so.
///
/// For a writer that produces its file over time rather than in one call — a
/// streamed upload, say — and so cannot use [`durable_atomic_write`]. The
/// caller owns getting the contents to stable storage before calling; this
/// covers the rename itself, which is worth nothing on its own if a crash can
/// lose it. `temporary` must already be in the same directory as `target`, or
/// the replacement is not atomic.
pub fn durable_replace(temporary: &Path, target: &Path) -> io::Result<()> {
    replace_file(temporary, target)?;
    sync_parent_directory(target.parent().unwrap_or_else(|| Path::new(".")))
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

    /// A group this process is allowed to hand a file to: any supplementary
    /// group other than the one its own files land in. `None` when the account
    /// running the test belongs to nothing else, which is the one case an
    /// unprivileged test cannot express a change of owner in.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_group_this_process_can_give_a_file_to(current: u32) -> Option<u32> {
        // SAFETY: the first call only asks for the count, the second fills a
        // buffer allocated at exactly that size.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        if count <= 0 {
            return None;
        }
        let mut groups = vec![0 as libc::gid_t; count as usize];
        let filled = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        if filled < 0 {
            return None;
        }
        groups.truncate(filled as usize);
        groups.into_iter().find(|&group| group != current)
    }

    /// Replacing the file must not hand it to whoever saved last: the daemon
    /// runs as SYSTEM / root while a portable host runs as the desktop user,
    /// and both write this same path. Owner is checked through the group, the
    /// half of it an unprivileged process can actually change.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn preserve_keeps_the_owner_a_previous_writer_established() {
        use std::os::unix::fs::MetadataExt as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        durable_atomic_write(&path, b"first", FileMode::Preserve).unwrap();
        let Some(other_group) =
            a_group_this_process_can_give_a_file_to(fs::metadata(&path).unwrap().gid())
        else {
            return;
        };
        std::os::unix::fs::chown(&path, None, Some(other_group)).unwrap();

        durable_atomic_write(&path, b"second", FileMode::Preserve).unwrap();

        assert_eq!(fs::metadata(&path).unwrap().gid(), other_group);
        assert_eq!(fs::read_to_string(&path).unwrap(), "second");
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
