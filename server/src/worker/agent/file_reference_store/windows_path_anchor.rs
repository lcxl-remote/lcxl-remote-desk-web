//! Checked local NTFS paths retained by non-delete-sharing ancestor handles.
use super::*;
use desk_file_recovery::windows::{FileIdentity as NativeIdentity, FileKind, file_identity};
use std::path::{Component, Prefix};
use windows::Win32::Storage::FileSystem::{
    FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

pub(super) fn open_text(path: &Path) -> Result<(OpenedFile, Vec<File>), AgentError> {
    let (file, ancestors, _) = open_anchored(path, FileKind::File)?;
    Ok((file, ancestors))
}

pub(super) fn open_anchored(
    path: &Path,
    kind: FileKind,
) -> Result<(OpenedFile, Vec<File>, NativeIdentity), AgentError> {
    let invalid = || {
        error(
            AgentErrorKind::InvalidInput,
            "artifact path requires a bounded local drive path",
            false,
        )
    };
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => return Err(invalid()),
        },
        _ => return Err(invalid()),
    };
    if components.next() != Some(Component::RootDir) {
        return Err(invalid());
    }
    let names: Vec<_> = components.collect();
    if names.len() > 255 || (names.is_empty() && !matches!(kind, FileKind::Directory)) {
        return Err(invalid());
    }
    let mut path = PathBuf::from(format!("{}:\\", char::from(drive)));
    let root = open_verified_with_access(
        &path,
        FILE_READ_ATTRIBUTES.0,
        (FILE_SHARE_READ | FILE_SHARE_WRITE).0,
    )?;
    let root_identity = file_identity(&root.handle, FileKind::Directory)
        .map_err(|e| io_error("validate artifact path root", e))?;
    if names.is_empty() {
        return Ok((root, Vec::new(), root_identity));
    }
    let volume = root_identity.volume_serial;
    let mut anchors = vec![root.handle];
    for (index, component) in names.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(invalid());
        };
        let name = name.to_str().ok_or_else(invalid)?;
        if name.is_empty()
            || name.len() > 1024
            || name.ends_with(['.', ' '])
            || name
                .chars()
                .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
        {
            return Err(invalid());
        }
        path.push(name);
        let last = index + 1 == names.len();
        let opened = open_verified_with_access(
            &path,
            if last && matches!(kind, FileKind::File) {
                FILE_GENERIC_READ.0
            } else {
                FILE_READ_ATTRIBUTES.0
            },
            if last && matches!(kind, FileKind::File) {
                FILE_SHARE_READ.0
            } else {
                (FILE_SHARE_READ | FILE_SHARE_WRITE).0
            },
        )?;
        let identity = file_identity(
            &opened.handle,
            if last { kind } else { FileKind::Directory },
        )
        .map_err(|e| io_error("validate artifact path component", e))?;
        if identity.volume_serial != volume {
            return Err(invalid());
        }
        if last {
            return Ok((opened, anchors, identity));
        }
        // Denying DELETE pins every checked ancestor until the bounded read ends.
        anchors.push(opened.handle);
    }
    Err(invalid())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_drive_root_is_a_directory_anchor_never_a_file() {
        let cwd = std::env::current_dir().unwrap();
        let root: PathBuf = cwd.components().take(2).collect();
        let (opened, ancestors, identity) = open_anchored(&root, FileKind::Directory).unwrap();
        assert!(opened.metadata.is_dir());
        assert!(ancestors.is_empty());
        assert_eq!(
            identity,
            file_identity(&opened.handle, FileKind::Directory).unwrap()
        );
        let verbatim = std::fs::canonicalize(&root).unwrap();
        let (_, _, same) = open_anchored(&verbatim, FileKind::Directory).unwrap();
        assert_eq!(identity, same);
        assert!(open_anchored(&root, FileKind::File).is_err());
        assert!(open_anchored(&verbatim, FileKind::File).is_err());
        let drive_relative: PathBuf = cwd.components().take(1).collect();
        assert!(open_anchored(&drive_relative, FileKind::Directory).is_err());
        assert!(open_anchored(Path::new("relative"), FileKind::Directory).is_err());
        assert!(open_anchored(Path::new(r"\\server\share\"), FileKind::Directory).is_err());
    }
}
