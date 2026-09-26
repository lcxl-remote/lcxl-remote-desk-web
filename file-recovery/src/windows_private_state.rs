//! Current-user state files created with a protected DACL before any bytes exist.
use super::{FileKind, file_identity, security};
use ::windows::{
    Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
    },
    Win32::{Foundation::HANDLE, Storage::FileSystem::*, System::IO::IO_STATUS_BLOCK},
};
use std::{
    fs::File,
    io::{self, Write},
    mem::size_of,
    os::windows::io::AsRawHandle,
    path::Path,
};

pub fn validate(file: &File) -> io::Result<()> {
    file_identity(file, FileKind::File)?;
    security::validate_private_file(file, &security::current_user()?)
}

fn anchored_parent(path: &Path) -> io::Result<(Vec<File>, String)> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("state parent missing"))?;
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("state file name missing"))?;
    super::private::leaf_name(leaf)?;
    Ok((super::private::state_ancestors(directory)?, leaf.into()))
}

/// Existing objects are checked, never repaired or truncated during open.
pub fn open(path: &Path, create: bool) -> io::Result<File> {
    let (ancestors, leaf) = anchored_parent(path)?;
    let file = super::private::state_open(ancestors.last().unwrap(), &leaf, create, false)?;
    validate(&file)?;
    Ok(file)
}

/// Publication preserves the new private DACL instead of inheriting the target ACL.
pub fn write(path: &Path, contents: &[u8], nonce: [u8; 16]) -> io::Result<()> {
    let (ancestors, leaf) = anchored_parent(path)?;
    let parent = ancestors.last().unwrap();
    let nonce = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut file =
        super::private::state_open(parent, &format!(".private-state-{nonce}.tmp"), true, true)?;
    let mut published = false;
    let result = (|| {
        validate(&file)?;
        file.write_all(contents)?;
        file.sync_all()?;
        validate(&file)?;
        replace(&file, parent, &leaf)?;
        published = true;
        parent.sync_all()?;
        file.sync_all()?;
        validate(&file)
    })();
    if result.is_err() && !published {
        // Delete only this held temporary, never a path another process can replace.
        let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
        let _ = unsafe {
            SetFileInformationByHandle(
                HANDLE(file.as_raw_handle()),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        };
    }
    result
}

fn replace(file: &File, parent: &File, leaf: &str) -> io::Result<()> {
    let encoded = super::private::leaf_name(leaf)?;
    let size = size_of::<FILE_RENAME_INFORMATION>() + encoded.len() * 2 + 2;
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = HANDLE(parent.as_raw_handle());
        (*info).FileNameLength = (encoded.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            encoded.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast(),
            encoded.len(),
        );
        NtSetInformationFile(
            HANDLE(file.as_raw_handle()),
            &mut status,
            info.cast(),
            size as u32,
            FileRenameInformation,
        )
    };
    if result.0 < 0 {
        return Err(io::Error::other(format!(
            "private state publication NTSTATUS {:#x}",
            result.0
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_publication_replaces_content_and_preserves_private_security() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        assert_eq!(
            open(&path, false).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        write(&path, b"first secret", [1; 16]).unwrap();
        let file = open(&path, false).unwrap();
        validate(&file).unwrap();
        write(&path, b"second secret", [2; 16]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second secret");
        validate(&open(&path, false).unwrap()).unwrap();
    }

    #[test]
    fn held_ancestors_prevent_directory_replacement_during_state_io() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        std::fs::create_dir(&directory).unwrap();
        let moved = root.path().join("moved");
        let (ancestors, leaf) = anchored_parent(&directory.join("state")).unwrap();
        assert_eq!(leaf, "state");
        assert!(std::fs::rename(&directory, &moved).is_err());
        drop(ancestors);
        std::fs::rename(&directory, &moved).unwrap();
    }

    #[test]
    fn nonlocal_and_parent_traversal_paths_are_rejected() {
        assert!(anchored_parent(Path::new(r"\\server\share\secret")).is_err());
        assert!(anchored_parent(Path::new(r"C:\Users\..\secret")).is_err());
        assert!(anchored_parent(Path::new("relative-state")).is_err());
    }

    #[test]
    fn shared_hard_links_are_rejected_without_repairing_the_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state");
        write(&path, b"secret", [3; 16]).unwrap();
        std::fs::hard_link(&path, root.path().join("other")).unwrap();
        assert!(open(&path, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
    }
}
