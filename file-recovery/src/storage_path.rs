//! Existing path-based storage for platforms without the Windows handle backend.
use crate::{
    invalid,
    storage_legacy::{atomic_write, bounded_read, open_private, private_dir},
};
use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

pub(super) struct Root(PathBuf);
pub(super) struct Locked {
    root: PathBuf,
    _lock: File,
}
impl Root {
    pub fn open(data_root: &Path) -> io::Result<Self> {
        Self::open_named(data_root, "file-recovery")
    }
    pub fn open_named(data_root: &Path, leaf: &str) -> io::Result<Self> {
        let root = data_root.join(leaf);
        private_dir(&root)?;
        Ok(Self(root))
    }
    pub fn lock(&self) -> io::Result<Locked> {
        private_dir(&self.0)?;
        let lock = open_private(&self.0.join("lock"), true)?;
        lock.lock()?;
        Ok(Locked {
            root: self.0.clone(),
            _lock: lock,
        })
    }
    pub fn try_lock(&self) -> io::Result<Option<Locked>> {
        private_dir(&self.0)?;
        let lock = open_private(&self.0.join("lock"), true)?;
        match lock.try_lock() {
            Ok(()) => Ok(Some(Locked {
                root: self.0.clone(),
                _lock: lock,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}
impl Locked {
    pub fn read(&self, leaf: &str, limit: u64) -> io::Result<Vec<u8>> {
        bounded_read(&self.root.join(leaf), limit)
    }
    pub fn write_index(&self, bytes: &[u8]) -> io::Result<()> {
        atomic_write(&self.root.join("index.json"), bytes)
    }
    pub fn write_material(&self, leaf: &str, bytes: &[u8]) -> io::Result<()> {
        atomic_write(&self.root.join(leaf), bytes)
    }
    pub fn extra_used_bytes(&self) -> io::Result<u64> {
        Ok(0)
    }
    pub fn validate_missing_index(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn remove_record_material(&self, id: &str) -> io::Result<()> {
        if !crate::valid_id(id) {
            return Err(invalid("invalid stored recovery id"));
        }
        for suffix in ["body", "metadata", "body.tmp", "metadata.tmp"] {
            let path = self.root.join(format!("{id}.{suffix}"));
            match open_private(&path, false) {
                Ok(_file) => fs::remove_file(&path)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(error),
            }
        }
        File::open(&self.root)?.sync_all()
    }
}
