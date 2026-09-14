use super::*;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochCleanupState {
    pub previous: u64,
    pub next: u64,
    pub pruned: bool,
}

impl LockedVault {
    pub fn execution_epoch(&self) -> u64 {
        self.ledger.execution_epoch
    }
    pub fn epoch_cleanup_state(&self) -> Option<EpochCleanupState> {
        self.ledger.epoch_cleanup
    }
    pub fn require_execution_epoch(&self, expected: u64) -> io::Result<()> {
        if self.ledger.epoch_cleanup.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "backup replay index cleanup is pending",
            ));
        }
        if expected != self.ledger.execution_epoch {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "backup execution epoch changed",
            ));
        }
        Ok(())
    }
    fn validate_cleanup_user(&self, os_user: &str) -> io::Result<()> {
        identity(os_user)?;
        let user = hash(os_user.as_bytes());
        if self
            .ledger
            .records
            .values()
            .any(|record| record.scope.os_user != os_user)
            || self
                .ledger
                .deleted_conversations
                .keys()
                .any(|namespace| namespace_user(namespace) != Some(user.as_str()))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "recovery cleanup user changed",
            ));
        }
        Ok(())
    }
    pub fn prunable_index_count(&self) -> usize {
        self.ledger
            .records
            .values()
            .filter(|record| {
                record.material == MaterialState::Purged
                    && !record.device_quota_pending
                    && record.transaction.is_none()
            })
            .count()
            + self
                .ledger
                .deleted_conversations
                .iter()
                .filter(|(namespace, deleted)| {
                    !deleted.quota_pending
                        && !self.ledger.records.values().any(|record| {
                            record.scope.key(&record.conversation) == **namespace
                                && (record.material != MaterialState::Purged
                                    || record.device_quota_pending
                                    || record.transaction.is_some())
                        })
                })
                .count()
    }
    fn quota_cleanup_settled(&self) -> bool {
        !self
            .ledger
            .records
            .values()
            .any(|record| record.material == MaterialState::Purged && record.device_quota_pending)
            && !self
                .ledger
                .deleted_conversations
                .values()
                .any(|deleted| deleted.quota_pending)
    }
    /// Persist intent while holding the OS vault lock, before asking the daemon
    /// to close this epoch. Do not remove indexes or release their quota here.
    pub fn prepare_epoch_cleanup(
        &mut self,
        os_user: &str,
    ) -> io::Result<Option<EpochCleanupState>> {
        self.validate_cleanup_user(os_user)?;
        if let Some(state) = self.ledger.epoch_cleanup {
            return Ok(Some(state));
        }
        if !self.quota_cleanup_settled() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "backup quota acknowledgments are pending",
            ));
        }
        if self.prunable_index_count() == 0 {
            return Ok(None);
        }
        let state = EpochCleanupState {
            previous: self.ledger.execution_epoch,
            next: self
                .ledger
                .execution_epoch
                .checked_add(1)
                .ok_or_else(|| invalid("backup epoch exhausted"))?,
            pruned: false,
        };
        self.ledger.epoch_cleanup = Some(state);
        self.persist()?;
        Ok(Some(state))
    }
    /// The daemon has confirmed that old-epoch reserves are fenced. Keep the
    /// pending marker until the daemon also acknowledges freed index charges.
    pub fn prune_epoch_indexes(&mut self, os_user: &str, advanced_epoch: u64) -> io::Result<()> {
        self.validate_cleanup_user(os_user)?;
        let mut state = self
            .ledger
            .epoch_cleanup
            .ok_or_else(|| invalid("backup epoch cleanup not prepared"))?;
        if state.next != advanced_epoch {
            return Err(invalid("backup epoch advancement does not match"));
        }
        if state.pruned {
            return Ok(());
        }
        if !self.quota_cleanup_settled() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "backup quota acknowledgments are pending",
            ));
        }
        self.ledger.records.retain(|_, record| {
            record.storage_epoch >= advanced_epoch
                || record.material != MaterialState::Purged
                || record.device_quota_pending
                || record.transaction.is_some()
        });
        let retained_namespaces: BTreeSet<_> = self
            .ledger
            .records
            .values()
            .map(|record| record.scope.key(&record.conversation))
            .collect();
        self.ledger
            .deleted_conversations
            .retain(|namespace, deleted| {
                deleted.storage_epoch >= advanced_epoch
                    || deleted.quota_pending
                    || retained_namespaces.contains(namespace)
            });
        self.ledger.execution_epoch = advanced_epoch;
        state.pruned = true;
        self.ledger.epoch_cleanup = Some(state);
        self.persist()
    }
    pub fn finish_epoch_cleanup(&mut self, acknowledged_epoch: u64) -> io::Result<()> {
        let Some(state) = self.ledger.epoch_cleanup else {
            return if self.ledger.execution_epoch == acknowledged_epoch {
                Ok(())
            } else {
                Err(invalid("backup cleanup epoch changed"))
            };
        };
        if !state.pruned || state.next != acknowledged_epoch {
            return Err(invalid("backup index cleanup is not complete"));
        }
        self.ledger.epoch_cleanup = None;
        self.persist()
    }
    pub fn namespace_epoch(&self, namespace: &str) -> io::Result<u64> {
        self.ledger
            .deleted_conversations
            .get(namespace)
            .map(|deleted| deleted.storage_epoch)
            .ok_or_else(|| invalid("backup namespace cleanup is unavailable"))
    }
}

/// The coordinator must authenticate the real OS user independently of this vault.
pub trait EpochCoordinator {
    fn begin(&mut self, os_user: &str, expected: u64) -> io::Result<quota::EpochStatus>;
    fn finish(&mut self, os_user: &str, epoch: u64) -> io::Result<quota::EpochStatus>;
}
impl EpochCoordinator for quota::LockedDeviceQuota {
    fn begin(&mut self, os_user: &str, expected: u64) -> io::Result<quota::EpochStatus> {
        self.begin_epoch_cleanup(os_user, expected)
    }
    fn finish(&mut self, os_user: &str, epoch: u64) -> io::Result<quota::EpochStatus> {
        self.finish_epoch_cleanup(os_user, epoch)
    }
}
impl LockedVault {
    /// Resume durable work regardless of threshold. usize::MAX means resume only.
    /// Hold the vault lock through both acknowledgments; never release charges
    /// before the local index removal has been persisted.
    pub fn maintain_epoch_indexes(
        &mut self,
        os_user: &str,
        coordinator: &mut impl EpochCoordinator,
        minimum_indexes: usize,
    ) -> io::Result<()> {
        if self.epoch_cleanup_state().is_none()
            && (minimum_indexes == usize::MAX
                || self.prunable_index_count() < minimum_indexes
                || !self.quota_cleanup_settled())
        {
            return Ok(());
        }
        let Some(state) = self.prepare_epoch_cleanup(os_user)? else {
            return Ok(());
        };
        let advanced = coordinator.begin(os_user, state.previous)?;
        if advanced.epoch != state.next || (!state.pruned && !advanced.cleanup_pending) {
            return Err(invalid(
                "backup coordinator epoch does not match persisted cleanup",
            ));
        }
        self.prune_epoch_indexes(os_user, advanced.epoch)?;
        let finished = coordinator.finish(os_user, state.next)?;
        if finished.epoch != state.next || finished.cleanup_pending {
            return Err(invalid("backup coordinator cleanup is not acknowledged"));
        }
        self.finish_epoch_cleanup(finished.epoch)
    }
}
