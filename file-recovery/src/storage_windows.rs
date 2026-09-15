//! Windows storage operations for the shared recovery ledger state machine.
use crate::{
    invalid,
    windows::{PrivateDirectory, PrivateDirectoryLock},
};
use std::{io, path::Path};

pub(super) struct Root(PrivateDirectory);
pub(super) struct Locked(PrivateDirectoryLock);

impl Root {
    pub fn open(data_root: &Path) -> io::Result<Self> {
        Self::open_named(data_root, "file-recovery")
    }
    pub fn open_named(data_root: &Path, leaf: &str) -> io::Result<Self> {
        Ok(Self(PrivateDirectory::open_data_root(data_root, leaf)?))
    }
    pub fn lock(&self) -> io::Result<Locked> {
        self.0.lock().map(Locked)
    }
    pub fn try_lock(&self) -> io::Result<Option<Locked>> {
        Ok(self.0.try_lock()?.map(Locked))
    }
}
impl Locked {
    pub fn ensure_writable(&self) -> io::Result<()> {
        self.0.ensure_writable()
    }
    pub fn read(&self, leaf: &str, limit: u64) -> io::Result<Vec<u8>> {
        self.0.read(leaf, limit)
    }
    pub fn write_index(&self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_index(bytes).map_err(io::Error::other)
    }
    pub fn write_material(&self, leaf: &str, bytes: &[u8]) -> io::Result<()> {
        self.0.create_new(leaf, bytes)
    }
    pub fn remove_record_material(&self, id: &str) -> io::Result<()> {
        self.0.remove_record_material(id)
    }
    pub fn extra_used_bytes(&self) -> io::Result<u64> {
        Ok(self.0.pending_index_inventory()?.bytes)
    }
    pub fn validate_missing_index(&self) -> io::Result<()> {
        if !self.0.pending_index_inventory()?.files.is_empty() {
            return Err(invalid(
                "recovery index is missing with unresolved publication material",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Policy, Vault};
    use std::{fs, os::windows::fs::OpenOptionsExt};

    #[test]
    fn vault_guard_pins_ancestor_directories_after_vault_drop() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("data parent");
        let data = parent.join("用户数据");
        fs::create_dir_all(&data).unwrap();
        let vault = Vault::open(&data).unwrap();
        let mut locked = vault.lock().unwrap();
        locked.set_policy(Policy::default()).unwrap();
        drop(vault);
        let moved = root.path().join("moved");
        assert!(fs::rename(&parent, &moved).is_err());
        drop(locked);
        fs::rename(&parent, &moved).unwrap();
        let reopened = Vault::open(&moved.join("用户数据")).unwrap();
        assert!(reopened.lock().is_ok());
        for invalid in [
            "relative",
            "C:relative",
            "\\\\server\\share",
            "C:\\data\\..\\other",
        ] {
            assert!(Vault::open(Path::new(invalid)).is_err());
        }
    }

    #[test]
    fn retained_index_bytes_are_charged_after_reopen_and_busy_inventory_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let vault = Vault::open(root.path()).unwrap();
        let mut locked = vault.lock().unwrap();
        locked.set_policy(Policy::default()).unwrap();
        let baseline = locked.used_bytes();
        locked
            .storage
            .write_material("index-pending-retained", b"retained")
            .unwrap();
        assert_eq!(locked.used_bytes(), baseline + 8);
        drop(locked);
        let locked = vault.lock().unwrap();
        assert_eq!(locked.used_bytes(), baseline + 8);
        let writer = fs::OpenOptions::new()
            .write(true)
            .share_mode(1)
            .open(root.path().join("file-recovery/index-pending-retained"))
            .unwrap();
        assert_eq!(locked.used_bytes(), u64::MAX);
        drop(writer);
        assert_eq!(locked.used_bytes(), baseline + 8);
    }

    #[test]
    fn missing_index_with_pending_publication_is_not_recreated() {
        let root = tempfile::tempdir().unwrap();
        let directory = PrivateDirectory::open_data_root(root.path(), "file-recovery").unwrap();
        directory
            .lock()
            .unwrap()
            .create_new("index-pending-empty", b"")
            .unwrap();
        drop(directory);
        let vault = Vault::open(root.path()).unwrap();
        assert!(vault.lock().is_err());
        assert!(!root.path().join("file-recovery/index.json").exists());
        assert!(
            root.path()
                .join("file-recovery/index-pending-empty")
                .exists()
        );
    }
}
