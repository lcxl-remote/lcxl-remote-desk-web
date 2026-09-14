//! Cleanup touches only registered directory objects and their known children.
use super::*;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Transaction {
    pub parent: String,
    pub parent_device: u64,
    #[cfg(target_os = "macos")]
    pub parent_volume_uuid: [u8; 16],
    pub parent_inode: u64,
    pub directory: String,
    pub directory_inode: Option<u64>,
    pub original_inode: u64,
    pub staged_inode: Option<u64>,
}
impl Transaction {
    pub(crate) fn validate(&self, id: &str) -> io::Result<()> {
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
        let r = self
            .ledger
            .records
            .get_mut(id)
            .filter(|r| {
                &r.scope == scope && r.change == ChangeState::BackupReady && r.transaction.is_none()
            })
            .ok_or_else(|| invalid("transaction cannot be planned"))?;
        let directory = format!(".assistant-transaction-{id}");
        r.transaction = Some(Transaction {
            parent: parent.into(),
            parent_device: device,
            #[cfg(target_os = "macos")]
            parent_volume_uuid,
            parent_inode: inode,
            directory: directory.clone(),
            directory_inode: None,
            original_inode,
            staged_inode: None,
        });
        self.persist()?;
        Ok(directory)
    }
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
        if tx
            .directory_inode
            .is_some_and(|inode| inode != directory_inode)
            || tx
                .staged_inode
                .is_some_and(|inode| Some(inode) != staged_inode)
        {
            return Err(invalid("transaction identity changed"));
        }
        tx.directory_inode = Some(directory_inode);
        tx.staged_inode = staged_inode;
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
        let result = clean(&tx);
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
fn clean(tx: &Transaction) -> io::Result<()> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };
    let dir_name =
        CString::new(tx.directory.as_bytes()).map_err(|_| invalid("invalid transaction name"))?;
    if !tx.directory.starts_with(".assistant-transaction-")
        || tx.directory.contains('/')
        || tx.directory.contains('\\')
    {
        return Err(invalid("invalid transaction name"));
    }
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&tx.parent)?;
    let pm = parent.metadata()?;
    #[cfg(target_os = "macos")]
    let same_volume = crate::macos::volume_uuid(&parent)? == tx.parent_volume_uuid;
    #[cfg(not(target_os = "macos"))]
    let same_volume = pm.dev() == tx.parent_device;
    if !same_volume || pm.ino() != tx.parent_inode {
        return Err(invalid("transaction parent identity changed"));
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            dir_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        };
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let dm = directory.metadata()?;
    if tx.directory_inode != Some(dm.ino()) || dm.dev() != pm.dev() {
        return Err(invalid(
            "transaction directory identity is unavailable or changed",
        ));
    }
    for name in [c"replacement", c"original"] {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::NotFound {
                continue;
            }
            return Err(e);
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let m = file.metadata()?;
        if !m.is_file()
            || m.dev() != pm.dev()
            || !(m.ino() == tx.original_inode || Some(m.ino()) == tx.staged_inode)
        {
            return Err(invalid("transaction child identity changed"));
        }
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), dir_name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(io::Error::last_os_error());
    }
    parent.sync_all()
}
#[cfg(not(unix))]
fn clean(_: &Transaction) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native transaction cleanup is unavailable",
    ))
}
