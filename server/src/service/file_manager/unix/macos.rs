//! macOS: mount table from `getfsstat`, network-volume trash heuristic and
//! the NSFileManager trash backend.
use std::ffi::{CStr, CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use trash::macos::{DeleteMethod, TrashContextExtMacos};

use super::MountTable;
use crate::error::DeskError;

pub(super) fn mount_points() -> io::Result<Vec<PathBuf>> {
    // SAFETY: a null buffer only asks for the number of mounted file systems.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    // Room for mounts that appear between the two calls.
    let capacity = count as usize + 8;
    // SAFETY: `statfs` is plain old data; all-zero is a valid value.
    let mut buffer: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; capacity];
    let bytes = (capacity * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: `buffer` holds `capacity` entries, which is what `bytes` reports.
    let filled = unsafe { libc::getfsstat(buffer.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if filled < 0 {
        return Err(io::Error::last_os_error());
    }
    buffer.truncate(filled as usize);
    Ok(buffer
        .iter()
        .map(|fs| {
            // SAFETY: the kernel NUL-terminates `f_mntonname`.
            let name = unsafe { CStr::from_ptr(fs.f_mntonname.as_ptr()) };
            PathBuf::from(OsStr::from_bytes(name.to_bytes()))
        })
        .collect())
}

/// Network volumes have no trash; local volumes are assumed to have one.
pub(super) fn trash_available(dir: &Path, _mounts: &MountTable) -> bool {
    let Ok(path) = CString::new(dir.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: `statfs` is plain old data; all-zero is a valid value.
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `info` is a valid out buffer.
    if unsafe { libc::statfs(path.as_ptr(), &mut info) } != 0 {
        return true;
    }
    info.f_flags & libc::MNT_LOCAL as u32 != 0
}

/// Same-volume moves keep a link intact, so macOS needs no link guard.
pub(super) fn guard_link_trash(_target: &Path, _mounts: &MountTable) -> Result<(), DeskError> {
    Ok(())
}

/// `trashItemAtURL` fails where a volume has no trash, and needs neither
/// Finder nor Automation consent, unlike the crate's default Finder method.
pub(super) fn move_to_trash(path: &Path) -> Result<(), trash::Error> {
    let mut context = trash::TrashContext::default();
    context.set_delete_method(DeleteMethod::NsFileManager);
    context.delete(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_table_contains_root() {
        let points = mount_points().unwrap();
        assert!(points.iter().any(|point| point == Path::new("/")));
    }

    #[test]
    fn local_temp_directory_has_a_trash() {
        let dir = tempfile::tempdir().unwrap();
        assert!(trash_available(dir.path(), &MountTable::default()));
    }
}
