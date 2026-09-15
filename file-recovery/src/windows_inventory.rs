//! Bounded observation of retained index material. Names alone never authorize cleanup.
use super::super::FileIdentity;
use super::*;
use ::windows::{Win32::Foundation::ERROR_NO_MORE_FILES, core::HRESULT};
use std::{collections::BTreeSet, mem::offset_of};

pub(super) const MAX_PENDING: usize = 256;
const MAX_ENTRIES: usize = crate::MAX_RECORDS * 4 + 1024;

#[derive(Debug, PartialEq, Eq)]
pub struct PendingIndexFile {
    pub name: String,
    pub identity: FileIdentity,
    pub bytes: u64,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PendingIndexInventory {
    pub files: Vec<PendingIndexFile>,
    pub bytes: u64,
}

impl PrivateDirectoryLock {
    pub fn pending_index_inventory(&self) -> io::Result<PendingIndexInventory> {
        self.validate()?;
        let mut result = PendingIndexInventory::default();
        let mut names = BTreeSet::new();
        let mut held = Vec::new();
        let mut entries = 0usize;
        for batch in 0..1024 {
            let mut buffer = vec![0u64; 8192];
            let query = unsafe {
                GetFileInformationByHandleEx(
                    HANDLE(self._directory.as_raw_handle()),
                    if batch == 0 {
                        FileIdBothDirectoryRestartInfo
                    } else {
                        FileIdBothDirectoryInfo
                    },
                    buffer.as_mut_ptr().cast(),
                    65_536,
                )
            };
            if let Err(error) = query {
                if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                    self.validate()?;
                    result.files.sort_by(|a, b| a.name.cmp(&b.name));
                    return Ok(result);
                }
                return Err(io::Error::other(error));
            }
            let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), 65_536) };
            for name in parse_names(bytes)? {
                entries += 1;
                if entries > MAX_ENTRIES {
                    return Err(crate::invalid(
                        "private directory inventory exceeds its bound",
                    ));
                }
                if !name.starts_with("index-pending-") {
                    continue;
                }
                if result.files.len() == MAX_PENDING || !names.insert(name.clone()) {
                    return Err(crate::invalid(
                        "pending index inventory exceeds its bound or changed",
                    ));
                }
                let file = open_relative(&self._directory, &name, &self.user, OpenKind::ReadFile)?;
                let identity = file_identity(&file, FileKind::File)?;
                security::validate_private_file(&file, &self.user)?;
                let size = file.metadata()?.len();
                if size > crate::MAX_LEDGER_BYTES {
                    return Err(crate::invalid("pending index material exceeds its bound"));
                }
                result.bytes = result
                    .bytes
                    .checked_add(size)
                    .ok_or_else(|| crate::invalid("pending index size overflow"))?;
                result.files.push(PendingIndexFile {
                    name,
                    identity,
                    bytes: size,
                });
                held.push(file);
            }
        }
        Err(crate::invalid(
            "private directory inventory did not complete within its bound",
        ))
    }
}

fn parse_names(bytes: &[u8]) -> io::Result<Vec<String>> {
    let header = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let length_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
    let invalid = || crate::invalid("invalid directory information buffer");
    let mut offset = 0usize;
    let mut names = Vec::new();
    loop {
        let entry = bytes.get(offset..).ok_or_else(invalid)?;
        if entry.len() < header {
            return Err(invalid());
        }
        let next = u32::from_le_bytes(entry[..4].try_into().unwrap()) as usize;
        let length = u32::from_le_bytes(entry[length_offset..length_offset + 4].try_into().unwrap())
            as usize;
        if length == 0 || length > 512 || !length.is_multiple_of(2) {
            return Err(invalid());
        }
        let end = header + length;
        let raw = entry.get(header..end).ok_or_else(invalid)?;
        let units: Vec<u16> = raw
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let name = String::from_utf16(&units).map_err(|_| invalid())?;
        if name.contains('\0') {
            return Err(invalid());
        }
        names.push(name);
        if next == 0 {
            return Ok(names);
        }
        if next < end || !next.is_multiple_of(8) || next >= entry.len() {
            return Err(invalid());
        }
        offset += next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn truncated_or_overlapping_directory_entries_are_rejected() {
        assert!(parse_names(&[]).is_err());
        let header = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let length_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileNameLength);
        let mut data = vec![0u8; header + 8];
        data[length_offset..length_offset + 4].copy_from_slice(&2u32.to_le_bytes());
        data[header..header + 2].copy_from_slice(&(b'x' as u16).to_le_bytes());
        assert_eq!(parse_names(&data).unwrap(), ["x"]);
        data[..4].copy_from_slice(&8u32.to_le_bytes());
        assert!(parse_names(&data).is_err());
        data[..4].copy_from_slice(&0u32.to_le_bytes());
        data[length_offset..length_offset + 4].copy_from_slice(&514u32.to_le_bytes());
        assert!(parse_names(&data).is_err());
    }
}
