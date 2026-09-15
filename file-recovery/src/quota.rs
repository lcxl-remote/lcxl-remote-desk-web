//! Daemon-owned device quota. Workers never open this directory directly.
//! An expired execution deadline does not prove that its backup was removed.
use super::{hash, identity, invalid, storage};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, path::Path};

const MAX_ENTRIES: usize = 10_000;
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
// Bounded identity hashes, generation and timestamps are included in quota.
const ENTRY_BYTES: u64 = 1024;
const NAMESPACE_BYTES: u64 = 1024;
const USER_EPOCH_BYTES: u64 = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaKey {
    pub namespace: String,
    pub os_user: String,
    pub operation: String,
    pub generation: String,
    pub epoch: u64,
}
impl QuotaKey {
    pub fn new(
        scope: &super::Scope,
        conversation: &str,
        operation: &str,
        generation: &str,
    ) -> io::Result<Self> {
        scope.validate()?;
        for value in [conversation, operation, generation] {
            identity(value)?;
        }
        Ok(Self {
            namespace: scope.key(conversation),
            os_user: hash(scope.os_user.as_bytes()),
            operation: hash(&serde_json::to_vec(&(scope, conversation, operation))?),
            generation: hash(generation.as_bytes()),
            epoch: 0,
        })
    }
    fn validate(&self) -> io::Result<()> {
        if super::namespace_user(&self.namespace) == Some(self.os_user.as_str())
            && [&self.operation, &self.generation, &self.os_user]
                .iter()
                .all(|value| super::valid_id(value))
        {
            Ok(())
        } else {
            Err(invalid("invalid device quota identity"))
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    key: QuotaKey,
    bytes: u64,
    execution_deadline_ms: u64,
    released: bool,
    settled: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochStatus {
    pub epoch: u64,
    pub cleanup_pending: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct ClosedNamespace {
    user: String,
    epoch: u64,
}
#[derive(Clone, Serialize, Deserialize)]
struct Ledger {
    max_bytes: u64,
    retention_days: u32,
    entries: BTreeMap<String, Entry>,
    closed_namespaces: BTreeMap<String, ClosedNamespace>,
    user_epochs: BTreeMap<String, EpochStatus>,
}
impl Default for Ledger {
    fn default() -> Self {
        Self {
            max_bytes: super::Policy::default().max_bytes,
            retention_days: super::Policy::default().retention_days,
            entries: BTreeMap::new(),
            closed_namespaces: BTreeMap::new(),
            user_epochs: BTreeMap::new(),
        }
    }
}

pub struct DeviceQuota {
    storage: storage::Root,
}
pub struct LockedDeviceQuota {
    storage: storage::Locked,
    ledger: Ledger,
    failed: bool,
}
impl DeviceQuota {
    /// Root belongs to the daemon's trusted runtime, never a worker-supplied path.
    pub fn open(daemon_data_root: &Path) -> io::Result<Self> {
        Ok(Self {
            storage: storage::Root::open_named(daemon_data_root, "file-recovery-quota")?,
        })
    }
    pub fn lock(&self) -> io::Result<LockedDeviceQuota> {
        let storage = self.storage.lock()?;
        let ledger: Ledger = match storage.read("index.json", MAX_INDEX_BYTES) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| invalid("device quota index is corrupt"))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                storage.validate_missing_index()?;
                Ledger::default()
            }
            Err(error) => return Err(error),
        };
        super::Policy {
            retention_days: ledger.retention_days,
            max_bytes: ledger.max_bytes,
        }
        .validate()?;
        if ledger.entries.len() > MAX_ENTRIES
            || ledger.closed_namespaces.len() > MAX_ENTRIES
            || ledger.user_epochs.len() > MAX_ENTRIES
        {
            return Err(invalid("device quota index exceeds its bound"));
        }
        for (user, state) in &ledger.user_epochs {
            if !super::valid_id(user) || state.epoch == 0 {
                return Err(invalid("device quota epoch index is corrupt"));
            }
        }
        for (namespace, closed) in &ledger.closed_namespaces {
            if super::namespace_user(namespace) != Some(closed.user.as_str())
                || closed.epoch
                    > ledger
                        .user_epochs
                        .get(&closed.user)
                        .copied()
                        .unwrap_or_default()
                        .epoch
            {
                return Err(invalid("device quota namespace index is corrupt"));
            }
        }
        for (operation, entry) in &ledger.entries {
            entry.key.validate()?;
            if operation != &entry.key.operation
                || ledger
                    .closed_namespaces
                    .get(&entry.key.namespace)
                    .is_some_and(|closed| closed.epoch == entry.key.epoch)
                || entry.key.epoch
                    > ledger
                        .user_epochs
                        .get(&entry.key.os_user)
                        .copied()
                        .unwrap_or_default()
                        .epoch
                || entry.execution_deadline_ms == 0
                || entry.bytes < ENTRY_BYTES
            {
                return Err(invalid("device quota index identity is corrupt"));
            }
        }
        Ok(LockedDeviceQuota {
            storage,
            ledger,
            failed: false,
        })
    }
}
fn validate_limit(max_bytes: u64) -> io::Result<()> {
    super::Policy {
        retention_days: 7,
        max_bytes,
    }
    .validate()
}
impl LockedDeviceQuota {
    pub fn epoch(&self, os_user: &str) -> io::Result<EpochStatus> {
        identity(os_user)?;
        Ok(self
            .ledger
            .user_epochs
            .get(&hash(os_user.as_bytes()))
            .copied()
            .unwrap_or_default())
    }
    /// Close the old execution epoch before the worker removes any replay
    /// indexes. Their quota remains charged until finish_epoch_cleanup.
    pub fn begin_epoch_cleanup(&mut self, os_user: &str, expected: u64) -> io::Result<EpochStatus> {
        self.require_healthy()?;
        let current = self.epoch(os_user)?;
        let epoch = expected
            .checked_add(1)
            .ok_or_else(|| invalid("quota epoch exhausted"))?;
        if current.epoch == epoch {
            return Ok(current);
        }
        if current.epoch != expected {
            return Err(invalid("quota epoch changed"));
        }
        if current.cleanup_pending {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "quota epoch cleanup is pending",
            ));
        }
        let user = hash(os_user.as_bytes());
        if !self.ledger.user_epochs.contains_key(&user)
            && self.ledger.user_epochs.len() >= MAX_ENTRIES
        {
            return Err(invalid("quota user epoch index exhausted"));
        }
        let state = EpochStatus {
            epoch,
            cleanup_pending: true,
        };
        let mut next = self.ledger.clone();
        next.user_epochs.insert(user, state);
        self.persist(next)?;
        Ok(state)
    }
    /// The original worker has durably pruned every eligible old local index.
    /// Unreleased material remains charged, even when it belongs to an old epoch.
    pub fn finish_epoch_cleanup(&mut self, os_user: &str, epoch: u64) -> io::Result<EpochStatus> {
        self.require_healthy()?;
        let current = self.epoch(os_user)?;
        if current.epoch != epoch || epoch == 0 {
            return Err(invalid("quota epoch changed"));
        }
        if !current.cleanup_pending {
            return Ok(current);
        }
        let user = hash(os_user.as_bytes());
        let mut next = self.ledger.clone();
        next.entries.retain(|_, entry| {
            entry.key.os_user != user || entry.key.epoch >= epoch || !entry.released
        });
        next.closed_namespaces
            .retain(|_, closed| closed.user != user || closed.epoch >= epoch);
        let state = EpochStatus {
            epoch,
            cleanup_pending: false,
        };
        next.user_epochs.insert(user, state);
        self.persist(next)?;
        Ok(state)
    }
    fn require_healthy(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::other(
                "device quota write failed; reopen before further operations",
            ))
        } else {
            Ok(())
        }
    }
    pub fn policy(&self) -> super::Policy {
        super::Policy {
            retention_days: self.ledger.retention_days,
            max_bytes: self.ledger.max_bytes,
        }
    }
    pub fn set_policy(&mut self, policy: super::Policy) -> io::Result<()> {
        policy.validate()?;
        let mut next = self.ledger.clone();
        next.max_bytes = policy.max_bytes;
        next.retention_days = policy.retention_days;
        self.persist(next)
    }
    pub fn max_bytes(&self) -> u64 {
        self.ledger.max_bytes
    }
    pub fn used_bytes(&self) -> u64 {
        self.ledger.entries.values().fold(
            self.ledger.closed_namespaces.len() as u64 * NAMESPACE_BYTES
                + self.ledger.user_epochs.len() as u64 * USER_EPOCH_BYTES,
            |used, entry| used.saturating_add(entry.bytes),
        )
    }
    /// Unsettled reservations are part of used_bytes, not additional usage.
    /// Timeouts cannot prove that the worker has stopped using these bytes.
    pub fn reserved_bytes(&self) -> u64 {
        self.ledger
            .entries
            .values()
            .filter(|entry| !entry.released && !entry.settled)
            .fold(0, |total, entry| total.saturating_add(entry.bytes))
    }
    /// Publish the new in-memory state only after its durable write succeeds.
    fn persist(&mut self, next: Ledger) -> io::Result<()> {
        let bytes = serde_json::to_vec(&next)?;
        if bytes.len() as u64 > MAX_INDEX_BYTES {
            return Err(invalid("device quota index capacity exhausted"));
        }
        self.require_healthy()?;
        if let Err(error) = self.storage.write_index(&bytes) {
            // Rename may have succeeded before a later sync failure. Never
            // continue allocating from a potentially stale in-memory ledger.
            self.failed = true;
            return Err(error);
        }
        self.ledger = next;
        Ok(())
    }
    /// Lower limits stop new reservations; existing backup materials are retained.
    pub fn set_limit(&mut self, max_bytes: u64) -> io::Result<()> {
        validate_limit(max_bytes)?;
        let mut next = self.ledger.clone();
        next.max_bytes = max_bytes;
        self.persist(next)
    }
    /// Reserve the upper bound before allowing a worker to create a backup.
    /// An idempotent transport retry cannot increase or change a reservation.
    pub fn reserve(
        &mut self,
        key: QuotaKey,
        material_bytes: u64,
        execution_deadline_ms: u64,
        now_ms: u64,
    ) -> io::Result<()> {
        self.require_healthy()?;
        key.validate()?;
        let epoch = self
            .ledger
            .user_epochs
            .get(&key.os_user)
            .copied()
            .unwrap_or_default();
        if key.epoch != epoch.epoch || epoch.cleanup_pending {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution epoch is closed or being cleaned",
            ));
        }
        if now_ms == 0 || execution_deadline_ms <= now_ms || material_bytes == 0 {
            return Err(invalid("invalid device quota reservation"));
        }
        if self
            .ledger
            .closed_namespaces
            .get(&key.namespace)
            .is_some_and(|closed| closed.epoch == key.epoch)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "conversation backup quota is closed",
            ));
        }
        let bytes = material_bytes
            .checked_add(ENTRY_BYTES)
            .ok_or_else(|| invalid("device quota size overflow"))?;
        if let Some(existing) = self.ledger.entries.get(&key.operation) {
            return if existing.key == key
                && existing.bytes == bytes
                && existing.execution_deadline_ms == execution_deadline_ms
                && !existing.released
            {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "device quota operation already recorded",
                ))
            };
        }
        let mut next = self.ledger.clone();
        // Only completed entries can expire. No amount of elapsed time proves
        // that an unacknowledged write or backup no longer occupies disk.
        next.entries.retain(|_, entry| {
            !entry.released || entry.bytes > ENTRY_BYTES || entry.execution_deadline_ms >= now_ms
        });
        let used = next.entries.values().fold(
            next.closed_namespaces.len() as u64 * NAMESPACE_BYTES
                + next.user_epochs.len() as u64 * USER_EPOCH_BYTES,
            |used, entry| used.saturating_add(entry.bytes),
        );
        if next.entries.len() >= MAX_ENTRIES || used.saturating_add(bytes) > next.max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "device file backup capacity exhausted; file was not changed",
            ));
        }
        next.entries.insert(
            key.operation.clone(),
            Entry {
                key,
                bytes,
                execution_deadline_ms,
                released: false,
                settled: false,
            },
        );
        self.persist(next)
    }
    /// Settle to the measured charge from the original worker. Growing beyond
    /// the reserved upper bound is forbidden, including during policy changes.
    pub fn settle(&mut self, key: &QuotaKey, material_bytes: u64) -> io::Result<()> {
        self.require_healthy()?;
        key.validate()?;
        let entry = self
            .ledger
            .entries
            .get(&key.operation)
            .ok_or_else(|| invalid("device quota reservation is missing"))?;
        let bytes = material_bytes
            .checked_add(ENTRY_BYTES)
            .ok_or_else(|| invalid("device quota size overflow"))?;
        if entry.key != *key || entry.released || bytes > entry.bytes {
            return Err(invalid(
                "device quota settlement does not match reservation",
            ));
        }
        let mut next = self.ledger.clone();
        let entry = next.entries.get_mut(&key.operation).unwrap();
        entry.bytes = bytes;
        entry.settled = true;
        self.persist(next)
    }
    /// Caller must authenticate an explicit cleanup completion (or definite
    /// cancellation before any material was created), never a timeout.
    pub fn release(&mut self, key: &QuotaKey) -> io::Result<()> {
        self.release_with_retained_index(key, 0)
    }
    /// A purged body may still have a replay index in the user's vault.
    /// Keep that index charged until conversation-level cleanup is confirmed.
    pub fn release_with_retained_index(
        &mut self,
        key: &QuotaKey,
        retained_bytes: u64,
    ) -> io::Result<()> {
        self.require_healthy()?;
        key.validate()?;
        let current = self
            .ledger
            .user_epochs
            .get(&key.os_user)
            .copied()
            .unwrap_or_default();
        if key.epoch > current.epoch {
            return Err(invalid("quota cleanup epoch is invalid"));
        }
        if let Some(closed) = self
            .ledger
            .closed_namespaces
            .get(&key.namespace)
            .filter(|closed| closed.epoch == key.epoch)
        {
            return if closed.user == key.os_user {
                Ok(())
            } else {
                Err(invalid("device quota namespace owner changed"))
            };
        }
        let retained = retained_bytes
            .checked_add(ENTRY_BYTES)
            .ok_or_else(|| invalid("device quota size overflow"))?;
        let Some(entry) = self.ledger.entries.get(&key.operation) else {
            if key.epoch < current.epoch {
                return Ok(());
            }
            // A cleanup RPC may overtake a delayed reservation handler. Record
            // cancellation even when its charge has not appeared yet.
            if self.ledger.entries.len() >= MAX_ENTRIES {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "device quota cleanup index exhausted",
                ));
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_millis() as u64;
            let mut next = self.ledger.clone();
            next.entries.insert(
                key.operation.clone(),
                Entry {
                    key: key.clone(),
                    bytes: retained,
                    execution_deadline_ms: now.saturating_add(60_000),
                    released: true,
                    settled: true,
                },
            );
            return self.persist(next);
        };
        if entry.key != *key {
            return Err(invalid("device quota cleanup identity changed"));
        }
        if retained > entry.bytes {
            return Err(invalid("retained quota exceeds reservation"));
        }
        if entry.released && entry.bytes == retained {
            return Ok(());
        }
        let mut next = self.ledger.clone();
        let entry = next.entries.get_mut(&key.operation).unwrap();
        entry.released = true;
        entry.bytes = retained;
        self.persist(next)
    }
    /// Called only after the original worker has removed this conversation's
    /// material and operation indexes. The compact conversation fence remains charged.
    #[cfg(test)]
    fn release_namespace(&mut self, namespace: &str, os_user: &str) -> io::Result<()> {
        self.release_namespace_at_epoch(namespace, os_user, 0)
    }
    pub fn release_namespace_at_epoch(
        &mut self,
        namespace: &str,
        os_user: &str,
        epoch: u64,
    ) -> io::Result<()> {
        self.require_healthy()?;
        identity(os_user)?;
        let user = hash(os_user.as_bytes());
        if super::namespace_user(namespace) != Some(user.as_str()) {
            return Err(invalid("quota namespace does not belong to this OS user"));
        }
        let current = self.epoch(os_user)?;
        if epoch > current.epoch {
            return Err(invalid("quota namespace epoch is invalid"));
        }
        if epoch < current.epoch {
            let mut next = self.ledger.clone();
            next.entries
                .retain(|_, entry| entry.key.namespace != namespace || entry.key.epoch > epoch);
            return self.persist(next);
        }
        if let Some(existing) = self.ledger.closed_namespaces.get(namespace) {
            return if existing.user == user && existing.epoch == epoch {
                Ok(())
            } else {
                Err(invalid("device quota namespace owner changed"))
            };
        }
        if self
            .ledger
            .entries
            .values()
            .any(|entry| entry.key.namespace == namespace && entry.key.os_user != user)
        {
            return Err(invalid("device quota namespace owner changed"));
        }
        if self.ledger.closed_namespaces.len() >= MAX_ENTRIES {
            return Err(invalid("device quota namespace index exhausted"));
        }
        let mut next = self.ledger.clone();
        next.entries
            .retain(|_, entry| entry.key.namespace != namespace || entry.key.epoch > epoch);
        next.closed_namespaces
            .insert(namespace.into(), ClosedNamespace { user, epoch });
        self.persist(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(user: &str, operation: &str) -> QuotaKey {
        key_for(user, "conversation", operation)
    }
    fn key_for(user: &str, conversation: &str, operation: &str) -> QuotaKey {
        QuotaKey::new(
            &crate::Scope {
                authority: "authority".into(),
                device: "device".into(),
                os_user: user.into(),
                owner: "owner".into(),
            },
            conversation,
            operation,
            "generation",
        )
        .unwrap()
    }
    #[test]
    fn epoch_cleanup_fences_old_requests_before_releasing_only_pruned_index_charges() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let mut locked = quota.lock().unwrap();
        let index = key("501", "index");
        let closed = key_for("501", "closed", "closed");
        let retained = key_for("501", "retained", "retained");
        let other = key("502", "other");
        for (key, bytes) in [
            (index.clone(), 4000),
            (closed.clone(), 5000),
            (retained.clone(), 7000),
            (other.clone(), 9000),
        ] {
            locked.reserve(key, bytes, 100, 1).unwrap();
        }
        locked.release_with_retained_index(&index, 1000).unwrap();
        locked.release_namespace(&closed.namespace, "501").unwrap();
        let before = locked.used_bytes();
        assert_eq!(
            locked.begin_epoch_cleanup("501", 0).unwrap(),
            EpochStatus {
                epoch: 1,
                cleanup_pending: true
            }
        );
        assert_eq!(locked.used_bytes(), before + USER_EPOCH_BYTES);
        assert!(locked.reserve(key("501", "late"), 10, 200, 101).is_err());
        let mut fresh = key_for("501", "closed", "fresh");
        fresh.epoch = 1;
        assert!(locked.reserve(fresh.clone(), 500, 200, 101).is_err());
        drop(locked);
        let mut restarted = quota.lock().unwrap();
        assert_eq!(
            restarted.begin_epoch_cleanup("501", 0).unwrap(),
            EpochStatus {
                epoch: 1,
                cleanup_pending: true
            }
        );
        assert!(restarted.finish_epoch_cleanup("502", 1).is_err());
        restarted.finish_epoch_cleanup("501", 1).unwrap();
        assert_eq!(
            restarted.used_bytes(),
            16000 + 2 * ENTRY_BYTES + USER_EPOCH_BYTES
        );
        restarted.reserve(fresh.clone(), 500, 200, 101).unwrap();
        let after = restarted.used_bytes();
        restarted
            .release(&key("501", "missing-old-request"))
            .unwrap();
        restarted
            .release_namespace_at_epoch(&fresh.namespace, "501", 0)
            .unwrap();
        assert_eq!(restarted.used_bytes(), after);
        assert!(
            restarted
                .reserve(key("501", "old-with-fresh-deadline"), 1, 5000, 1000)
                .is_err()
        );
        assert_eq!(
            restarted.begin_epoch_cleanup("501", 0).unwrap(),
            EpochStatus {
                epoch: 1,
                cleanup_pending: false
            }
        );
        restarted
            .release_with_retained_index(&retained, 500)
            .unwrap();
        let before_second = restarted.used_bytes();
        restarted.begin_epoch_cleanup("501", 1).unwrap();
        assert_eq!(restarted.used_bytes(), before_second);
        restarted.finish_epoch_cleanup("501", 2).unwrap();
        assert_eq!(
            restarted.used_bytes(),
            9500 + 2 * ENTRY_BYTES + USER_EPOCH_BYTES
        );
        assert!(restarted.finish_epoch_cleanup("501", 1).is_err());
    }
    #[test]
    fn reservations_are_durable_cross_user_and_separate_from_settled_usage() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let first = key("501", "first");
        let second = key("502", "second");
        {
            let mut locked = quota.lock().unwrap();
            locked.reserve(first.clone(), 700_000, 100, 1).unwrap();
            locked.reserve(second.clone(), 200_000, 100, 1).unwrap();
            assert_eq!(locked.reserved_bytes(), 900_000 + 2 * ENTRY_BYTES);
            locked.settle(&first, 100_000).unwrap();
            assert_eq!(locked.reserved_bytes(), 200_000 + ENTRY_BYTES);
            assert_eq!(locked.used_bytes(), 300_000 + 2 * ENTRY_BYTES);
        }
        let mut locked = quota.lock().unwrap();
        assert_eq!(locked.reserved_bytes(), 200_000 + ENTRY_BYTES);
        // A later request must not expire another worker's unsettled bytes.
        locked.reserve(key("501", "later"), 100, 300, 200).unwrap();
        assert_eq!(locked.reserved_bytes(), 200_100 + 2 * ENTRY_BYTES);
        locked.release(&second).unwrap();
        assert_eq!(locked.reserved_bytes(), 100 + ENTRY_BYTES);
        assert!(locked.used_bytes() > locked.reserved_bytes());
    }
    #[test]
    fn namespace_ownership_is_checked_even_after_its_index_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let mut locked = DeviceQuota::open(root.path()).unwrap().lock().unwrap();
        let namespace = key("501", "unseen").namespace;
        assert!(locked.release_namespace(&namespace, "502").is_err());
        assert_eq!(locked.used_bytes(), 0);
    }
    #[test]
    fn different_users_share_durable_capacity_and_expiry_does_not_release_unknown_writes() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let mut locked = quota.lock().unwrap();
        locked.set_limit(1024 * 1024).unwrap();
        let first = key("501", "first");
        let second = key("502", "second");
        locked.reserve(first.clone(), 700_000, 100, 1).unwrap();
        locked.reserve(first.clone(), 700_000, 100, 2).unwrap();
        assert_eq!(locked.used_bytes(), 700_000 + ENTRY_BYTES);
        assert_eq!(
            locked
                .reserve(second.clone(), 400_000, 100, 3)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        drop(locked);
        let mut restarted = DeviceQuota::open(root.path()).unwrap().lock().unwrap();
        assert_eq!(
            restarted
                .reserve(second.clone(), 400_000, 1000, 200)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        let wrong = key("502", "first");
        assert!(restarted.settle(&wrong, 1).is_err());
        assert!(restarted.settle(&first, 800_000).is_err());
        restarted.settle(&first, 100_000).unwrap();
        restarted
            .reserve(second.clone(), 400_000, 1000, 200)
            .unwrap();
        restarted.release(&first).unwrap();
        restarted.release(&first).unwrap();
        assert_eq!(restarted.used_bytes(), 400_000 + 2 * ENTRY_BYTES);
        assert!(restarted.reserve(first.clone(), 700_000, 100, 200).is_err());
    }
    #[test]
    fn concurrent_handles_cannot_both_spend_the_remaining_budget() {
        let root = tempfile::tempdir().unwrap();
        DeviceQuota::open(root.path())
            .unwrap()
            .lock()
            .unwrap()
            .set_limit(1024 * 1024)
            .unwrap();
        let mut threads = vec![];
        for user in ["501", "502"] {
            let path = root.path().to_owned();
            threads.push(std::thread::spawn(move || {
                DeviceQuota::open(&path)
                    .unwrap()
                    .lock()
                    .unwrap()
                    .reserve(key(user, "operation"), 700_000, 100, 1)
                    .is_ok()
            }));
        }
        assert_eq!(
            threads
                .into_iter()
                .map(|thread| usize::from(thread.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }
    #[test]
    fn corrupted_quota_is_not_recreated_and_stale_generation_cannot_release_a_new_charge() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let mut locked = quota.lock().unwrap();
        let current = key("501", "op");
        locked.reserve(current.clone(), 100, 100, 1).unwrap();
        let mut stale = current.clone();
        stale.generation = hash(b"stale");
        assert!(locked.release(&stale).is_err());
        assert_eq!(locked.used_bytes(), 100 + ENTRY_BYTES);
        drop(locked);
        quota
            .storage
            .lock()
            .unwrap()
            .write_index(b"invalid")
            .unwrap();
        assert!(quota.lock().is_err());
    }
    #[test]
    fn failed_journal_write_fences_the_handle_until_reopen() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let mut locked = quota.lock().unwrap();
        let first = key("501", "first");
        locked.reserve(first.clone(), 100, 100, 1).unwrap();
        #[cfg(not(windows))]
        let temporary = root.path().join("file-recovery-quota/index.json.tmp");
        #[cfg(not(windows))]
        std::fs::create_dir(&temporary).unwrap();
        #[cfg(windows)]
        let blocker = {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(1)
                .open(root.path().join("file-recovery-quota/index.json"))
                .unwrap()
        };
        assert!(locked.reserve(key("502", "second"), 100, 100, 2).is_err());
        #[cfg(not(windows))]
        std::fs::remove_dir(&temporary).unwrap();
        #[cfg(windows)]
        drop(blocker);
        // Even an idempotent retry must not return a permit from stale state.
        assert!(locked.reserve(first.clone(), 100, 100, 3).is_err());
        assert!(locked.release(&first).is_err());
        drop(locked);
        let mut reopened = quota.lock().unwrap();
        reopened.reserve(first, 100, 100, 4).unwrap();
        assert_eq!(reopened.used_bytes(), 100 + ENTRY_BYTES);
    }
    #[test]
    fn lowering_the_limit_preserves_existing_material_and_only_blocks_new_reservations() {
        let root = tempfile::tempdir().unwrap();
        let mut locked = DeviceQuota::open(root.path()).unwrap().lock().unwrap();
        let first = key("501", "first");
        locked.reserve(first.clone(), 2_000_000, 100, 1).unwrap();
        locked.set_limit(1024 * 1024).unwrap();
        assert!(locked.reserve(key("502", "second"), 1, 100, 2).is_err());
        locked.settle(&first, 1_500_000).unwrap();
        assert_eq!(locked.used_bytes(), 1_500_000 + ENTRY_BYTES);
        locked.release(&first).unwrap();
        locked.reserve(key("502", "second"), 100, 100, 3).unwrap();
    }
    #[test]
    fn cleanup_overtaking_a_delayed_reservation_fences_the_late_handler() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let operation = key("501", "delayed");
        quota.lock().unwrap().release(&operation).unwrap();
        let mut restarted = quota.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert_eq!(
            restarted
                .reserve(operation.clone(), 1000, now + 30_000, now)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        restarted.release(&operation).unwrap();
        assert_eq!(restarted.used_bytes(), ENTRY_BYTES);
    }
    #[test]
    fn shared_retention_and_capacity_commit_together_and_survive_reopen() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let expected = super::super::Policy {
            retention_days: 12,
            max_bytes: 1048576,
        };
        quota.lock().unwrap().set_policy(expected.clone()).unwrap();
        assert_eq!(quota.lock().unwrap().policy(), expected);
        assert!(
            quota
                .lock()
                .unwrap()
                .set_policy(super::super::Policy {
                    retention_days: 0,
                    max_bytes: 2097152
                })
                .is_err()
        );
        assert_eq!(quota.lock().unwrap().policy(), expected);
    }
    #[test]
    fn retained_indexes_stay_charged_until_exact_user_namespace_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let quota = DeviceQuota::open(root.path()).unwrap();
        let a = key("501", "first");
        let a2 = key("501", "second");
        let b = key("502", "other");
        let mut locked = quota.lock().unwrap();
        locked.reserve(a.clone(), 20000, 100, 1).unwrap();
        locked.reserve(a2.clone(), 30000, 100, 1).unwrap();
        locked.reserve(b.clone(), 3000, 100, 1).unwrap();
        locked.release_with_retained_index(&a, 2000).unwrap();
        locked.release_with_retained_index(&a2, 3000).unwrap();
        // A later reservation triggers expiry processing, but retained indexes
        // must not disappear from accounting along with execution deadlines.
        locked.reserve(key("502", "later"), 1, 1000, 200).unwrap();
        assert_eq!(locked.used_bytes(), 8001 + 4 * ENTRY_BYTES);
        assert!(locked.release_namespace(&a.namespace, "502").is_err());
        locked.release_namespace(&a.namespace, "501").unwrap();
        assert_eq!(
            locked.used_bytes(),
            3001 + 2 * ENTRY_BYTES + NAMESPACE_BYTES
        );
        let before = locked.used_bytes();
        locked.release(&a).unwrap();
        assert_eq!(locked.used_bytes(), before);
        assert_eq!(
            locked
                .reserve(key("501", "late"), 100, 1000, 200)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(locked);
        let mut restarted = quota.lock().unwrap();
        restarted.release_namespace(&a.namespace, "501").unwrap();
        assert_eq!(restarted.used_bytes(), before);
        assert!(restarted.release_namespace(&a.namespace, "502").is_err());
    }
}
