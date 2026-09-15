//! Cleanup after terminal state or a safe on-disk arrangement is established.
//! This never changes the recorded outcome or replays the user-file operation.
use super::super::{FileIdentity, SourceSnapshot};
use super::*;
use crate::{ChangeState, Transaction, TransactionIdentity};
use std::path::Path;

pub(crate) fn clean_transaction(
    tx: &Transaction,
    state: ChangeState,
    file_name: &str,
    content: &[u8],
    metadata: &[u8],
) -> io::Result<()> {
    let TransactionIdentity::Windows { files } = &tx.identity else {
        return Err(crate::invalid(
            "Windows cleanup requires Windows identities",
        ));
    };
    if !matches!(
        state,
        ChangeState::Succeeded
            | ChangeState::Aborted
            | ChangeState::CommitIntent
            | ChangeState::OutcomeUnknown
    ) {
        return Err(crate::invalid(
            "unresolved Windows transaction cannot be cleaned",
        ));
    }
    let anchors = root::resolve_directory(Path::new(&tx.parent))?;
    let parent = anchors
        .last()
        .ok_or_else(|| crate::invalid("transaction parent unavailable"))?;
    let identity = file_identity(parent, FileKind::Directory)?;
    if identity.volume_serial != files.volume_serial || identity.file_id != files.parent_file_id {
        return Err(crate::invalid("transaction cleanup parent changed"));
    }
    let user = security::current_user()?;
    let directory = match open_relative(parent, &tx.directory, &user, OpenKind::DeleteDirectory) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let directory_id = file_identity(&directory, FileKind::Directory)?;
    if directory_id.volume_serial != files.volume_serial
        || Some(directory_id.file_id) != files.directory_file_id
    {
        return Err(crate::invalid("transaction cleanup directory changed"));
    }
    security::validate_private(&directory, &user)?;
    let original = optional_snapshot(
        &directory,
        "original",
        FileIdentity {
            volume_serial: files.volume_serial,
            file_id: files.original_file_id,
        },
    )?;
    // Pin the current user pathname only when the journal did not establish
    // completion. A half-moved original must survive even explicit backup discard.
    let current = if matches!(
        state,
        ChangeState::CommitIntent | ChangeState::OutcomeUnknown
    ) {
        match open_relative(parent, file_name, &user, OpenKind::ReadFile) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let allow_unpublished = match state {
        ChangeState::Aborted => true,
        ChangeState::Succeeded => false,
        _ => {
            let current = current
                .as_ref()
                .ok_or_else(|| crate::invalid("original may be between transaction moves"))?;
            let current_id = file_identity(current, FileKind::File)?;
            if current_id.volume_serial != files.volume_serial {
                return Err(crate::invalid("current transaction volume changed"));
            }
            if current_id.file_id == files.original_file_id && original.is_none() {
                true
            } else if Some(current_id.file_id) == files.staged_file_id {
                false
            } else {
                return Err(crate::invalid(
                    "current file does not establish a safe cleanup state",
                ));
            }
        }
    };
    // An aborted operation must not have moved its original. A successful
    // update must not leave the replacement in its unpublished location.
    if allow_unpublished && original.is_some() {
        return Err(crate::invalid(
            "aborted transaction still contains its original",
        ));
    }
    if let Some(original) = &original {
        if original.content() != content
            || normalized(&original.metadata()?)? != normalized(metadata)?
        {
            return Err(crate::invalid(
                "retained original differs from saved recovery material",
            ));
        }
    }
    let replacement = match open_relative(&directory, "replacement", &user, OpenKind::DeleteFile) {
        Ok(file) => {
            let identity = file_identity(&file, FileKind::File)?;
            if !allow_unpublished
                || identity.volume_serial != files.volume_serial
                || Some(identity.file_id) != files.staged_file_id
            {
                return Err(crate::invalid("unexpected retained replacement"));
            }
            Some(file)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    security::validate_private(&directory, &user)?;
    if let Some(original) = original {
        original.revalidate()?;
        remove_handle(original.handle())?;
        drop(original);
    }
    if let Some(file) = replacement {
        remove_handle(&file)?;
        drop(file);
    }
    // Native directory removal refuses unexpected children. Never recurse or
    // discover paths to delete from enumeration or journal strings.
    remove_handle(&directory)?;
    drop(directory);
    parent.sync_all()
}

fn optional_snapshot(
    parent: &File,
    leaf: &str,
    expected: FileIdentity,
) -> io::Result<Option<SourceSnapshot>> {
    // Probe with the same fixed relative name to distinguish absence from every
    // other failure. The snapshot then reopens with its restrictive source guard.
    match open_relative(
        parent,
        leaf,
        &security::current_user()?,
        OpenKind::InspectTransactionFile,
    ) {
        Ok(file) => {
            if file_identity(&file, FileKind::File)? != expected {
                return Err(crate::invalid("retained original identity changed"));
            }
            drop(file);
            SourceSnapshot::capture(parent, leaf, expected).map(Some)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
fn normalized(bytes: &[u8]) -> io::Result<serde_json::Value> {
    let mut metadata: serde_json::Value =
        serde_json::from_slice(bytes).map_err(io::Error::other)?;
    let basic = metadata
        .get_mut("basic")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| crate::invalid("Windows recovery metadata is incomplete"))?;
    // Native movement changes ChangeTime; reads may update last access. All
    // content, ADS, permissions and other recorded metadata must still match.
    basic.remove("changed");
    basic.remove("accessed");
    Ok(metadata)
}
fn remove_handle(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    }
    .map_err(io::Error::other)
}
