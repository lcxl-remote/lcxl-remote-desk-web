//! Deleting one file-manager entry, to the trash or permanently.
use std::fs;
use std::io;
use std::path::{Component, Path};

use desk_utils::error::DeskErrorCode;
use log::{info, warn};

use super::platform;
use crate::error::DeskError;

fn refused(message: &str) -> DeskError {
    DeskError::new_custom_error(DeskErrorCode::INVALID_PARAMS, message)
}

/// The path comes from the controller: it must name one entry inside a
/// directory, never a root, and must not climb with `..`.
fn validate_target(path: &Path) -> Result<(), DeskError> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err(refused(
            "The path must be absolute and must not contain '..'",
        ));
    }
    // `/`, a drive root and a UNC share root have no parent.
    if path.parent().is_none() || path.file_name().is_none() {
        return Err(refused("A root directory cannot be deleted"));
    }
    Ok(())
}

pub(super) fn delete_entry(path: &Path, permanently: bool) -> Result<(), DeskError> {
    validate_target(path)?;
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        // Already gone: deleting is idempotent.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let is_link = meta.file_type().is_symlink();
    platform::guard_delete(path, is_link, permanently)?;
    if permanently {
        warn!("Delete {} permanently", path.display());
        if is_link {
            platform::remove_link(path, &meta)?;
        } else if meta.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    } else {
        info!("Move {} to the trash", path.display());
        if let Err(error) = platform::move_to_trash(path) {
            log::error!("Moving {} to the trash failed: {error}", path.display());
            return Err(DeskError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("Failed to move to trash: {error}"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_relative_and_parent_paths_are_refused() {
        assert!(delete_entry(Path::new("relative/file"), true).is_err());
        #[cfg(unix)]
        {
            assert!(delete_entry(Path::new("/"), true).is_err());
            assert!(delete_entry(Path::new("/tmp/../etc"), true).is_err());
        }
        #[cfg(windows)]
        {
            assert!(delete_entry(Path::new("C:\\"), true).is_err());
            assert!(delete_entry(Path::new(r"\\server\share\"), true).is_err());
        }
    }

    #[test]
    fn permanent_delete_removes_a_non_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::write(tree.join("nested/file.txt"), b"x").unwrap();
        delete_entry(&tree, true).unwrap();
        assert!(!tree.exists());
    }

    #[test]
    fn missing_entry_is_already_deleted() {
        let dir = tempfile::tempdir().unwrap();
        delete_entry(&dir.path().join("absent"), true).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_link_is_deleted_not_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("missing"), &link).unwrap();
        delete_entry(&link, true).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn deleting_a_directory_link_keeps_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), b"x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        delete_entry(&link, true).unwrap();
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(target.join("keep.txt").exists());
    }
}
