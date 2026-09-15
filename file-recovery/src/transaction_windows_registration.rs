//! Register handle-derived Windows identities in the shared recovery ledger.
use super::*;
use std::fs::File;

impl LockedVault {
    /// Production source-to-ledger bridge: the saved recovery bytes must be the
    /// same frozen text and complete metadata currently held by the executor.
    /// Quota reservation and authorization precede this call; no user-file write
    /// is permitted if this method fails, even if the backup itself was saved.
    pub fn plan_windows_source_transaction(
        &mut self,
        scope: &Scope,
        id: &str,
        parent_path: &Path,
        parent: &File,
        source: &crate::windows::SourceSnapshot,
        now_ms: u64,
    ) -> io::Result<String> {
        source.revalidate()?;
        let (record, content, metadata) = self.export(scope, id, now_ms)?;
        if record.change != ChangeState::BackupReady
            || record.file_name != source.file_name()
            || crate::windows::file_identity(parent, crate::windows::FileKind::Directory)?
                != source.parent_identity()
            || content != source.content()
            || metadata != source.metadata()?
        {
            return Err(invalid(
                "saved backup does not match the held Windows source",
            ));
        }
        let directory =
            self.plan_windows_transaction(scope, id, parent_path, parent, source.handle())?;
        source.revalidate()?;
        Ok(directory)
    }

    /// Create only the directory named by an existing backed-up transaction plan,
    /// then persist its observed identity before returning it to the executor.
    /// Persistence failure retains the new directory; it must not be adopted by
    /// a retry or treated as permission to continue the user-file transaction.
    pub fn create_windows_transaction_directory(
        &mut self,
        scope: &Scope,
        id: &str,
        parent: &File,
    ) -> io::Result<crate::windows::PrivateDirectory> {
        self.storage.ensure_writable()?;
        let transaction = self
            .ledger
            .records
            .get(id)
            .filter(|record| &record.scope == scope && record.change == ChangeState::BackupReady)
            .and_then(|record| record.transaction.as_ref())
            .ok_or_else(|| invalid("transaction was not planned"))?
            .clone();
        let TransactionIdentity::Windows { files } = &transaction.identity else {
            return Err(invalid("transaction has a different platform identity"));
        };
        let parent_identity =
            crate::windows::file_identity(parent, crate::windows::FileKind::Directory)?;
        if files.directory_file_id.is_some()
            || files.staged_file_id.is_some()
            || files.parent_file_id != parent_identity.file_id
            || files.volume_serial != parent_identity.volume_serial
        {
            return Err(invalid(
                "transaction directory is already registered or its parent changed",
            ));
        }
        let _location = crate::windows::transaction_location(
            Path::new(&transaction.parent),
            parent,
            &transaction.directory,
            None,
            None,
        )?;
        let directory =
            crate::windows::PrivateDirectory::create_new(parent, &transaction.directory)?;
        self.register_windows_transaction(scope, id, parent, directory.handle(), None)?;
        Ok(directory)
    }

    /// The caller must retain its source version guard and authorized handles.
    /// The parent path is resolved and matched before persistence; neither this
    /// plan nor a saved identity authorizes access to an arbitrary user path.
    /// No transaction directory or user file is created by this operation.
    pub fn plan_windows_transaction(
        &mut self,
        scope: &Scope,
        id: &str,
        parent_path: &Path,
        parent: &File,
        original: &File,
    ) -> io::Result<String> {
        let files = WindowsIdentity::from_handles(parent, original)?;
        let directory = format!(".assistant-transaction-{id}");
        let _location =
            crate::windows::transaction_location(parent_path, parent, &directory, None, None)?;
        self.store_transaction_plan(
            scope,
            id,
            Transaction {
                parent: parent_path
                    .to_str()
                    .ok_or_else(|| invalid("invalid transaction parent encoding"))?
                    .into(),
                directory: directory.clone(),
                identity: TransactionIdentity::Windows { files },
            },
        )?;
        Ok(directory)
    }

    /// Register only the named transaction directory and its replacement file.
    /// Already registered directory and staging identities cannot be changed or
    /// retracted. Reopen the vault after any persistence error before proceeding.
    pub fn register_windows_transaction(
        &mut self,
        scope: &Scope,
        id: &str,
        parent: &File,
        directory: &File,
        staged: Option<&File>,
    ) -> io::Result<()> {
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|record| &record.scope == scope && record.change == ChangeState::BackupReady)
            .ok_or_else(|| invalid("transaction unavailable"))?;
        let transaction = record
            .transaction
            .as_mut()
            .ok_or_else(|| invalid("transaction was not planned"))?;
        let _location = crate::windows::transaction_location(
            Path::new(&transaction.parent),
            parent,
            &transaction.directory,
            Some(directory),
            staged,
        )?;
        let TransactionIdentity::Windows { files } = &mut transaction.identity else {
            return Err(invalid("transaction has a different platform identity"));
        };
        files.register_handles(parent, directory, staged)?;
        self.persist()
    }
}

#[cfg(test)]
#[path = "transaction_windows_tests.rs"]
mod tests;
