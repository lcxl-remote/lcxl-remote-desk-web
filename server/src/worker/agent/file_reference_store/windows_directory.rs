//! Windows directory enumeration and references issued from pinned native handles.
use super::*;
use desk_file_recovery::windows::FileKind;

pub(super) fn enumerate_directory(
    stored: &StoredFile,
    opened: &OpenedFile,
    max_entries: usize,
    filter: &ValidatedDirectoryFilter,
) -> Result<(Vec<DirectoryEntryProjection>, bool), AgentError> {
    let (rows, more, limit_reached) =
        enumerate_directory_from(stored, opened, 0, max_entries, filter)?;
    Ok((rows, more || limit_reached))
}

pub(super) fn enumerate_directory_from(
    stored: &StoredFile,
    opened: &OpenedFile,
    skip_matches: usize,
    max_entries: usize,
    filter: &ValidatedDirectoryFilter,
) -> Result<(Vec<DirectoryEntryProjection>, bool, bool), AgentError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
        FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandleEx,
    };

    // Pin every checked ancestor until all child references are issued.
    // The namespace cannot be renamed or replaced during child path opens.
    let (parent, _ancestors, _) =
        windows_path_anchor::open_anchored(&stored.path, FileKind::Directory)?;
    if parent.identity != opened.identity || parent.identity != stored.identity {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected directory changed before enumeration",
            false,
        ));
    }
    let mut rows = Vec::new();
    let mut scanned = 0;
    let mut matched = 0;
    let mut restart = true;
    loop {
        let mut buffer = vec![0u8; 64 * 1024];
        let class = if restart {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        restart = false;
        let result = unsafe {
            GetFileInformationByHandleEx(
                HANDLE(opened.handle.as_raw_handle()),
                class,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
            )
        };
        if let Err(cause) = result {
            if cause.code() == windows::core::HRESULT::from_win32(ERROR_NO_MORE_FILES.0) {
                return Ok((rows, false, false));
            }
            return Err(error(
                AgentErrorKind::InvalidInput,
                format!("enumerate selected directory handle: {cause}"),
                false,
            ));
        }

        let mut offset = 0usize;
        loop {
            let header_len = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if offset + header_len > buffer.len() {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "directory enumeration returned a malformed record",
                    false,
                ));
            }
            let info = unsafe {
                std::ptr::read_unaligned(
                    buffer.as_ptr().add(offset).cast::<FILE_ID_BOTH_DIR_INFO>(),
                )
            };
            let name_len = info.FileNameLength as usize;
            let name_start = offset + header_len;
            if !name_len.is_multiple_of(2) || name_start + name_len > buffer.len() {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "directory enumeration returned an invalid file name",
                    false,
                ));
            }
            let name_utf16 = buffer[name_start..name_start + name_len]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            if scanned >= MAX_DIRECTORY_SCAN_ENTRIES {
                return Ok((rows, true, true));
            }
            scanned += 1;
            let display_name = String::from_utf16_lossy(&name_utf16);
            let exact_name_matches = filter.file_name.as_deref().is_none_or(|requested| {
                String::from_utf16(&name_utf16).ok().as_deref() == Some(requested)
            });
            let reparse = info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
            if display_name != "." && display_name != ".." && !reparse && exact_name_matches {
                let is_directory = info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0;
                let byte_len = (!is_directory).then_some(info.EndOfFile.max(0) as u64);
                let modified_at = windows_file_time_value(info.LastWriteTime);
                let matches = if is_directory {
                    !filter.is_active()
                } else {
                    filter.matches_file(&display_name, byte_len.unwrap_or(0), modified_at)
                };
                if !matches {
                    if info.NextEntryOffset == 0 {
                        break;
                    }
                    let next = info.NextEntryOffset as usize;
                    if next < header_len || offset + next >= buffer.len() {
                        return Err(error(
                            AgentErrorKind::InvalidInput,
                            "directory enumeration returned an invalid record offset",
                            false,
                        ));
                    }
                    offset += next;
                    continue;
                }
                if matched < skip_matches {
                    matched += 1;
                } else if rows.len() >= max_entries {
                    return Ok((rows, true, false));
                } else {
                    matched += 1;
                    rows.push(DirectoryEntryProjection {
                        object_ref: if is_directory {
                            None
                        } else {
                            Some(issue_child(stored, &name_utf16, info.FileId as u64)?)
                        },
                        parent_snapshot_id: stored.snapshot_id.clone(),
                        display_name: display_name.chars().take(512).collect(),
                        is_directory,
                        byte_len,
                        modified_at: modified_at.map(|timestamp| timestamp.to_rfc3339()),
                    });
                }
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            let next = info.NextEntryOffset as usize;
            if next < header_len || offset + next >= buffer.len() {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "directory enumeration returned an invalid record offset",
                    false,
                ));
            }
            offset += next;
        }
    }
}

fn issue_child(
    stored: &StoredFile,
    name_utf16: &[u16],
    expected_id: u64,
) -> Result<ObjectRef, AgentError> {
    let name = String::from_utf16(name_utf16).map_err(|_| {
        error(
            AgentErrorKind::InvalidInput,
            "child file name is not valid Unicode",
            false,
        )
    })?;
    let path = stored.path.join(&name);
    let (child, _ancestors, _) = windows_path_anchor::open_anchored(&path, FileKind::File)?;
    if child.identity.primary != stored.identity.primary || child.identity.secondary != expected_id
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "directory child changed before reference issuance",
            false,
        ));
    }
    issue_opened_with_lifetime(&path, child, DURABLE_ARTIFACT_REF_TTL_SECS, true)
}
