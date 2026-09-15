//! Quota access for local backup management in-process or in a user worker.
use crate::worker::session::QuotaClient;
use desk_file_recovery::{
    EpochCoordinator, LockedVault, Policy, Record,
    quota::{LockedDeviceQuota, QuotaKey},
};
use std::io;

pub(crate) trait LocalQuota: EpochCoordinator {
    fn usage(&mut self) -> io::Result<(Policy, u64, u64)>;
    fn settle_record(&mut self, record: &Record) -> io::Result<()>;
    fn release_record(&mut self, record: &Record) -> io::Result<()>;
    fn release_user_namespace(
        &mut self,
        namespace: &str,
        os_user: &str,
        epoch: u64,
    ) -> io::Result<()>;
}

fn key(record: &Record) -> io::Result<QuotaKey> {
    let mut key = QuotaKey::new(
        &record.scope,
        &record.conversation,
        &record.operation,
        &record.generation,
    )?;
    key.epoch = record.storage_epoch;
    Ok(key)
}

impl LocalQuota for LockedDeviceQuota {
    fn usage(&mut self) -> io::Result<(Policy, u64, u64)> {
        Ok((self.policy(), self.used_bytes(), self.reserved_bytes()))
    }

    fn settle_record(&mut self, record: &Record) -> io::Result<()> {
        self.settle(&key(record)?, record.bytes)
    }

    fn release_record(&mut self, record: &Record) -> io::Result<()> {
        self.release_with_retained_index(&key(record)?, LockedVault::retained_index_bytes(record)?)
    }

    fn release_user_namespace(
        &mut self,
        namespace: &str,
        os_user: &str,
        epoch: u64,
    ) -> io::Result<()> {
        self.release_namespace_at_epoch(namespace, os_user, epoch)
    }
}

fn require_user(expected: &str) -> io::Result<()> {
    if super::platform_user::current()? != expected {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "backup quota user changed",
        ));
    }
    Ok(())
}

impl LocalQuota for QuotaClient {
    fn usage(&mut self) -> io::Result<(Policy, u64, u64)> {
        self.policy()
    }

    fn settle_record(&mut self, record: &Record) -> io::Result<()> {
        require_user(&record.scope.os_user)?;
        self.settle(record)
    }

    fn release_record(&mut self, record: &Record) -> io::Result<()> {
        require_user(&record.scope.os_user)?;
        self.release(record)
    }

    fn release_user_namespace(
        &mut self,
        namespace: &str,
        os_user: &str,
        epoch: u64,
    ) -> io::Result<()> {
        require_user(os_user)?;
        self.release_namespace(namespace, epoch)
    }
}
