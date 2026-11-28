//! Creating one directory inside a listed directory.
use std::path::{Component, Path, PathBuf};

use desk_signal_facade::model::files::CreateDirectoryRequest;
use desk_utils::error::DeskErrorCode;

use crate::error::DeskError;

fn invalid(message: &str) -> DeskError {
    DeskError::new_custom_error(DeskErrorCode::INVALID_PARAMS, message)
}

/// Device names Windows reserves in every directory, with or without an
/// extension.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Syntax-only check of the new name, run before any approval prompt.
pub(super) fn validate_name(name: &str) -> Result<(), DeskError> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(invalid("The folder name is empty or reserved"));
    }
    if name.contains(['/', '\\', '\0']) {
        return Err(invalid("The folder name must not contain path separators"));
    }
    if cfg!(windows) {
        if name
            .chars()
            .any(|c| c.is_control() || "<>:\"|?*".contains(c))
        {
            return Err(invalid(
                "The folder name contains a character Windows does not allow",
            ));
        }
        if name.ends_with(['.', ' ']) {
            return Err(invalid(
                "The folder name must not end with a dot or a space",
            ));
        }
        let stem = name.split('.').next().unwrap_or(name).trim_end();
        if WINDOWS_RESERVED
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(stem))
        {
            return Err(invalid("The folder name is reserved by Windows"));
        }
    }
    Ok(())
}

/// The directory that receives the new entry. An empty path is the root
/// listing: `/` on Unix, the drive list on Windows, which takes no folders.
fn parent_dir(parent_path: &str) -> Result<PathBuf, DeskError> {
    let parent = if parent_path.is_empty() && cfg!(unix) {
        PathBuf::from("/")
    } else {
        PathBuf::from(parent_path)
    };
    if !parent.is_absolute() || parent.components().any(|c| c == Component::ParentDir) {
        return Err(invalid(
            "The parent directory must be an absolute path without '..'",
        ));
    }
    Ok(parent)
}

pub(super) async fn create_directory(request: &CreateDirectoryRequest) -> Result<(), DeskError> {
    validate_name(&request.name)?;
    let target = parent_dir(&request.parent_path)?.join(Path::new(&request.name));
    tokio::fs::create_dir(&target).await?;
    log::info!("Created directory {}", target.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(parent: &Path, name: &str) -> CreateDirectoryRequest {
        CreateDirectoryRequest {
            parent_path: parent.to_string_lossy().into_owned(),
            name: name.to_string(),
        }
    }

    #[tokio::test]
    async fn creates_once_and_rejects_an_existing_name() {
        let dir = tempfile::tempdir().unwrap();
        create_directory(&request(dir.path(), "new")).await.unwrap();
        assert!(dir.path().join("new").is_dir());
        assert!(create_directory(&request(dir.path(), "new")).await.is_err());
    }

    #[tokio::test]
    async fn invalid_names_create_nothing() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["", ".", "..", "a/b", "a\\b", "nul\0"] {
            assert!(
                create_directory(&request(dir.path(), name)).await.is_err(),
                "{name:?}"
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn windows_rules_apply_only_on_windows() {
        let windows_only = ["CON", "con.txt", "trailing.", "trailing ", "a<b", "a?b"];
        for name in windows_only {
            assert_eq!(validate_name(name).is_err(), cfg!(windows), "{name:?}");
        }
        assert!(validate_name("CONSOLE").is_ok());
    }

    #[test]
    fn parent_must_be_absolute_without_parent_components() {
        assert!(parent_dir("relative").is_err());
        #[cfg(unix)]
        {
            assert_eq!(parent_dir("").unwrap(), PathBuf::from("/"));
            assert!(parent_dir("/tmp/../etc").is_err());
        }
        #[cfg(windows)]
        assert!(parent_dir("").is_err());
    }
}
