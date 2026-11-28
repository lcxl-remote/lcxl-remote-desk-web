//! Windows file-manager primitives: handle-open probes, the volume-type trash
//! heuristic and deletion helpers.
use std::fs::{self, Metadata};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::FileTypeExt;
use std::path::{Component, Path, Prefix};

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW,
    GetVolumePathNameW, OPEN_EXISTING,
};
use windows::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
};
use windows::core::PCWSTR;

use super::access::EntryAccess;
use crate::error::DeskError;

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Whether the host user can open `path` with `access`. Opening checks the
/// ACL, the read-only attribute and sharing with other handles; the handle is
/// closed at once. `FILE_FLAG_OPEN_NO_RECALL` keeps cloud placeholders from
/// being downloaded by the probe.
fn can_open(path: &Path, access: u32, flags: FILE_FLAGS_AND_ATTRIBUTES) -> bool {
    let name = wide(path);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            flags | FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_NO_RECALL,
            None,
        )
    };
    match handle {
        Ok(handle) => {
            // SAFETY: the handle was just opened and is closed exactly once.
            let _ = unsafe { CloseHandle(handle) };
            true
        }
        Err(_) => false,
    }
}

fn is_unc(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    )
}

/// Probes for one listing.
pub(super) struct Probe;

impl Probe {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn entry(&self, path: &Path) -> EntryAccess {
        EntryAccess {
            readable: can_open(path, FILE_READ_DATA.0, FILE_FLAGS_AND_ATTRIBUTES(0)),
            // A drive or share root has no parent and is never deletable.
            deletable: path.parent().is_some()
                && can_open(path, DELETE.0, FILE_FLAG_OPEN_REPARSE_POINT),
        }
    }

    pub(super) fn dir_writable(&self, dir: &Path) -> bool {
        can_open(
            dir,
            (FILE_ADD_FILE | FILE_ADD_SUBDIRECTORY).0,
            FILE_FLAGS_AND_ATTRIBUTES(0),
        )
    }

    /// Network, removable, optical and RAM drives have no recycle bin.
    pub(super) fn trash_available(&self, dir: &Path) -> bool {
        if is_unc(dir) {
            return false;
        }
        let name = wide(dir);
        let mut root = [0u16; 261];
        // SAFETY: `name` is NUL-terminated; `root` is a writable buffer.
        if unsafe { GetVolumePathNameW(PCWSTR(name.as_ptr()), &mut root) }.is_err() {
            return true;
        }
        // SAFETY: GetVolumePathNameW NUL-terminated `root`.
        let kind = unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) };
        !matches!(
            kind,
            DRIVE_REMOTE | DRIVE_REMOVABLE | DRIVE_CDROM | DRIVE_RAMDISK
        )
    }
}

/// Volume mount points are reparse points, which `remove_dir_all` does not
/// enter, so Windows needs no mount guard.
pub(super) fn guard_delete(
    _path: &Path,
    _is_link: bool,
    _permanently: bool,
) -> Result<(), DeskError> {
    Ok(())
}

/// Remove a symlink or junction itself, never its target.
pub(super) fn remove_link(path: &Path, meta: &Metadata) -> io::Result<()> {
    if meta.file_type().is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

pub(super) fn move_to_trash(path: &Path) -> Result<(), trash::Error> {
    trash::delete(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_root_is_never_deletable() {
        let probe = Probe::new();
        assert!(!probe.entry(Path::new("C:\\")).deletable);
    }

    #[test]
    fn read_only_file_is_readable_and_probed_for_delete() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ro.txt");
        fs::write(&file, b"x").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&file, permissions).unwrap();
        let probe = Probe::new();
        assert!(probe.entry(&file).readable);
        assert!(probe.dir_writable(dir.path()));
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(&file, permissions).unwrap();
    }

    #[test]
    fn unc_paths_have_no_trash() {
        assert!(!Probe::new().trash_available(Path::new(r"\\server\share\dir")));
    }

    #[test]
    fn deleting_a_junction_keeps_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), b"x").unwrap();
        let link = dir.path().join("link");
        std::os::windows::fs::symlink_dir(&target, &link)
            .or_else(|_| {
                std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J"])
                    .arg(&link)
                    .arg(&target)
                    .status()
                    .map(|_| ())
            })
            .unwrap();
        let meta = fs::symlink_metadata(&link).unwrap();
        remove_link(&link, &meta).unwrap();
        assert!(!link.exists());
        assert!(target.join("keep.txt").exists());
    }
}
