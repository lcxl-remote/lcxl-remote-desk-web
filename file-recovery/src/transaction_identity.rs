//! Persistent file identities retain their originating platform on every host.
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InodeIdentity {
    pub parent_device: u64,
    pub parent_inode: u64,
    pub directory_inode: Option<u64>,
    pub original_inode: u64,
    pub staged_inode: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsIdentity {
    pub volume_serial: u64,
    pub parent_file_id: [u8; 16],
    pub directory_file_id: Option<[u8; 16]>,
    pub original_file_id: [u8; 16],
    pub staged_file_id: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransactionIdentity {
    Macos {
        volume_uuid: [u8; 16],
        files: InodeIdentity,
    },
    Unix {
        files: InodeIdentity,
    },
    Windows {
        files: WindowsIdentity,
    },
}

impl TransactionIdentity {
    pub(super) fn validate_host(&self) -> io::Result<()> {
        let supported = match self {
            Self::Macos { volume_uuid, .. } => cfg!(target_os = "macos") && *volume_uuid != [0; 16],
            Self::Unix { .. } => cfg!(all(unix, not(target_os = "macos"))),
            Self::Windows { .. } => cfg!(windows),
        };
        if !supported {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "transaction identity belongs to a different or unsupported platform",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(super) fn inodes(&self) -> io::Result<&InodeIdentity> {
        self.validate_host()?;
        match self {
            Self::Macos { files, .. } | Self::Unix { files } => Ok(files),
            Self::Windows { .. } => Err(super::invalid("inode identity required")),
        }
    }

    #[cfg(unix)]
    pub(super) fn inodes_mut(&mut self) -> io::Result<&mut InodeIdentity> {
        self.validate_host()?;
        match self {
            Self::Macos { files, .. } | Self::Unix { files } => Ok(files),
            Self::Windows { .. } => Err(super::invalid("inode identity required")),
        }
    }
}
