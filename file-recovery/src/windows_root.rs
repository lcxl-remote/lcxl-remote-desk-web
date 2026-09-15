//! Resolve the host-selected data root one component at a time without reparses.
use super::*;
use std::{
    fs::OpenOptions,
    os::windows::fs::OpenOptionsExt,
    path::{Component, Path, Prefix},
};

pub(super) fn resolve_directory(data_root: &Path) -> io::Result<Vec<File>> {
    let mut components = data_root.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => {
                return Err(crate::invalid(
                    "recovery storage requires a local drive path",
                ));
            }
        },
        _ => return Err(crate::invalid("recovery storage requires an absolute path")),
    };
    if !matches!(components.next(), Some(Component::RootDir)) {
        return Err(crate::invalid("recovery storage requires an absolute path"));
    }
    let first = OpenOptions::new()
        .access_mode((FILE_READ_ATTRIBUTES | FILE_TRAVERSE).0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(format!("{}:\\", char::from(drive)))?;
    let volume = file_identity(&first, FileKind::Directory)?.volume_serial;
    let user = security::current_user()?;
    let mut anchors = vec![first];
    for component in components {
        if anchors.len() >= 256 {
            return Err(crate::invalid("recovery directory depth exceeds its bound"));
        }
        let Component::Normal(name) = component else {
            return Err(crate::invalid("invalid recovery directory component"));
        };
        let name = name
            .to_str()
            .ok_or_else(|| crate::invalid("invalid recovery directory encoding"))?;
        let handle = open_relative(anchors.last().unwrap(), name, &user, OpenKind::Anchor)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("recovery ancestor {}: {error}", anchors.len()),
                )
            })?;
        if file_identity(&handle, FileKind::Directory)?.volume_serial != volume {
            return Err(crate::invalid("recovery directory crossed volumes"));
        }
        anchors.push(handle);
    }
    Ok(anchors)
}

impl PrivateDirectory {
    pub fn open_data_root(data_root: &Path, leaf: &str) -> io::Result<Self> {
        let anchors = resolve_directory(data_root)?;
        let mut result = Self::open_or_create(anchors.last().unwrap(), leaf)?;
        result.ancestors = anchors;
        Ok(result)
    }
}
