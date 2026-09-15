//! Cleanup touches only registered directory objects and their known children.
use super::*;
#[cfg(unix)]
use std::fs::OpenOptions;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "StoredTransaction")]
pub struct Transaction {
    pub parent: String,
    pub directory: String,
    pub identity: TransactionIdentity,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredTransaction {
    Current(CurrentTransaction),
    Legacy(LegacyTransaction),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentTransaction {
    parent: String,
    directory: String,
    identity: TransactionIdentity,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyTransaction {
    parent: String,
    directory: String,
    parent_device: u64,
    parent_volume_uuid: Option<[u8; 16]>,
    parent_inode: u64,
    directory_inode: Option<u64>,
    original_inode: u64,
    staged_inode: Option<u64>,
}
impl TryFrom<StoredTransaction> for Transaction {
    type Error = String;
    fn try_from(value: StoredTransaction) -> Result<Self, Self::Error> {
        Ok(match value {
            StoredTransaction::Current(tx) => Self {
                parent: tx.parent,
                directory: tx.directory,
                identity: tx.identity,
            },
            StoredTransaction::Legacy(tx) => {
                let files = InodeIdentity {
                    parent_device: tx.parent_device,
                    parent_inode: tx.parent_inode,
                    directory_inode: tx.directory_inode,
                    original_inode: tx.original_inode,
                    staged_inode: tx.staged_inode,
                };
                let identity = match tx.parent_volume_uuid {
                    Some(volume_uuid) => TransactionIdentity::Macos { volume_uuid, files },
                    None => TransactionIdentity::Unix { files },
                };
                Self {
                    parent: tx.parent,
                    directory: tx.directory,
                    identity,
                }
            }
        })
    }
}
impl Transaction {
    pub(crate) fn validate(&self, id: &str) -> io::Result<()> {
        self.identity.validate_host()?;
        if !Path::new(&self.parent).is_absolute()
            || self.parent.len() > 4096
            || self.parent.chars().any(char::is_control)
            || self.directory != format!(".assistant-transaction-{id}")
        {
            return Err(invalid("invalid registered transaction location"));
        }
        Ok(())
    }
}
impl LockedVault {
    #[cfg(unix)]
    pub fn plan_transaction(
        &mut self,
        scope: &Scope,
        id: &str,
        parent: &str,
        device: u64,
        inode: u64,
        original_inode: u64,
    ) -> io::Result<String> {
        if !Path::new(parent).is_absolute()
            || parent.len() > 4096
            || parent.chars().any(char::is_control)
        {
            return Err(invalid("invalid transaction parent"));
        }
        #[cfg(target_os = "macos")]
        let parent_volume_uuid = {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let directory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(parent)?;
            let metadata = directory.metadata()?;
            if metadata.dev() != device || metadata.ino() != inode {
                return Err(invalid("transaction parent identity changed"));
            }
            crate::macos::volume_uuid(&directory)?
        };
        let directory = format!(".assistant-transaction-{id}");
        let files = InodeIdentity {
            parent_device: device,
            parent_inode: inode,
            directory_inode: None,
            original_inode,
            staged_inode: None,
        };
        #[cfg(target_os = "macos")]
        let identity = TransactionIdentity::Macos {
            volume_uuid: parent_volume_uuid,
            files,
        };
        #[cfg(not(target_os = "macos"))]
        let identity = TransactionIdentity::Unix { files };
        self.store_transaction_plan(
            scope,
            id,
            Transaction {
                parent: parent.into(),
                directory: directory.clone(),
                identity,
            },
        )?;
        Ok(directory)
    }

    fn store_transaction_plan(
        &mut self,
        scope: &Scope,
        id: &str,
        transaction: Transaction,
    ) -> io::Result<()> {
        transaction.validate(id)?;
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|record| {
                &record.scope == scope
                    && record.change == ChangeState::BackupReady
                    && record.transaction.is_none()
            })
            .ok_or_else(|| invalid("transaction cannot be planned"))?;
        record.transaction = Some(transaction);
        if let Err(error) = self.persist() {
            // A failed plan cannot authorize creation from in-memory state.
            // Reopening reads the actual durable plan if publication occurred.
            self.ledger.records.get_mut(id).unwrap().transaction = None;
            return Err(error);
        }
        Ok(())
    }
    #[cfg(unix)]
    pub fn register_transaction(
        &mut self,
        scope: &Scope,
        id: &str,
        directory_inode: u64,
        staged_inode: Option<u64>,
    ) -> io::Result<()> {
        let r = self
            .ledger
            .records
            .get_mut(id)
            .filter(|r| &r.scope == scope && r.change == ChangeState::BackupReady)
            .ok_or_else(|| invalid("transaction unavailable"))?;
        let tx = r
            .transaction
            .as_mut()
            .ok_or_else(|| invalid("transaction was not planned"))?;
        let files = tx.identity.inodes_mut()?;
        if files
            .directory_inode
            .is_some_and(|inode| inode != directory_inode)
            || files
                .staged_inode
                .is_some_and(|inode| Some(inode) != staged_inode)
        {
            return Err(invalid("transaction identity changed"));
        }
        files.directory_inode = Some(directory_inode);
        files.staged_inode = staged_inode;
        self.persist()
    }
    pub fn settle_transaction(&mut self, scope: &Scope, id: &str) -> io::Result<bool> {
        let r = self
            .ledger
            .records
            .get(id)
            .filter(|r| &r.scope == scope)
            .ok_or_else(|| invalid("transaction unavailable"))?;
        let Some(tx) = r.transaction.clone() else {
            return Ok(true);
        };
        #[cfg(not(windows))]
        let result = clean(&tx);
        #[cfg(windows)]
        let result = (|| {
            let content = self
                .storage
                .read(&format!("{id}.body"), MAX_TEXT_BYTES as u64)?;
            let metadata = self
                .storage
                .read(&format!("{id}.metadata"), MAX_METADATA_BYTES as u64)?;
            if hash(&content) != r.sha256 {
                return Err(invalid("Windows recovery content changed"));
            }
            crate::windows::clean_transaction(&tx, r.change, &r.file_name, &content, &metadata)
        })();
        let r = self.ledger.records.get_mut(id).unwrap();
        match result {
            Ok(()) => {
                r.transaction = None;
                r.cleanup_error = None;
            }
            Err(e) => {
                r.cleanup_error = Some(format!("transaction cleanup: {:?}", e.kind()));
            }
        }
        let settled = r.transaction.is_none();
        self.persist()?;
        Ok(settled)
    }
}
#[cfg(unix)]
#[path = "transaction_unix.rs"]
mod native;
#[cfg(not(any(unix, windows)))]
#[path = "transaction_unsupported.rs"]
mod native;
#[cfg(not(windows))]
use native::clean;

#[cfg(windows)]
#[path = "transaction_windows_commit.rs"]
mod windows_commit;
#[cfg(windows)]
#[path = "transaction_windows_registration.rs"]
mod windows_registration;
#[cfg(windows)]
pub use windows_commit::{WindowsCommitError, WindowsCommitRequest};
