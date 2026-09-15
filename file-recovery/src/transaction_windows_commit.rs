//! Shared ledger ordering around the Windows two-move transaction. Intermediate
//! states retain both backup and native material; no automatic overwrite rollback.
use super::*;
use crate::windows::{
    FileKind, MoveOutcome, PrivateDirectory, SourceSnapshot, StagedReplacement, file_identity,
    move_no_replace,
};
use std::fs::File;

pub struct WindowsCommitRequest<'a> {
    pub scope: &'a Scope,
    pub id: &'a str,
    pub parent: &'a File,
    pub directory: &'a PrivateDirectory,
    pub source: &'a SourceSnapshot,
    pub replacement: Option<&'a StagedReplacement>,
    pub now_ms: u64,
}
#[derive(Debug)]
pub struct WindowsCommitError {
    pub outcome: MoveOutcome,
    source: io::Error,
}
impl std::fmt::Display for WindowsCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Windows file commit {:?}: {}", self.outcome, self.source)
    }
}
impl std::error::Error for WindowsCommitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl LockedVault {
    /// The caller holds its device quota, execution epoch and exact authorization
    /// lane throughout. `guard` rechecks that lane before each visible move.
    pub fn commit_windows_transaction(
        &mut self,
        request: WindowsCommitRequest<'_>,
        guard: impl Fn() -> io::Result<()>,
    ) -> Result<(), WindowsCommitError> {
        let WindowsCommitRequest {
            scope,
            id,
            parent,
            directory,
            source,
            replacement,
            now_ms,
        } = request;
        let mut attempted = false;
        let result = (|| -> io::Result<()> {
            self.storage.ensure_writable()?;
            guard()?;
            source.revalidate()?;
            directory.validate()?;
            let (record, content, metadata) = self.export(scope, id, now_ms)?;
            if record.change != ChangeState::BackupReady
                || record.file_name != source.file_name()
                || content != source.content()
                || metadata != source.metadata()?
            {
                return Err(invalid(
                    "Windows commit backup no longer matches its source",
                ));
            }
            let tx = record
                .transaction
                .as_ref()
                .ok_or_else(|| invalid("Windows transaction is not registered"))?;
            let TransactionIdentity::Windows { files } = &tx.identity else {
                return Err(invalid("Windows transaction has another platform identity"));
            };
            let parent_identity = file_identity(parent, FileKind::Directory)?;
            let directory_identity = file_identity(directory.handle(), FileKind::Directory)?;
            if files.volume_serial != parent_identity.volume_serial
                || parent_identity != source.parent_identity()
                || files.parent_file_id != parent_identity.file_id
                || files.directory_file_id != Some(directory_identity.file_id)
                || source.identity().file_id != files.original_file_id
                || source.identity().volume_serial != files.volume_serial
                || replacement.map(|file| file.identity().file_id) != files.staged_file_id
            {
                return Err(invalid("Windows commit identities differ from the ledger"));
            }
            let _location = crate::windows::transaction_location(
                Path::new(&tx.parent),
                parent,
                &tx.directory,
                Some(directory.handle()),
                replacement.map(StagedReplacement::handle),
            )?;
            if let Some(staged) = replacement {
                staged.verify_for_source(source)?;
            }
            guard()?;
            source.revalidate()?;
            self.transition(scope, id, ChangeState::CommitIntent)?;
            // After this point, preserve CommitIntent on every error. Existing
            // projection and restart handling expose it as an unknown outcome.
            attempted = true;
            move_no_replace(
                source.handle(),
                source.identity(),
                directory.handle(),
                directory_identity,
                "original",
            )
            .map_err(io::Error::other)?;
            directory.handle().sync_all()?;
            source.revalidate_after_owned_move()?;
            if let Some(staged) = replacement {
                guard()?;
                staged.verify_for_source(source)?;
                move_no_replace(
                    staged.handle(),
                    staged.identity(),
                    parent,
                    parent_identity,
                    &record.file_name,
                )
                .map_err(io::Error::other)?;
                source.finish_replacement_metadata(staged)?;
            }
            parent.sync_all()?;
            directory.validate()?;
            source.revalidate_after_owned_move()?;
            self.transition(scope, id, ChangeState::Succeeded)
        })();
        result.map_err(|source| WindowsCommitError {
            outcome: if attempted {
                MoveOutcome::OutcomeUnknown
            } else {
                MoveOutcome::NotStarted
            },
            source,
        })
    }
}
