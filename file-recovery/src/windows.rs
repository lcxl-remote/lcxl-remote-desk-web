//! Native identity observation through handles validated by the caller's
//! authorization and containment layer. This module does not authorize mutation.
use crate::WindowsIdentity;
use ::windows::Win32::{Foundation::HANDLE, Storage::FileSystem::*};
use std::{fs::File, io, mem::size_of, os::windows::io::AsRawHandle};

#[path = "windows_private.rs"]
mod private;
pub(crate) use private::clean_transaction;
pub(crate) use private::transaction_location;
#[path = "windows_security.rs"]
mod security;
#[path = "windows_source.rs"]
mod source;
#[path = "windows_streams.rs"]
mod streams;
pub use private::{
    IndexWriteError, IndexWriteOutcome, PendingIndexFile, PendingIndexInventory, PrivateDirectory,
    PrivateDirectoryLock,
};
pub use source::{SourceSnapshot, StagedReplacement};
#[path = "windows_move.rs"]
mod movement;
pub use movement::{MoveError, MoveOutcome, move_no_replace};
pub use streams::{StreamInfo, StreamInventory, stream_inventory};

pub fn current_user_sid() -> io::Result<String> {
    security::current_user()
}

/// The daemon supplies a held process handle, never an unverified PID from IPC.
pub fn process_user_sid(process: HANDLE) -> io::Result<String> {
    security::process_user(process)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume_serial: u64,
    pub file_id: [u8; 16],
}

pub fn file_identity(file: &File, kind: FileKind) -> io::Result<FileIdentity> {
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }.map_err(io::Error::other)?;
    let directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || directory != (kind == FileKind::Directory)
        || (!directory && info.nNumberOfLinks != 1)
    {
        return Err(crate::invalid(
            "unexpected recovery object type, reparse point or hard link",
        ));
    }
    let mut filesystem = [0u16; 32];
    unsafe { GetVolumeInformationByHandleW(handle, None, None, None, None, Some(&mut filesystem)) }
        .map_err(io::Error::other)?;
    let length = filesystem
        .iter()
        .position(|unit| *unit == 0)
        .ok_or_else(|| crate::invalid("recovery filesystem name is unavailable"))?;
    if filesystem[..length] != [b'N' as u16, b'T' as u16, b'F' as u16, b'S' as u16] {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "recovery identity requires NTFS",
        ));
    }
    let mut id = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(io::Error::other)?;
    Ok(FileIdentity {
        volume_serial: id.VolumeSerialNumber,
        file_id: id.FileId.Identifier,
    })
}

impl WindowsIdentity {
    /// The caller retains the validated handles and the source version guard.
    /// Names, directory containment, permissions and snapshots are separate checks.
    pub fn from_handles(parent: &File, original: &File) -> io::Result<Self> {
        let parent = file_identity(parent, FileKind::Directory)?;
        let original = file_identity(original, FileKind::File)?;
        if parent.volume_serial != original.volume_serial || parent.file_id == original.file_id {
            return Err(crate::invalid(
                "recovery objects do not share the expected volume",
            ));
        }
        Ok(Self {
            volume_serial: parent.volume_serial,
            parent_file_id: parent.file_id,
            directory_file_id: None,
            original_file_id: original.file_id,
            staged_file_id: None,
        })
    }

    /// Extends a planned identity without replacing already registered objects.
    /// No ledger write or filesystem mutation occurs here.
    pub fn register_handles(
        &mut self,
        parent: &File,
        directory: &File,
        staged: Option<&File>,
    ) -> io::Result<()> {
        let parent = file_identity(parent, FileKind::Directory)?;
        let directory = file_identity(directory, FileKind::Directory)?;
        let staged = staged
            .map(|file| file_identity(file, FileKind::File))
            .transpose()?;
        if parent.volume_serial != self.volume_serial
            || parent.file_id != self.parent_file_id
            || directory.volume_serial != self.volume_serial
            || directory.file_id == parent.file_id
            || self
                .directory_file_id
                .is_some_and(|id| id != directory.file_id)
            || staged.is_some_and(|file| {
                file.volume_serial != self.volume_serial || file.file_id == self.original_file_id
            })
            || self
                .staged_file_id
                .is_some_and(|id| staged.map(|file| file.file_id) != Some(id))
        {
            return Err(crate::invalid("registered recovery identity changed"));
        }
        self.directory_file_id = Some(directory.file_id);
        self.staged_file_id = staged.map(|file| file.file_id);
        Ok(())
    }
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
