//! Device-private storage. All methods are blocking and must run off async executors.
//! Callers supply identities only after validating their central authorization lane.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::Path,
};
#[cfg(test)]
use std::{fs, path::PathBuf};

#[cfg(windows)]
#[path = "storage_windows.rs"]
mod storage;
#[cfg(not(windows))]
#[path = "storage_path.rs"]
mod storage;
#[cfg(not(windows))]
mod storage_legacy;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

mod clock_guard;
mod epoch_cleanup;
mod ledger_format;
pub use epoch_cleanup::{EpochCleanupState, EpochCoordinator};
pub mod quota;

mod transaction;
pub use transaction::Transaction;
#[cfg(windows)]
pub use transaction::{WindowsCommitError, WindowsCommitRequest};
mod transaction_identity;
pub use transaction_identity::{InodeIdentity, TransactionIdentity, WindowsIdentity};
#[cfg(test)]
mod transaction_identity_tests;

const MAX_TEXT_BYTES: usize = 65_536;
const MAX_METADATA_BYTES: usize = 256 * 1024;
const MAX_LEDGER_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RECORDS: usize = 10_000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub authority: String,
    pub device: String,
    pub os_user: String,
    pub owner: String,
}
impl Scope {
    fn validate(&self) -> io::Result<()> {
        for value in [&self.authority, &self.device, &self.os_user, &self.owner] {
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err(invalid("invalid trusted recovery scope"));
            }
        }
        Ok(())
    }
    fn key(&self, conversation: &str) -> String {
        format!(
            "{}{}",
            hash(self.os_user.as_bytes()),
            hash(&serde_json::to_vec(&(self, conversation)).expect("scope serialization"))
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub retention_days: u32,
    pub max_bytes: u64,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            retention_days: 7,
            max_bytes: 100 * 1024 * 1024,
        }
    }
}
impl Policy {
    pub fn validate(&self) -> io::Result<()> {
        if !(1..=3650).contains(&self.retention_days)
            || !(1024 * 1024..=1024 * 1024 * 1024 * 10).contains(&self.max_bytes)
        {
            return Err(invalid(
                "retention_days must be 1..3650 and max_bytes must be 1 MiB..10 GiB",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    Preparing,
    BackupReady,
    CommitIntent,
    OutcomeUnknown,
    Succeeded,
    Aborted,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialState {
    Preparing,
    Saved,
    Purging,
    Purged,
}

/// Stable export failures carry no paths or material contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExportError {
    Unavailable,
    Expired,
    Cleaning,
    Cleaned,
    Preparing,
}
impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "recovery export {:?}", self)
    }
}
impl std::error::Error for ExportError {}
fn export_error(reason: ExportError) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, reason)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub scope: Scope,
    pub conversation: String,
    pub operation: String,
    pub generation: String,
    pub file_name: String,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub bytes: u64,
    pub sha256: String,
    pub change: ChangeState,
    pub material: MaterialState,
    pub cleanup_error: Option<String>,
    pub transaction: Option<Transaction>,
    pub device_quota_pending: bool,
    pub device_quota_managed: bool,
    pub device_quota_settled: bool,
    pub discard_requested: bool,
    pub clock_generation: u64,
    pub storage_epoch: u64,
}
#[derive(Serialize, Deserialize)]
struct DeletedConversation {
    deleted_at_ms: u64,
    quota_pending: bool,
    storage_epoch: u64,
}
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    format_version: ledger_format::Version,
    policy: Policy,
    records: BTreeMap<String, Record>,
    deleted_conversations: BTreeMap<String, DeletedConversation>,
    last_cleanup_at_ms: u64,
    cleanup_clock_paused: bool,
    clock_guard: clock_guard::Guard,
    clock_generation: u64,
    execution_epoch: u64,
    epoch_cleanup: Option<EpochCleanupState>,
}

pub struct BackupRequest<'a> {
    pub scope: Scope,
    pub conversation: &'a str,
    pub operation: &'a str,
    pub generation: &'a str,
    pub file_name: &'a str,
    pub content: &'a [u8],
    pub metadata: &'a [u8],
    pub now_ms: u64,
}

pub struct Vault {
    storage: storage::Root,
    #[cfg(test)]
    root: PathBuf,
}
/// The OS lock fences other workers, exports and cleanup until this transaction drops.
pub struct LockedVault {
    storage: storage::Locked,
    ledger: Ledger,
}

#[derive(Debug)]
pub struct CleanupClockError;
impl std::fmt::Display for CleanupClockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "file recovery cleanup paused: system clock changed or could not be verified",
        )
    }
}
impl std::error::Error for CleanupClockError {}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn namespace_user(namespace: &str) -> Option<&str> {
    let user = namespace.get(..64)?;
    let scope = namespace.get(64..)?;
    (valid_id(user) && valid_id(scope)).then_some(user)
}
fn identity(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        Err(invalid("invalid recovery operation identity"))
    } else {
        Ok(())
    }
}

impl Vault {
    /// `data_root` is resolved by the host for the verified worker OS user.
    pub fn open(data_root: &Path) -> io::Result<Self> {
        Ok(Self {
            storage: storage::Root::open(data_root)?,
            #[cfg(test)]
            root: data_root.join("file-recovery"),
        })
    }
    pub fn lock(&self) -> io::Result<LockedVault> {
        self.load_locked(self.storage.lock()?)
    }
    /// Maintenance never waits behind an active file mutation or download.
    pub fn try_lock(&self) -> io::Result<Option<LockedVault>> {
        self.storage
            .try_lock()?
            .map(|lock| self.load_locked(lock))
            .transpose()
    }
    fn load_locked(&self, storage: storage::Locked) -> io::Result<LockedVault> {
        let ledger: Ledger = match storage.read("index.json", MAX_LEDGER_BYTES) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| invalid("recovery index is corrupt; refusing to recreate it"))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                storage.validate_missing_index()?;
                Ledger::default()
            }
            Err(e) => return Err(e),
        };
        ledger.policy.validate()?;
        ledger.clock_guard.validate()?;
        if let Some(state) = ledger.epoch_cleanup
            && (state.previous.checked_add(1) != Some(state.next)
                || ledger.execution_epoch
                    != if state.pruned {
                        state.next
                    } else {
                        state.previous
                    })
        {
            return Err(invalid("recovery epoch cleanup state is corrupt"));
        }
        if ledger.records.len() > MAX_RECORDS {
            return Err(invalid("recovery index exceeds its record bound"));
        }
        for (namespace, deleted) in &ledger.deleted_conversations {
            if namespace_user(namespace).is_none()
                || deleted.deleted_at_ms == 0
                || deleted.storage_epoch > ledger.execution_epoch
            {
                return Err(invalid("recovery conversation index is corrupt"));
            }
        }
        for (id, record) in &ledger.records {
            if record.clock_generation > ledger.clock_generation {
                return Err(invalid("recovery record clock generation is invalid"));
            }
            if record.storage_epoch > ledger.execution_epoch {
                return Err(invalid("recovery record execution epoch is invalid"));
            }
            if id != &record.id || !valid_id(id) {
                return Err(invalid("recovery index identity is corrupt"));
            }
            record.scope.validate()?;
            identity(&record.conversation)?;
            identity(&record.operation)?;
            identity(&record.generation)?;
            if let Some(transaction) = &record.transaction {
                transaction.validate(id)?;
            }
        }
        Ok(LockedVault { storage, ledger })
    }
}
impl LockedVault {
    fn persist(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec(&self.ledger)?;
        if bytes.len() as u64 > MAX_LEDGER_BYTES {
            return Err(invalid("recovery index capacity exhausted"));
        }
        self.storage.write_index(&bytes)
    }
    pub fn policy(&self) -> &Policy {
        &self.ledger.policy
    }
    pub fn set_policy(&mut self, policy: Policy) -> io::Result<()> {
        policy.validate()?;
        for record in self.ledger.records.values_mut() {
            record.expires_at_ms = record
                .created_at_ms
                .saturating_add(u64::from(policy.retention_days) * 86_400_000);
        }
        self.ledger.policy = policy;
        self.persist()
    }
    pub fn used_bytes(&self) -> u64 {
        // Purging materials does not remove the replay fences or conversation
        // tombstones. Count their index space even after the body is gone.
        let index_bytes = serde_json::to_vec(&self.ledger)
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(MAX_LEDGER_BYTES);
        self.ledger
            .records
            .values()
            .filter(|r| r.material != MaterialState::Purged)
            .fold(
                index_bytes.saturating_add(self.storage.extra_used_bytes().unwrap_or(u64::MAX)),
                |total, record| {
                    let indexed_bytes = serde_json::to_vec(record)
                        .map(|bytes| bytes.len() as u64)
                        .unwrap_or(0);
                    total.saturating_add(record.bytes.saturating_sub(indexed_bytes))
                },
            )
    }
    pub fn pending_cleanup_count(&self) -> u64 {
        u64::from(self.cleanup_clock_paused())
            + self
                .ledger
                .records
                .values()
                .filter(|r| {
                    r.cleanup_error.is_some()
                        || r.transaction.is_some()
                        || (r.material == MaterialState::Purged && r.device_quota_pending)
                })
                .count() as u64
            + self
                .ledger
                .deleted_conversations
                .values()
                .filter(|deleted| deleted.quota_pending)
                .count() as u64
    }
    pub fn cleanup_clock_paused(&self) -> bool {
        self.ledger.cleanup_clock_paused || self.ledger.clock_guard.blocked
    }
    /// Production callers sample the OS clock before maintenance or mutation.
    /// Read-only management may still return records while cleanup is paused.
    pub fn observe_system_clock(&mut self) -> io::Result<()> {
        self.ledger.clock_guard.observe_system();
        self.persist()
    }
    /// Owner explicitly accepts the displayed device time. This only changes
    /// the clock fence, never original record timestamps or mutation outcomes.
    pub fn acknowledge_clock(&mut self, displayed_wall_ms: u64) -> io::Result<()> {
        if !self.cleanup_clock_paused() {
            return Ok(());
        }
        let generation = self
            .ledger
            .clock_generation
            .checked_add(1)
            .ok_or_else(|| invalid("recovery clock generation exhausted"))?;
        let now = self
            .ledger
            .clock_guard
            .acknowledge_system(displayed_wall_ms)?;
        self.ledger.clock_generation = generation;
        self.ledger.last_cleanup_at_ms = now;
        self.ledger.cleanup_clock_paused = false;
        self.persist()
    }
    pub fn unknown_outcome_count(&self, scope: Option<&Scope>) -> u64 {
        self.ledger
            .records
            .values()
            .filter(|record| {
                scope.is_none_or(|scope| &record.scope == scope)
                    && matches!(
                        record.change,
                        ChangeState::CommitIntent | ChangeState::OutcomeUnknown
                    )
                    && record.material != MaterialState::Purged
            })
            .count() as u64
    }
    pub fn oldest_pending_created_at(&self, scope: Option<&Scope>) -> Option<u64> {
        self.oldest_pending_record(scope, None)
            .map(|record| record.created_at_ms)
    }
    pub fn oldest_pending_record(
        &self,
        scope: Option<&Scope>,
        conversation: Option<&str>,
    ) -> Option<Record> {
        self.ledger
            .records
            .values()
            .filter(|record| {
                scope.is_none_or(|scope| &record.scope == scope)
                    && conversation.is_none_or(|conversation| record.conversation == conversation)
                    && ((record.device_quota_managed
                        && !record.device_quota_settled
                        && record.material == MaterialState::Saved)
                        || record.cleanup_error.is_some()
                        || record.transaction.is_some()
                        || (record.material == MaterialState::Purged
                            && record.device_quota_pending)
                        || (record.change == ChangeState::OutcomeUnknown
                            && record.material != MaterialState::Purged))
            })
            .min_by_key(|record| (record.created_at_ms, &record.id))
            .cloned()
    }
    pub fn list(&self, scope: &Scope, conversation: Option<&str>) -> Vec<Record> {
        self.ledger
            .records
            .values()
            .filter(|r| &r.scope == scope && conversation.is_none_or(|c| c == r.conversation))
            .cloned()
            .collect()
    }
    /// Original-OS-user recovery only. Never expose this through a remote route:
    /// remote callers must use `list` and an authenticated, frozen Scope.
    pub fn local_records(&self, after: Option<&str>) -> io::Result<Vec<Record>> {
        if after.is_some_and(|id| !valid_id(id)) {
            return Err(invalid("invalid recovery page cursor"));
        }
        Ok(self
            .ledger
            .records
            .values()
            .filter(|r| {
                r.material != MaterialState::Purged && after.is_none_or(|id| r.id.as_str() > id)
            })
            .take(101)
            .cloned()
            .collect())
    }
    /// Authenticated local OS owner may retrieve old credential-domain material.
    pub fn export_local_package(&self, id: &str, now_ms: u64) -> io::Result<Vec<u8>> {
        let scope = &self
            .ledger
            .records
            .get(id)
            .ok_or_else(|| invalid("recovery material unavailable"))?
            .scope;
        self.export_package(scope, id, now_ms)
    }
    /// Local owner authentication is enforced by the loopback controller.
    pub fn discard_local(&mut self, conversation: &str, id: &str, now_ms: u64) -> io::Result<()> {
        let scope = self
            .ledger
            .records
            .get(id)
            .ok_or_else(|| invalid("recovery material unavailable"))?
            .scope
            .clone();
        self.discard(&scope, conversation, id, now_ms)
    }
    pub fn cleanup_complete(&self, scope: &Scope, conversation: &str) -> bool {
        self.ledger
            .deleted_conversations
            .get(&scope.key(conversation))
            .is_some_and(|deleted| !deleted.quota_pending)
            && self.ledger.records.values().all(|r| {
                &r.scope != scope
                    || r.conversation != conversation
                    || (r.material == MaterialState::Purged && !r.device_quota_pending)
            })
    }

    pub fn is_deleted(&self, scope: &Scope, conversation: &str) -> bool {
        self.ledger
            .deleted_conversations
            .contains_key(&scope.key(conversation))
    }
    /// A current connection cannot claim an old namespace's material was purged.
    pub fn has_foreign_conversation(&self, scope: &Scope, conversation: &str) -> bool {
        self.ledger.records.values().any(|r| {
            r.conversation == conversation
                && r.material != MaterialState::Purged
                && &r.scope != scope
        })
    }
    /// Caller must hold this lock until the file action has left its commit region.
    pub fn backup(&mut self, request: BackupRequest<'_>) -> io::Result<Record> {
        self.backup_inner(request, 0, false, |_| Ok(()))
    }
    #[cfg(test)]
    fn backup_with_reservation(
        &mut self,
        request: BackupRequest<'_>,
        reserve: impl FnOnce(&Record) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.backup_inner(request, 0, true, reserve)
    }
    pub fn backup_with_reservation_in_epoch(
        &mut self,
        request: BackupRequest<'_>,
        expected_epoch: u64,
        reserve: impl FnOnce(&Record) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.backup_inner(request, expected_epoch, true, reserve)
    }
    fn backup_inner(
        &mut self,
        request: BackupRequest<'_>,
        expected_epoch: u64,
        device_quota_pending: bool,
        reserve: impl FnOnce(&Record) -> io::Result<()>,
    ) -> io::Result<Record> {
        self.require_execution_epoch(expected_epoch)?;
        let BackupRequest {
            scope,
            conversation,
            operation,
            generation,
            file_name,
            content,
            metadata,
            now_ms,
        } = request;
        scope.validate()?;
        for value in [conversation, operation, generation] {
            identity(value)?;
        }
        if file_name.is_empty()
            || file_name.len() > 512
            || file_name
                .chars()
                .any(|c| c.is_control() || c == '/' || c == '\\')
        {
            return Err(invalid("invalid recovery file name"));
        }
        if content.len() > MAX_TEXT_BYTES
            || metadata.len() > MAX_METADATA_BYTES
            || content.contains(&0)
            || std::str::from_utf8(content).is_err()
        {
            return Err(invalid("recovery text or metadata exceeds its bound"));
        }
        if self.is_deleted(&scope, conversation) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "conversation was deleted",
            ));
        }
        let id = hash(&serde_json::to_vec(&(&scope, conversation, operation))?);
        // An existing operation is never permission to repeat its file mutation.
        if self.ledger.records.contains_key(&id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "recovery operation already exists; do not repeat the file change",
            ));
        }
        let bytes = (content.len() + metadata.len()) as u64;
        if self.ledger.records.len() >= MAX_RECORDS
            || self.used_bytes().saturating_add(bytes) > self.ledger.policy.max_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "file backup capacity exhausted; file was not changed",
            ));
        }
        let mut record = Record {
            id: id.clone(),
            scope,
            conversation: conversation.into(),
            operation: operation.into(),
            generation: generation.into(),
            file_name: file_name.into(),
            created_at_ms: now_ms,
            expires_at_ms: now_ms
                .saturating_add(u64::from(self.ledger.policy.retention_days) * 86_400_000),
            bytes,
            sha256: hash(content),
            change: ChangeState::Preparing,
            material: MaterialState::Preparing,
            cleanup_error: None,
            transaction: None,
            device_quota_pending,
            device_quota_managed: device_quota_pending,
            device_quota_settled: false,
            discard_requested: false,
            clock_generation: self.ledger.clock_generation,
            storage_epoch: self.ledger.execution_epoch,
        };
        // A 4096-byte parent path can expand to six JSON bytes per byte.
        // Reserve the bounded transaction metadata before writing any backup.
        record.bytes += serde_json::to_vec(&record)?.len() as u64 + 32 * 1024;
        // The new map key and separator also occupy index space.
        let index_key_bytes = id.len() as u64 + 4;
        if self
            .used_bytes()
            .saturating_add(record.bytes)
            .saturating_add(index_key_bytes)
            > self.ledger.policy.max_bytes
        {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "file backup capacity exhausted; file was not changed",
            ));
        }
        self.ledger.records.insert(id.clone(), record);
        self.persist()?;
        // The index owns these exact generated paths before any material exists.
        let result = (|| {
            // The durable local record exists before requesting shared quota,
            // including when the response is lost after the daemon reserves it.
            reserve(&self.ledger.records[&id])?;
            self.storage
                .write_material(&format!("{id}.body"), content)?;
            self.storage
                .write_material(&format!("{id}.metadata"), metadata)
        })();
        if let Err(error) = result {
            let record = self.ledger.records.get_mut(&id).unwrap();
            record.change = ChangeState::Aborted;
            record.expires_at_ms = now_ms;
            let _ = self.persist();
            return Err(error);
        }
        let record = self.ledger.records.get_mut(&id).unwrap();
        record.change = ChangeState::BackupReady;
        record.material = MaterialState::Saved;
        self.persist()?;
        Ok(self.ledger.records[&id].clone())
    }
    pub fn pending_quota_settlement(&self) -> Vec<Record> {
        self.ledger
            .records
            .values()
            .filter(|record| {
                record.device_quota_managed
                    && !record.device_quota_settled
                    && record.material == MaterialState::Saved
            })
            .cloned()
            .collect()
    }
    pub fn acknowledge_quota_settlement(&mut self, scope: &Scope, id: &str) -> io::Result<()> {
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|r| {
                &r.scope == scope && r.device_quota_managed && r.material == MaterialState::Saved
            })
            .ok_or_else(|| invalid("quota settlement record unavailable"))?;
        record.device_quota_settled = true;
        if let Err(error) = self.persist() {
            self.ledger
                .records
                .get_mut(id)
                .unwrap()
                .device_quota_settled = false;
            return Err(error);
        }
        Ok(())
    }
    pub fn pending_quota_cleanup(&self) -> Vec<Record> {
        self.ledger
            .records
            .values()
            .filter(|record| {
                record.device_quota_pending
                    && record.material == MaterialState::Purged
                    && record.transaction.is_none()
            })
            .cloned()
            .collect()
    }
    /// Compact tombstones are retried after all per-operation cleanup has settled.
    pub fn pending_quota_namespaces(&self) -> Vec<String> {
        self.ledger
            .deleted_conversations
            .iter()
            .filter(|(namespace, deleted)| {
                deleted.quota_pending
                    && !self
                        .ledger
                        .records
                        .values()
                        .any(|record| record.scope.key(&record.conversation) == **namespace)
            })
            .map(|(namespace, _)| namespace.clone())
            .collect()
    }
    pub fn acknowledge_quota_namespace(&mut self, namespace: &str) -> io::Result<()> {
        if !self
            .pending_quota_namespaces()
            .iter()
            .any(|pending| pending == namespace)
        {
            return Err(invalid("quota namespace cleanup is not settled"));
        }
        self.ledger
            .deleted_conversations
            .get_mut(namespace)
            .unwrap()
            .quota_pending = false;
        self.persist()
    }
    pub fn retained_index_bytes(record: &Record) -> io::Result<u64> {
        // Include the map key and the pending flag's final serialized size.
        Ok(serde_json::to_vec(record)?.len() as u64 + record.id.len() as u64 + 8)
    }
    pub fn acknowledge_quota_cleanup(&mut self, scope: &Scope, id: &str) -> io::Result<()> {
        let record = self
            .ledger
            .records
            .get_mut(id)
            .ok_or_else(|| invalid("quota cleanup record missing"))?;
        if record.scope != *scope
            || record.material != MaterialState::Purged
            || record.transaction.is_some()
        {
            return Err(invalid("quota cleanup record is not settled"));
        }
        record.device_quota_pending = false;
        if self
            .ledger
            .deleted_conversations
            .contains_key(&scope.key(&record.conversation))
        {
            self.ledger.records.remove(id);
        }
        self.persist()
    }

    /// Called under the exclusive lock after a worker restart. Missing syscall
    /// results become terminal unknown outcomes, never success or failure guesses.
    /// Unknown outcomes retain their materials until explicit discard; the journal fences replay.
    pub fn recover_interrupted(&mut self, now_ms: u64) -> io::Result<Vec<Record>> {
        self.check_cleanup_clock(now_ms)?;
        let mut unknown = Vec::new();
        for record in self.ledger.records.values_mut() {
            match record.change {
                ChangeState::Preparing | ChangeState::BackupReady => {
                    record.change = ChangeState::Aborted;
                    record.expires_at_ms = now_ms;
                }
                ChangeState::CommitIntent => {
                    record.change = ChangeState::OutcomeUnknown;
                    unknown.push(record.clone());
                }
                ChangeState::OutcomeUnknown => {
                    if record.material != MaterialState::Purged {
                        unknown.push(record.clone());
                    }
                }
                ChangeState::Succeeded | ChangeState::Aborted => (),
            }
        }
        self.persist()?;
        // A failed immediate cleanup must be retried before expiry, too.
        // Cleanup only registered temporary objects, never replay the target mutation.
        let settled: Vec<_> = self
            .ledger
            .records
            .values()
            .filter(|r| {
                r.transaction.is_some()
                    && matches!(
                        r.change,
                        ChangeState::Succeeded | ChangeState::Aborted | ChangeState::OutcomeUnknown
                    )
            })
            .map(|r| (r.scope.clone(), r.id.clone()))
            .collect();
        for (scope, id) in settled {
            self.settle_transaction(&scope, &id)?;
        }
        self.cleanup(now_ms)?;
        unknown.retain(|record| {
            self.ledger
                .records
                .get(&record.id)
                .is_some_and(|current| current.material != MaterialState::Purged)
        });
        Ok(unknown)
    }

    pub fn transition(&mut self, scope: &Scope, id: &str, next: ChangeState) -> io::Result<()> {
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|r| &r.scope == scope)
            .ok_or_else(|| invalid("recovery operation unavailable"))?;
        let allowed = matches!(
            (record.change, next),
            (
                ChangeState::BackupReady,
                ChangeState::CommitIntent | ChangeState::Aborted
            ) | (
                ChangeState::CommitIntent,
                ChangeState::Succeeded | ChangeState::Aborted
            )
        );
        if !allowed {
            return Err(invalid("invalid recovery operation transition"));
        }
        record.change = next;
        self.persist()
    }
    pub fn delete_conversation(
        &mut self,
        scope: &Scope,
        conversation: &str,
        now_ms: u64,
    ) -> io::Result<()> {
        self.delete_conversation_inner(scope, conversation, now_ms, false)
    }
    pub fn delete_conversation_with_quota(
        &mut self,
        scope: &Scope,
        conversation: &str,
        now_ms: u64,
    ) -> io::Result<()> {
        self.delete_conversation_inner(scope, conversation, now_ms, true)
    }
    fn delete_conversation_inner(
        &mut self,
        scope: &Scope,
        conversation: &str,
        now_ms: u64,
        shared: bool,
    ) -> io::Result<()> {
        scope.validate()?;
        identity(conversation)?;
        if now_ms == 0 {
            return Err(invalid("invalid recovery deletion time"));
        }
        let namespace = scope.key(conversation);
        let pending = shared
            || self.ledger.records.values().any(|record| {
                record.scope == *scope
                    && record.conversation == conversation
                    && record.device_quota_managed
            })
            || self
                .ledger
                .deleted_conversations
                .get(&namespace)
                .is_some_and(|deleted| deleted.quota_pending);
        self.ledger
            .deleted_conversations
            .entry(namespace)
            .and_modify(|deleted| deleted.quota_pending |= pending)
            .or_insert(DeletedConversation {
                deleted_at_ms: now_ms,
                quota_pending: pending,
                storage_epoch: self.ledger.execution_epoch,
            });
        self.persist()?;
        self.cleanup(now_ms)
    }
    pub fn export(
        &self,
        scope: &Scope,
        id: &str,
        now_ms: u64,
    ) -> io::Result<(Record, Vec<u8>, Vec<u8>)> {
        if !valid_id(id) {
            return Err(invalid("invalid recovery id"));
        }
        let record = self
            .ledger
            .records
            .get(id)
            .filter(|r| &r.scope == scope && !self.is_deleted(scope, &r.conversation))
            .ok_or_else(|| export_error(ExportError::Unavailable))?;
        match record.material {
            MaterialState::Purged => return Err(export_error(ExportError::Cleaned)),
            MaterialState::Purging => return Err(export_error(ExportError::Cleaning)),
            MaterialState::Preparing => return Err(export_error(ExportError::Preparing)),
            MaterialState::Saved => (),
        }
        if record.discard_requested {
            return Err(export_error(ExportError::Cleaning));
        }
        if record.expires_at_ms <= now_ms && record.change != ChangeState::OutcomeUnknown {
            return Err(export_error(ExportError::Expired));
        }
        Ok((
            record.clone(),
            self.storage
                .read(&format!("{id}.body"), MAX_TEXT_BYTES as u64)?,
            self.storage
                .read(&format!("{id}.metadata"), MAX_METADATA_BYTES as u64)?,
        ))
    }
    /// Fixed archive names prevent original paths from controlling extraction.
    /// Keep the exclusive vault lock until the response bytes have been built.
    pub fn export_package(&self, scope: &Scope, id: &str, now_ms: u64) -> io::Result<Vec<u8>> {
        let (record, content, metadata) = self.export(scope, id, now_ms)?;
        let filesystem: serde_json::Value = serde_json::from_slice(&metadata)
            .map_err(|_| invalid("backup metadata is unavailable"))?;
        let manifest = serde_json::json!({
            "recovery_id": record.id,
            "file_name": record.file_name,
            "created_at_unix_ms": record.created_at_ms,
            "expires_at_unix_ms": record.expires_at_ms,
            "content_sha256": record.sha256,
            "change_state": record.change,
            "filesystem": filesystem,
            "restoration": "Content and metadata export only; no automatic restoration of ownership, ACLs, or the original path."
        });
        let mut archive = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o600);
        archive
            .start_file("before.txt", options)
            .map_err(io::Error::other)?;
        archive.write_all(&content)?;
        archive
            .start_file("metadata.json", options)
            .map_err(io::Error::other)?;
        archive.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
        Ok(archive.finish().map_err(io::Error::other)?.into_inner())
    }
    /// Explicit owner confirmation only abandons recovery material. It never
    /// changes the recorded outcome or replays a mutation of the user's file.
    pub fn discard(
        &mut self,
        scope: &Scope,
        conversation: &str,
        id: &str,
        now_ms: u64,
    ) -> io::Result<()> {
        self.check_cleanup_clock(now_ms)?;
        let record = self
            .ledger
            .records
            .get_mut(id)
            .filter(|record| &record.scope == scope && record.conversation == conversation)
            .ok_or_else(|| invalid("recovery material unavailable"))?;
        if !matches!(
            record.change,
            ChangeState::Succeeded | ChangeState::Aborted | ChangeState::OutcomeUnknown
        ) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "recovery operation has not settled",
            ));
        }
        record.discard_requested = true;
        self.persist()?;
        self.cleanup(now_ms)
    }

    /// Persist the clock fence across worker restarts. Never lower it on retry:
    /// retrying cleanup is not permission to discard material after a clock reset.
    fn check_cleanup_clock(&mut self, now_ms: u64) -> io::Result<()> {
        if self.ledger.clock_guard.initialized() {
            self.ledger.clock_guard.observe_system();
        }
        let latest_creation = self
            .ledger
            .records
            .values()
            .filter(|record| record.clock_generation == self.ledger.clock_generation)
            .map(|record| record.created_at_ms)
            .max()
            .unwrap_or(0);
        if self.ledger.clock_guard.blocked
            || now_ms == 0
            || now_ms < self.ledger.last_cleanup_at_ms.max(latest_creation)
        {
            self.ledger.cleanup_clock_paused = true;
            self.persist()?;
            return Err(io::Error::other(CleanupClockError));
        }
        self.ledger.last_cleanup_at_ms = now_ms;
        self.ledger.cleanup_clock_paused = false;
        self.persist()
    }

    /// API success is sufficient; cleanup does not perform post-delete stat calls.
    pub fn cleanup(&mut self, now_ms: u64) -> io::Result<()> {
        self.check_cleanup_clock(now_ms)?;
        let ids: Vec<_> = self
            .ledger
            .records
            .values()
            .filter(|r| {
                r.material != MaterialState::Purged
                    && (r.discard_requested
                        || self.is_deleted(&r.scope, &r.conversation)
                        || (r.expires_at_ms <= now_ms
                            && matches!(r.change, ChangeState::Succeeded | ChangeState::Aborted)))
            })
            .map(|r| r.id.clone())
            .collect();
        for id in ids {
            let scope = self.ledger.records[&id].scope.clone();
            if !self.settle_transaction(&scope, &id)? {
                continue;
            }
            if !valid_id(&id) {
                return Err(invalid("invalid stored recovery id"));
            }
            // No caller can still be committing while this exclusive lock is held.
            self.ledger.records.get_mut(&id).unwrap().material = MaterialState::Purging;
            self.persist()?;
            let result = self.storage.remove_record_material(&id);
            let record = self.ledger.records.get_mut(&id).unwrap();
            match result {
                Ok(()) => {
                    record.material = MaterialState::Purged;
                    record.cleanup_error = None;
                }
                Err(e) => {
                    record.cleanup_error = Some(format!("{:?}", e.kind()));
                }
            }
            self.persist()?;
        }
        // The conversation tombstone fences every old operation, so completed
        // per-file records can be dropped without permitting a replay.
        let deleted = &self.ledger.deleted_conversations;
        self.ledger.records.retain(|_, record| {
            record.material != MaterialState::Purged
                || record.device_quota_pending
                || !deleted.contains_key(&record.scope.key(&record.conversation))
        });
        self.persist()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
