//! Private directories are created relative to held, caller-validated handles.
use super::{FileKind, file_identity, security};
use ::windows::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
            FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    },
    Win32::{
        Foundation::{HANDLE, OBJ_CASE_INSENSITIVE, UNICODE_STRING},
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
    core::PWSTR,
};
use std::{
    cell::Cell,
    fs::File,
    io::{self, Read, Write},
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle},
};

#[path = "windows_publish.rs"]
mod publish;
pub use publish::{IndexWriteError, IndexWriteOutcome};
#[path = "windows_inventory.rs"]
mod inventory;
pub use inventory::{PendingIndexFile, PendingIndexInventory};
#[path = "windows_cleanup.rs"]
mod cleanup;
#[path = "windows_root.rs"]
mod root;
#[path = "windows_transaction_location.rs"]
mod transaction_location;
pub(crate) use transaction_location::transaction_location;
#[path = "windows_transaction_cleanup.rs"]
mod transaction_cleanup;
pub(crate) use transaction_cleanup::clean_transaction;

pub struct PrivateDirectory {
    handle: File,
    user: String,
    ancestors: Vec<File>,
}

pub struct PrivateDirectoryLock {
    _lock: File,
    _directory: File,
    user: String,
    write_uncertain: Cell<bool>,
    _ancestors: Vec<File>,
}

enum OpenKind {
    Anchor,
    InspectDirectory,
    NewDirectory,
    Directory,
    Lock,
    NewFile,
    ReadFile,
    Temporary,
    DeleteFile,
    TransactionFile,
    InspectTransactionFile,
    DeleteDirectory,
}

impl PrivateDirectory {
    /// The parent must already be resolved for the verified worker OS user.
    /// Existing directories are validated without rewriting their security.
    pub fn open_or_create(parent: &File, leaf: &str) -> io::Result<Self> {
        Self::open_directory(parent, leaf, OpenKind::Directory)
    }

    /// Create a new private directory without adopting an existing object.
    /// A failure after creation can leave material for explicit reconciliation.
    pub fn create_new(parent: &File, leaf: &str) -> io::Result<Self> {
        Self::open_directory(parent, leaf, OpenKind::NewDirectory)
    }

    fn open_directory(parent: &File, leaf: &str, kind: OpenKind) -> io::Result<Self> {
        file_identity(parent, FileKind::Directory)?;
        let user = security::current_user()?;
        let handle = open_relative(parent, leaf, &user, kind)?;
        let directory = Self {
            handle,
            user,
            ancestors: vec![parent.try_clone()?],
        };
        directory.validate()?;
        Ok(directory)
    }

    pub fn validate(&self) -> io::Result<()> {
        file_identity(&self.handle, FileKind::Directory)?;
        security::validate_private(&self.handle, &self.user)
    }

    pub fn open_child(&self, leaf: &str) -> io::Result<Self> {
        self.validate()?;
        let mut child = Self::open_or_create(&self.handle, leaf)?;
        child.ancestors.extend(
            self.ancestors
                .iter()
                .map(File::try_clone)
                .collect::<io::Result<Vec<_>>>()?,
        );
        self.validate()?;
        Ok(child)
    }

    /// Retain this directory while operating on its children; its handle does
    /// not share DELETE, preventing renaming of the directory itself.
    pub fn handle(&self) -> &File {
        &self.handle
    }

    /// A newly created, fixed transaction leaf. The returned handle retains
    /// write/delete rights for the executor without permitting competing writers.
    pub(crate) fn create_replacement(&self) -> io::Result<File> {
        self.validate()?;
        let file = open_relative(
            &self.handle,
            "replacement",
            &self.user,
            OpenKind::TransactionFile,
        )?;
        file_identity(&file, FileKind::File)?;
        security::validate_private_file(&file, &self.user)?;
        self.validate()?;
        Ok(file)
    }

    /// Maintenance can skip a busy vault without waiting behind a mutation.
    /// The returned guard pins both the lock file and the directory until drop.
    pub fn try_lock(&self) -> io::Result<Option<PrivateDirectoryLock>> {
        self.acquire_lock(false)
    }

    pub fn lock(&self) -> io::Result<PrivateDirectoryLock> {
        self.acquire_lock(true)?
            .ok_or_else(|| io::Error::other("recovery lock was not acquired"))
    }

    fn acquire_lock(&self, wait: bool) -> io::Result<Option<PrivateDirectoryLock>> {
        self.validate()?;
        let lock = open_relative(&self.handle, "lock", &self.user, OpenKind::Lock)?;
        file_identity(&lock, FileKind::File)?;
        security::validate_private_file(&lock, &self.user)?;
        let acquired = if wait {
            lock.lock().map_err(std::fs::TryLockError::Error)
        } else {
            lock.try_lock()
        };
        match acquired {
            Ok(()) => {
                self.validate()?;
                security::validate_private_file(&lock, &self.user)?;
                Ok(Some(PrivateDirectoryLock {
                    _lock: lock,
                    _directory: self.handle.try_clone()?,
                    user: self.user.clone(),
                    write_uncertain: Cell::new(false),
                    _ancestors: self
                        .ancestors
                        .iter()
                        .map(File::try_clone)
                        .collect::<io::Result<Vec<_>>>()?,
                }))
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error),
        }
    }
}

impl PrivateDirectoryLock {
    pub(crate) fn ensure_writable(&self) -> io::Result<()> {
        if self.write_uncertain.get() {
            return Err(io::Error::other("private index publication is unresolved"));
        }
        self.validate()
    }

    fn validate(&self) -> io::Result<()> {
        file_identity(&self._directory, FileKind::Directory)?;
        security::validate_private(&self._directory, &self.user)?;
        file_identity(&self._lock, FileKind::File)?;
        security::validate_private_file(&self._lock, &self.user)
    }

    pub fn read(&self, leaf: &str, limit: u64) -> io::Result<Vec<u8>> {
        if limit > crate::MAX_LEDGER_BYTES || leaf.eq_ignore_ascii_case("lock") {
            return Err(crate::invalid("invalid private storage read bound or name"));
        }
        self.validate()?;
        let file = open_relative(&self._directory, leaf, &self.user, OpenKind::ReadFile)?;
        file_identity(&file, FileKind::File)?;
        security::validate_private_file(&file, &self.user)?;
        if file.metadata()?.len() > limit {
            return Err(crate::invalid(
                "private storage item exceeds its read bound",
            ));
        }
        let mut bytes = Vec::new();
        (&file).take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(crate::invalid(
                "private storage item exceeds its read bound",
            ));
        }
        self.validate()?;
        file_identity(&file, FileKind::File)?;
        security::validate_private_file(&file, &self.user)?;
        Ok(bytes)
    }

    /// Creates new material only. A failure can leave an incomplete private
    /// item; callers must not publish it or treat a retry as an overwrite.
    pub fn create_new(&self, leaf: &str, bytes: &[u8]) -> io::Result<()> {
        if self.write_uncertain.get() {
            return Err(io::Error::other("private index publication is unresolved"));
        }
        if bytes.len() as u64 > crate::MAX_LEDGER_BYTES || leaf.eq_ignore_ascii_case("lock") {
            return Err(crate::invalid(
                "invalid private storage write bound or name",
            ));
        }
        self.validate()?;
        let mut file = open_relative(&self._directory, leaf, &self.user, OpenKind::NewFile)?;
        file_identity(&file, FileKind::File)?;
        security::validate_private_file(&file, &self.user)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        self.validate()?;
        file_identity(&file, FileKind::File)?;
        security::validate_private_file(&file, &self.user)?;
        self._directory.sync_all()
    }
}

pub(super) fn leaf_name(value: &str) -> io::Result<Vec<u16>> {
    if value.is_empty()
        || value.len() > 200
        || value.ends_with([' ', '.'])
        || value
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
    {
        return Err(crate::invalid(
            "expected one bounded recovery directory name",
        ));
    }
    let stem = value.split('.').next().unwrap().to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        return Err(crate::invalid("reserved recovery directory name"));
    }
    Ok(value.encode_utf16().collect())
}

fn open_relative(parent: &File, leaf: &str, user: &str, kind: OpenKind) -> io::Result<File> {
    let directory = matches!(
        kind,
        OpenKind::Directory
            | OpenKind::Anchor
            | OpenKind::InspectDirectory
            | OpenKind::NewDirectory
            | OpenKind::DeleteDirectory
    );
    let (access, share, disposition) = match kind {
        OpenKind::InspectDirectory => (
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE | READ_CONTROL,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
        ),
        OpenKind::NewDirectory => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_CREATE,
        ),
        OpenKind::Anchor => (
            FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
        ),
        OpenKind::Directory | OpenKind::Lock => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN_IF,
        ),
        OpenKind::NewFile => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            FILE_CREATE,
        ),
        OpenKind::ReadFile => (FILE_GENERIC_READ, FILE_SHARE_READ, FILE_OPEN),
        OpenKind::TransactionFile => (FILE_ALL_ACCESS, FILE_SHARE_READ, FILE_CREATE),
        OpenKind::InspectTransactionFile => (
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
        ),
        OpenKind::DeleteFile => (FILE_GENERIC_READ | DELETE, FILE_SHARE_MODE(0), FILE_OPEN),
        OpenKind::DeleteDirectory => (
            FILE_GENERIC_READ | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
        ),
        OpenKind::Temporary => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
            FILE_SHARE_READ,
            FILE_CREATE,
        ),
    };
    let flags = if directory { "OICI" } else { "" };
    let mut name = leaf_name(leaf)?;
    let descriptor = security::Descriptor::from_sddl(&format!(
        "O:{user}D:P(A;{flags};FA;;;{user})(A;{flags};FA;;;SY)"
    ))?;
    let text = UNICODE_STRING {
        Length: (name.len() * 2) as u16,
        MaximumLength: (name.len() * 2) as u16,
        Buffer: PWSTR(name.as_mut_ptr()),
    };
    let attrs = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: HANDLE(parent.as_raw_handle()),
        ObjectName: &text,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: descriptor.pointer.0.cast_const().cast(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = HANDLE::default();
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attrs,
            &mut status,
            None,
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            (if directory {
                FILE_DIRECTORY_FILE
            } else {
                FILE_NON_DIRECTORY_FILE
            }) | FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    };
    if result.0 < 0 {
        let kind = match result {
            ::windows::Win32::Foundation::STATUS_OBJECT_NAME_NOT_FOUND => io::ErrorKind::NotFound,
            ::windows::Win32::Foundation::STATUS_OBJECT_NAME_COLLISION => {
                io::ErrorKind::AlreadyExists
            }
            _ => io::ErrorKind::Other,
        };
        return Err(io::Error::new(
            kind,
            format!("private storage open NTSTATUS {:#x}", result.0),
        ));
    }
    Ok(unsafe { File::from_raw_handle(handle.0) })
}
