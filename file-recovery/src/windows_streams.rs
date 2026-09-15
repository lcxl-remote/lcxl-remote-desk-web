//! Bounded stream observation. This is not a whole-file snapshot or write fence.
use super::{FileKind, file_identity};
use ::windows::{
    Wdk::Storage::FileSystem::{FileStreamInformation, NtQueryInformationFile},
    Win32::{Foundation::HANDLE, System::IO::IO_STATUS_BLOCK},
};
use std::{fs::File, io, os::windows::io::AsRawHandle};

const BUFFER_BYTES: usize = 65_536;
const MAX_STREAMS: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub name: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInventory {
    pub streams: Vec<StreamInfo>,
    pub logical_bytes: u64,
}

/// Observe names and lengths through an already authorized file handle. Content,
/// stream-set stability and metadata consistency require separate verification.
pub fn stream_inventory(file: &File, limit: u64) -> io::Result<StreamInventory> {
    let identity = file_identity(file, FileKind::File)?;
    let mut buffer = vec![0u64; BUFFER_BYTES / 8];
    let mut status = IO_STATUS_BLOCK::default();
    let result = unsafe {
        NtQueryInformationFile(
            HANDLE(file.as_raw_handle()),
            &mut status,
            buffer.as_mut_ptr().cast(),
            BUFFER_BYTES as u32,
            FileStreamInformation,
        )
    };
    if result.0 != 0 || status.Information > BUFFER_BYTES {
        return Err(io::Error::other("incomplete recovery stream inventory"));
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), status.Information) };
    let inventory = parse(bytes, limit)?;
    if file_identity(file, FileKind::File)? != identity {
        return Err(crate::invalid("recovery stream file identity changed"));
    }
    Ok(inventory)
}

fn parse(bytes: &[u8], limit: u64) -> io::Result<StreamInventory> {
    let invalid = || crate::invalid("invalid or excessive recovery stream inventory");
    let mut result = StreamInventory {
        streams: Vec::new(),
        logical_bytes: 0,
    };
    let mut offset = 0;
    loop {
        let entry = bytes.get(offset..).ok_or_else(invalid)?;
        if entry.len() < 24 || result.streams.len() == MAX_STREAMS {
            return Err(invalid());
        }
        let next = u32::from_le_bytes(entry[..4].try_into().unwrap()) as usize;
        let length = u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize;
        let size = i64::from_le_bytes(entry[8..16].try_into().unwrap());
        let allocation = i64::from_le_bytes(entry[16..24].try_into().unwrap());
        if length == 0 || length > 1024 || !length.is_multiple_of(2) || size < 0 || allocation < 0 {
            return Err(invalid());
        }
        let end = 24 + length;
        let wide: Vec<u16> = entry
            .get(24..end)
            .ok_or_else(invalid)?
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        let name = String::from_utf16(&wide).map_err(|_| invalid())?;
        let component = name
            .strip_prefix(':')
            .and_then(|part| part.strip_suffix(":$DATA"))
            .ok_or_else(invalid)?;
        if component
            .chars()
            .any(|c| c.is_control() || "\\/:*?\"<>|".contains(c))
            || result.streams.iter().any(|stream| stream.name == name)
        {
            return Err(invalid());
        }
        result.logical_bytes = result
            .logical_bytes
            .checked_add(size as u64)
            .ok_or_else(invalid)?;
        if result.logical_bytes > limit {
            return Err(invalid());
        }
        result.streams.push(StreamInfo {
            name,
            bytes: size as u64,
        });
        if next == 0 {
            break;
        }
        if next < end || !next.is_multiple_of(8) || next >= entry.len() {
            return Err(invalid());
        }
        offset += next;
    }
    if !result.streams.iter().any(|stream| stream.name == "::$DATA") {
        return Err(invalid());
    }
    result.streams.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(name: &str, bytes: i64) -> Vec<u8> {
        let wide: Vec<u16> = name.encode_utf16().collect();
        let mut result = vec![0; 24];
        result[4..8].copy_from_slice(&((wide.len() * 2) as u32).to_le_bytes());
        result[8..16].copy_from_slice(&bytes.to_le_bytes());
        for unit in wide {
            result.extend_from_slice(&unit.to_le_bytes());
        }
        result
    }
    #[test]
    fn stream_parser_rejects_partial_malformed_and_unbounded_results() {
        assert!(parse(&[], 100).is_err());
        let valid = entry("::$DATA", 4);
        assert_eq!(parse(&valid, 4).unwrap().logical_bytes, 4);
        assert!(parse(&valid, 3).is_err());
        assert!(parse(&entry("::$DATA", -1), 100).is_err());
        assert!(parse(&entry(":hidden:$DATA", 1), 100).is_err());
        assert!(parse(&entry(":x:$OTHER", 1), 100).is_err());
        assert!(parse(&entry(":../x:$DATA", 1), 100).is_err());
        let mut overlap = valid.clone();
        overlap[..4].copy_from_slice(&8u32.to_le_bytes());
        assert!(parse(&overlap, 100).is_err());
        assert!(parse(&valid[..valid.len() - 1], 100).is_err());
        let mut duplicate = valid;
        duplicate.resize(duplicate.len().next_multiple_of(8), 0);
        let next = duplicate.len() as u32;
        duplicate[..4].copy_from_slice(&next.to_le_bytes());
        duplicate.extend(entry("::$DATA", 1));
        assert!(parse(&duplicate, 100).is_err());
    }
}
