//! Unix file-manager primitives: access probes, the mount table and deletion
//! helpers. Linux and macOS specifics live in the submodules.
use std::ffi::CString;
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use desk_utils::error::DeskErrorCode;

use super::access::EntryAccess;
use crate::error::DeskError;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;

/// `faccessat` with the effective ids, following a final symlink.
pub(super) fn access(path: &Path, mode: libc::c_int) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated string for the whole call.
    unsafe { libc::faccessat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_EACCESS) == 0 }
}

/// The path a deletion acts on: the canonical parent joined with the final
/// name, so a final symlink is the link itself rather than its target.
fn delete_target(path: &Path) -> io::Result<PathBuf> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no parent",
        ));
    };
    Ok(fs::canonicalize(parent)?.join(name))
}

/// Mount points of the host, longest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MountTable {
    points: Vec<PathBuf>,
}

impl MountTable {
    pub(super) fn load() -> io::Result<Self> {
        Ok(Self::from_points(os::mount_points()?))
    }

    pub(super) fn from_points(mut points: Vec<PathBuf>) -> Self {
        points.sort_by_key(|point| std::cmp::Reverse(point.as_os_str().len()));
        Self { points }
    }

    /// Whether deleting `target` would remove a mount point or reach into one.
    fn guards(&self, target: &Path) -> bool {
        self.points.iter().any(|point| point.starts_with(target))
    }

    /// The mount point containing `path`; `/` when none matches.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn topdir(&self, path: &Path) -> &Path {
        self.points
            .iter()
            .map(PathBuf::as_path)
            .find(|point| path.starts_with(point))
            .unwrap_or(Path::new("/"))
    }
}

/// Probes for one listing; the mount table is read once.
pub(super) struct Probe {
    mounts: MountTable,
}

impl Probe {
    pub(super) fn new() -> Self {
        let mounts = MountTable::load().unwrap_or_else(|error| {
            log::warn!("Reading the mount table failed: {error}");
            MountTable::default()
        });
        Self { mounts }
    }

    pub(super) fn entry(&self, path: &Path) -> EntryAccess {
        EntryAccess {
            readable: access(path, libc::R_OK),
            deletable: self.deletable(path),
        }
    }

    fn deletable(&self, path: &Path) -> bool {
        let Ok(entry) = fs::symlink_metadata(path) else {
            return false;
        };
        let Ok(target) = delete_target(path) else {
            return false;
        };
        if !entry.file_type().is_symlink() && self.mounts.guards(&target) {
            return false;
        }
        let Some(parent) = target.parent() else {
            return false;
        };
        if !access(parent, libc::W_OK | libc::X_OK) {
            return false;
        }
        let Ok(parent_meta) = fs::metadata(parent) else {
            return false;
        };
        // In a sticky directory only the entry's or the directory's owner (or
        // root) may remove the entry.
        if parent_meta.mode() & u32::from(libc::S_ISVTX) != 0 {
            // SAFETY: geteuid has no preconditions.
            let euid = unsafe { libc::geteuid() };
            return euid == 0 || entry.uid() == euid || parent_meta.uid() == euid;
        }
        true
    }

    pub(super) fn dir_writable(&self, dir: &Path) -> bool {
        access(dir, libc::W_OK | libc::X_OK)
    }

    pub(super) fn trash_available(&self, dir: &Path) -> bool {
        os::trash_available(dir, &self.mounts)
    }
}

fn refused(message: &str) -> DeskError {
    DeskError::new_custom_error(DeskErrorCode::INVALID_PARAMS, message)
}

/// Refuse deletions the platform cannot perform safely.
pub(super) fn guard_delete(path: &Path, is_link: bool, permanently: bool) -> Result<(), DeskError> {
    let mounts = MountTable::load()?;
    let target = delete_target(path)?;
    if !is_link && mounts.guards(&target) {
        return Err(refused(
            "A mount point, or a directory containing one, cannot be deleted",
        ));
    }
    if is_link && !permanently {
        os::guard_link_trash(&target, &mounts)?;
    }
    Ok(())
}

/// Remove a symlink itself, never its target.
pub(super) fn remove_link(path: &Path, _meta: &Metadata) -> io::Result<()> {
    fs::remove_file(path)
}

pub(super) fn move_to_trash(path: &Path) -> Result<(), trash::Error> {
    os::move_to_trash(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_guard_covers_the_mount_and_its_ancestors_only() {
        let mounts = MountTable::from_points(vec![PathBuf::from("/"), PathBuf::from("/srv/data")]);
        assert!(mounts.guards(Path::new("/srv/data")));
        assert!(mounts.guards(Path::new("/srv")));
        assert!(!mounts.guards(Path::new("/srv/data/file")));
        assert!(!mounts.guards(Path::new("/srv/database")));
        assert_eq!(
            mounts.topdir(Path::new("/srv/data/x")),
            Path::new("/srv/data")
        );
        assert_eq!(mounts.topdir(Path::new("/srv/other")), Path::new("/"));
    }

    #[test]
    fn delete_target_keeps_a_final_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let target = delete_target(&link).unwrap();
        assert_eq!(target.file_name().unwrap(), "link");
        assert_eq!(
            target.parent().unwrap(),
            fs::canonicalize(dir.path()).unwrap()
        );
    }
}
