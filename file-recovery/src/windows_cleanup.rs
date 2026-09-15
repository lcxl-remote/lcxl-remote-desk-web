//! Removal of record material selected by the locked ledger's retention policy.
use super::*;

impl PrivateDirectoryLock {
    /// The caller must first durably mark this ledger record as purging and
    /// settle its user-file transaction. This only removes private backups;
    /// it does not decide retention, discard permission, or transaction state.
    /// Errors retain the purging state for reconciliation, including errors
    /// after deletion. Repeating cleanup tolerates already absent materials.
    pub fn remove_record_material(&self, id: &str) -> io::Result<()> {
        if !crate::valid_id(id) {
            return Err(crate::invalid("invalid recovery material record id"));
        }
        if self.write_uncertain.get() {
            return Err(io::Error::other("private index publication is unresolved"));
        }
        self.validate()?;
        for suffix in ["body", "metadata", "body.tmp", "metadata.tmp"] {
            let name = format!("{id}.{suffix}");
            let file =
                match open_relative(&self._directory, &name, &self.user, OpenKind::DeleteFile) {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
            file_identity(&file, FileKind::File)?;
            security::validate_private_file(&file, &self.user)?;
            self.validate()?;
            let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            unsafe {
                SetFileInformationByHandle(
                    HANDLE(file.as_raw_handle()),
                    FileDispositionInfo,
                    (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            }
            .map_err(io::Error::other)?;
            // Closing the exclusively opened handle completes deletion before
            // the directory flush. No path lookup is used to remove the object.
            drop(file);
        }
        self._directory.sync_all()?;
        self.validate()
    }
}
