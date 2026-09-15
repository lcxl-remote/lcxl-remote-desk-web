//! Handle-relative, same-volume transaction moves. The caller must persist
//! CommitIntent and retain its authorization/parent/source guards before calling.
//! Once the native call is attempted, failure must never authorize blind replay.
use super::{FileIdentity, FileKind, file_identity, private};
use ::windows::{
    Wdk::Storage::FileSystem::{
        FILE_RENAME_INFORMATION, FileRenameInformation, NtSetInformationFile,
    },
    Win32::{Foundation::HANDLE, System::IO::IO_STATUS_BLOCK},
};
use std::{fs::File, io, mem::size_of, os::windows::io::AsRawHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveOutcome {
    NotStarted,
    OutcomeUnknown,
}

#[derive(Debug)]
pub struct MoveError {
    pub outcome: MoveOutcome,
    source: io::Error,
}
impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "file transaction move {:?}: {}",
            self.outcome, self.source
        )
    }
}
impl std::error::Error for MoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// This primitive only moves an already-open file; it neither opens a model
/// pathname nor overwrites a colliding leaf. `expected_*` are authenticated
/// transaction identities, not caller-supplied strings from the action payload.
pub fn move_no_replace(
    source: &File,
    expected_source: FileIdentity,
    destination: &File,
    expected_destination: FileIdentity,
    leaf: &str,
) -> Result<(), MoveError> {
    let mut attempted = false;
    let result = (|| -> io::Result<()> {
        let encoded = private::leaf_name(leaf)?;
        if file_identity(source, FileKind::File)? != expected_source
            || file_identity(destination, FileKind::Directory)? != expected_destination
            || expected_source.volume_serial != expected_destination.volume_serial
        {
            return Err(crate::invalid("transaction move identity changed"));
        }
        let bytes = size_of::<FILE_RENAME_INFORMATION>() + encoded.len() * 2;
        let mut storage = vec![0usize; bytes.div_ceil(size_of::<usize>())];
        let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        let mut status = IO_STATUS_BLOCK::default();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = HANDLE(destination.as_raw_handle());
            (*info).FileNameLength = (encoded.len() * 2) as u32;
            std::ptr::copy_nonoverlapping(
                encoded.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast(),
                encoded.len(),
            );
        }
        attempted = true;
        let status = unsafe {
            NtSetInformationFile(
                HANDLE(source.as_raw_handle()),
                &mut status,
                info.cast(),
                bytes as u32,
                FileRenameInformation,
            )
        };
        if status.0 != 0 {
            return Err(io::Error::other(format!(
                "transaction rename NTSTATUS {:#x}",
                status.0
            )));
        }
        if file_identity(source, FileKind::File)? != expected_source
            || file_identity(destination, FileKind::Directory)? != expected_destination
        {
            return Err(crate::invalid("transaction identity changed after move"));
        }
        Ok(())
    })();
    result.map_err(|source| MoveError {
        outcome: if attempted {
            MoveOutcome::OutcomeUnknown
        } else {
            MoveOutcome::NotStarted
        },
        source,
    })
}
