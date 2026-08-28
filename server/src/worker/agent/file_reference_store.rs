//! Worker-lifetime, edge-issued references for explicit file selections.
//!
//! The file manager lists native paths for the human UI, but the Assistant is
//! only allowed to carry a short-lived `ObjectRef`. Resolution reopens the exact
//! path without following a final reparse point and compares filesystem identity
//! before returning bounded metadata. File contents are never read here.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
#[cfg(windows)]
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use desk_agent_protocol::computer_use::{
    DirectoryEntryProjection, FileContentReadOutput, FileContentReadParams,
    FileMetadataInspectOutput, FileMetadataInspectParams, FileMetadataProjection, ObjectKind,
    ObjectRef,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
#[cfg(windows)]
use sha2::{Digest, Sha256};

const FILE_REF_TTL_SECS: i64 = 5 * 60;
const MAX_FILE_REFS: usize = 8_192;
const MAX_SELECTED_ROOTS: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 256;
const MAX_TEXT_READ_BYTES: u32 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    primary: u64,
    secondary: u64,
}

#[derive(Clone)]
struct StoredFile {
    snapshot_id: String,
    expires_at: DateTime<Utc>,
    object_kind: ObjectKind,
    path: PathBuf,
    identity: FileIdentity,
}

struct StoreState {
    incarnation: String,
    sequence: u64,
    objects: HashMap<String, StoredFile>,
}

impl Default for StoreState {
    fn default() -> Self {
        Self {
            incarnation: uuid::Uuid::new_v4().to_string(),
            sequence: 0,
            objects: HashMap::new(),
        }
    }
}

fn store() -> &'static Mutex<StoreState> {
    static STORE: OnceLock<Mutex<StoreState>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(StoreState::default()))
}

/// In-process workers can be recreated without restarting the executable.
/// Resetting here keeps old refs one-way stale across that worker boundary.
pub fn reset_worker_incarnation() {
    if let Ok(mut state) = store().lock() {
        *state = StoreState::default();
    }
    super::spreadsheet_file::reset_preview_store();
}

/// Mint one short-lived reference from the exact filesystem object currently
/// opened at `path`. Failure only removes the Assistant affordance from that
/// row; it must not break ordinary file-manager browsing.
pub fn issue(path: &Path) -> Result<ObjectRef, AgentError> {
    let opened = open_verified(path)?;
    let object_kind = if opened.metadata.is_dir() {
        ObjectKind::Directory
    } else if opened.metadata.is_file() {
        ObjectKind::File
    } else {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected filesystem object is not a regular file or directory",
            false,
        ));
    };
    let expires_at = Utc::now() + Duration::seconds(FILE_REF_TTL_SECS);
    let token = uuid::Uuid::new_v4().to_string();
    let mut state = store().lock().map_err(|_| {
        error(
            AgentErrorKind::Internal,
            "file reference store is unavailable",
            true,
        )
    })?;
    state
        .objects
        .retain(|_, object| object.expires_at > Utc::now());
    if state.objects.len() >= MAX_FILE_REFS {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "file reference store reached its bounded capacity",
            true,
        ));
    }
    state.sequence = state.sequence.saturating_add(1);
    let snapshot_id = format!("{}:{}", state.incarnation, state.sequence);
    let object_ref = ObjectRef {
        token: token.clone(),
        snapshot_id: snapshot_id.clone(),
        object_kind,
        expires_at: expires_at.to_rfc3339(),
    };
    state.objects.insert(
        token,
        StoredFile {
            snapshot_id,
            expires_at,
            object_kind,
            path: path.to_path_buf(),
            identity: opened.identity,
        },
    );
    Ok(object_ref)
}

pub fn inspect(
    params: &FileMetadataInspectParams,
) -> Result<FileMetadataInspectOutput, AgentError> {
    if params.roots.is_empty()
        || params.roots.len() > MAX_SELECTED_ROOTS
        || params.max_entries == 0
        || params.max_entries > MAX_DIRECTORY_ENTRIES as u32
        || params.max_bytes < 512
        || params.max_bytes > 256 * 1024
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "file metadata bounds exceed the selected-root ceiling",
            false,
        ));
    }
    let directory_filter = ValidatedDirectoryFilter::from_params(params)?;

    let mut entries = Vec::new();
    let mut directory_entries = Vec::new();
    let mut truncated = false;
    for object_ref in &params.roots {
        if entries.len() >= params.max_entries as usize {
            truncated = true;
            break;
        }
        let stored = resolve(object_ref)?;
        let opened =
            if params.enumerate_directories && object_ref.object_kind == ObjectKind::Directory {
                open_verified_for_read(&stored.path)?
            } else {
                open_verified(&stored.path)?
            };
        if opened.identity != stored.identity {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "selected filesystem object changed after reference issuance",
                false,
            ));
        }
        let projection = FileMetadataProjection {
            object_ref: object_ref.clone(),
            display_name: stored
                .path
                .file_name()
                .map(|name| name.to_string_lossy().chars().take(512).collect())
                .unwrap_or_else(|| stored.path.to_string_lossy().chars().take(512).collect()),
            is_directory: opened.metadata.is_dir(),
            byte_len: opened.metadata.is_file().then_some(opened.metadata.len()),
            modified_at: opened
                .metadata
                .modified()
                .ok()
                .map(DateTime::<Utc>::from)
                .map(|value| value.to_rfc3339()),
        };
        let mut candidate = entries.clone();
        candidate.push(projection.clone());
        let encoded = serde_json::to_vec(&candidate).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "cannot encode selected file metadata",
                false,
            )
        })?;
        if encoded.len() > params.max_bytes as usize {
            truncated = true;
            break;
        }
        entries.push(projection);
        if params.enumerate_directories && opened.metadata.is_dir() {
            let remaining = params
                .max_entries
                .saturating_sub((entries.len() + directory_entries.len()) as u32)
                as usize;
            if remaining == 0 {
                truncated = true;
                break;
            }
            let (mut listed, listed_truncated) =
                enumerate_directory(&stored, &opened, remaining, &directory_filter)?;
            directory_entries.append(&mut listed);
            truncated |= listed_truncated;
        }

        let encoded = serde_json::to_vec(&(&entries, &directory_entries)).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "cannot encode selected file metadata",
                false,
            )
        })?;
        if encoded.len() > params.max_bytes as usize {
            while !directory_entries.is_empty()
                && serde_json::to_vec(&(&entries, &directory_entries))
                    .is_ok_and(|value| value.len() > params.max_bytes as usize)
            {
                directory_entries.pop();
            }
            truncated = true;
        }
    }
    Ok(FileMetadataInspectOutput {
        snapshot_id: format!("file-selection-{}", uuid::Uuid::new_v4()),
        entries,
        directory_entries,
        truncated,
    })
}

#[derive(Debug, Default)]
struct ValidatedDirectoryFilter {
    file_extensions: Vec<String>,
    min_file_bytes: Option<u64>,
    max_file_bytes: Option<u64>,
    modified_after: Option<DateTime<Utc>>,
    modified_before: Option<DateTime<Utc>>,
}

impl ValidatedDirectoryFilter {
    fn from_params(params: &FileMetadataInspectParams) -> Result<Self, AgentError> {
        if params.file_extensions.len() > 16 {
            return Err(invalid_file_filter());
        }
        let mut file_extensions = Vec::with_capacity(params.file_extensions.len());
        for extension in &params.file_extensions {
            let bytes = extension.as_bytes();
            if !(2..=17).contains(&bytes.len())
                || bytes[0] != b'.'
                || !bytes[1].is_ascii_alphanumeric()
                || !bytes[1..].iter().all(|value| {
                    value.is_ascii_alphanumeric() || matches!(value, b'.' | b'_' | b'-')
                })
            {
                return Err(invalid_file_filter());
            }
            let normalized = extension.to_ascii_lowercase();
            if !file_extensions.contains(&normalized) {
                file_extensions.push(normalized);
            }
        }
        let modified_after = parse_filter_time(params.modified_after.as_deref())?;
        let modified_before = parse_filter_time(params.modified_before.as_deref())?;
        if params
            .min_file_bytes
            .zip(params.max_file_bytes)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
            || modified_after
                .zip(modified_before)
                .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err(invalid_file_filter());
        }
        Ok(Self {
            file_extensions,
            min_file_bytes: params.min_file_bytes,
            max_file_bytes: params.max_file_bytes,
            modified_after,
            modified_before,
        })
    }

    fn is_active(&self) -> bool {
        !self.file_extensions.is_empty()
            || self.min_file_bytes.is_some()
            || self.max_file_bytes.is_some()
            || self.modified_after.is_some()
            || self.modified_before.is_some()
    }

    fn matches_file(
        &self,
        display_name: &str,
        byte_len: u64,
        modified_at: Option<DateTime<Utc>>,
    ) -> bool {
        if !self.file_extensions.is_empty()
            && !self.file_extensions.iter().any(|extension| {
                display_name
                    .get(display_name.len().saturating_sub(extension.len())..)
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(extension))
            })
        {
            return false;
        }
        if self
            .min_file_bytes
            .is_some_and(|minimum| byte_len < minimum)
            || self
                .max_file_bytes
                .is_some_and(|maximum| byte_len > maximum)
        {
            return false;
        }
        if self.modified_after.is_some() || self.modified_before.is_some() {
            let Some(modified_at) = modified_at else {
                return false;
            };
            if self
                .modified_after
                .is_some_and(|minimum| modified_at < minimum)
                || self
                    .modified_before
                    .is_some_and(|maximum| modified_at > maximum)
            {
                return false;
            }
        }
        true
    }
}

fn parse_filter_time(value: Option<&str>) -> Result<Option<DateTime<Utc>>, AgentError> {
    value
        .map(|value| {
            DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&Utc))
                .map_err(|_| invalid_file_filter())
        })
        .transpose()
}

fn invalid_file_filter() -> AgentError {
    error(
        AgentErrorKind::InvalidInput,
        "file metadata filter is outside the extension, size, or RFC3339 time bounds",
        false,
    )
}

#[cfg(windows)]
fn enumerate_directory(
    stored: &StoredFile,
    opened: &OpenedFile,
    max_entries: usize,
    filter: &ValidatedDirectoryFilter,
) -> Result<(Vec<DirectoryEntryProjection>, bool), AgentError> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ID_BOTH_DIR_INFO,
        FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandleEx,
    };

    let mut rows = Vec::new();
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
                return Ok((rows, false));
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
            let display_name = String::from_utf16_lossy(&name_utf16);
            let reparse = info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0;
            if display_name != "." && display_name != ".." && !reparse {
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
                if rows.len() >= max_entries {
                    return Ok((rows, true));
                }
                rows.push(DirectoryEntryProjection {
                    parent_snapshot_id: stored.snapshot_id.clone(),
                    display_name: display_name.chars().take(512).collect(),
                    is_directory,
                    byte_len,
                    modified_at: modified_at.map(|timestamp| timestamp.to_rfc3339()),
                });
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

#[cfg(windows)]
fn windows_file_time_value(value: i64) -> Option<DateTime<Utc>> {
    const WINDOWS_TO_UNIX_100NS: i64 = 116_444_736_000_000_000;
    let unix_100ns = value.checked_sub(WINDOWS_TO_UNIX_100NS)?;
    let seconds = unix_100ns.div_euclid(10_000_000);
    let nanos = unix_100ns.rem_euclid(10_000_000) as u32 * 100;
    DateTime::<Utc>::from_timestamp(seconds, nanos)
}

#[cfg(not(windows))]
fn enumerate_directory(
    _stored: &StoredFile,
    _opened: &OpenedFile,
    _max_entries: usize,
    _filter: &ValidatedDirectoryFilter,
) -> Result<(Vec<DirectoryEntryProjection>, bool), AgentError> {
    Err(error(
        AgentErrorKind::UnsupportedCapability,
        "handle-relative directory enumeration is currently enabled only on Windows",
        false,
    ))
}

#[cfg(windows)]
pub fn read_text(params: &FileContentReadParams) -> Result<FileContentReadOutput, AgentError> {
    if params.max_bytes == 0 || params.max_bytes > MAX_TEXT_READ_BYTES {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected text read exceeds the 64 KiB ceiling",
            false,
        ));
    }
    if params.file.object_kind != ObjectKind::File {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected text read requires a regular file reference",
            false,
        ));
    }
    let stored = resolve(&params.file)?;
    let mut opened = open_verified_for_read(&stored.path)?;
    if opened.identity != stored.identity || !opened.metadata.is_file() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected file changed after reference issuance",
            false,
        ));
    }
    if opened.metadata.len() > params.max_bytes as u64 {
        return Err(error(
            AgentErrorKind::InvalidInput,
            format!(
                "selected file is {} bytes; maximum text read is {} bytes",
                opened.metadata.len(),
                params.max_bytes
            ),
            false,
        ));
    }
    let mut bytes = Vec::with_capacity(opened.metadata.len() as usize);
    Read::by_ref(&mut opened.handle)
        .take(params.max_bytes as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| io_error("read selected file handle", cause))?;
    if bytes.len() > params.max_bytes as usize {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected file grew beyond the text read ceiling while it was being read",
            false,
        ));
    }
    if bytes.contains(&0) {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected file contains NUL bytes and is not treated as text",
            false,
        ));
    }
    let content_utf8 = String::from_utf8(bytes.clone()).map_err(|_| {
        error(
            AgentErrorKind::InvalidInput,
            "selected file is not valid UTF-8 text",
            false,
        )
    })?;
    Ok(FileContentReadOutput {
        file: params.file.clone(),
        display_name: stored
            .path
            .file_name()
            .map(|name| name.to_string_lossy().chars().take(512).collect())
            .unwrap_or_else(|| "selected-file".into()),
        content_utf8,
        byte_len: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
    })
}

#[cfg(not(windows))]
pub fn read_text(_params: &FileContentReadParams) -> Result<FileContentReadOutput, AgentError> {
    Err(error(
        AgentErrorKind::UnsupportedCapability,
        "handle-bound text reading is currently enabled only on Windows",
        false,
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedTextArtifact {
    pub file_name: String,
    pub byte_len: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedFileBytes {
    pub display_name: String,
    pub bytes: Vec<u8>,
    pub sha256: String,
}

#[cfg(windows)]
pub fn read_verified_bytes(
    file: &ObjectRef,
    max_bytes: u64,
) -> Result<VerifiedFileBytes, AgentError> {
    if max_bytes == 0 || max_bytes > 16 * 1024 * 1024 || file.object_kind != ObjectKind::File {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected file byte read exceeds its object or size ceiling",
            false,
        ));
    }
    let stored = resolve(file)?;
    let mut opened = open_verified_for_read(&stored.path)?;
    if opened.identity != stored.identity || !opened.metadata.is_file() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected file changed after reference issuance",
            false,
        ));
    }
    if opened.metadata.len() > max_bytes {
        return Err(error(
            AgentErrorKind::InvalidInput,
            format!(
                "selected file is {} bytes; maximum is {max_bytes} bytes",
                opened.metadata.len()
            ),
            false,
        ));
    }
    let mut bytes = Vec::with_capacity(opened.metadata.len() as usize);
    Read::by_ref(&mut opened.handle)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|cause| io_error("read selected file handle", cause))?;
    if bytes.len() as u64 > max_bytes {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected file grew beyond the byte read ceiling while it was being read",
            false,
        ));
    }
    Ok(VerifiedFileBytes {
        display_name: stored
            .path
            .file_name()
            .map(|name| name.to_string_lossy().chars().take(512).collect())
            .unwrap_or_else(|| "selected-file".into()),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        bytes,
    })
}

#[cfg(not(windows))]
pub fn read_verified_bytes(
    _file: &ObjectRef,
    _max_bytes: u64,
) -> Result<VerifiedFileBytes, AgentError> {
    Err(error(
        AgentErrorKind::UnsupportedCapability,
        "handle-bound file reading is currently enabled only on Windows",
        false,
    ))
}

/// Resolve explicit spreadsheet files and direct spreadsheet children of an
/// explicitly selected directory. Directory children are never minted as
/// reusable references: they are enumerated and opened relative to the same
/// retained directory handle within this call.
#[cfg(windows)]
pub fn read_verified_spreadsheet_inputs(
    roots: &[ObjectRef],
    max_files: usize,
    max_file_bytes: u64,
) -> Result<(Vec<VerifiedFileBytes>, bool), AgentError> {
    use anyhow::{Context, anyhow, bail};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows::Wdk::Storage::FileSystem::{
        FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
        NtCreateFile,
    };
    use windows::Win32::Foundation::{
        HANDLE, OBJ_CASE_INSENSITIVE, STATUS_SUCCESS, UNICODE_STRING,
    };
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, GetFileInformationByHandle,
        SYNCHRONIZE,
    };
    use windows::Win32::System::IO::IO_STATUS_BLOCK;
    use windows::core::PWSTR;

    fn read_relative(
        directory: &File,
        display_name: &str,
        max_file_bytes: u64,
    ) -> anyhow::Result<VerifiedFileBytes> {
        if display_name.is_empty()
            || display_name.len() > 512
            || matches!(display_name, "." | "..")
            || display_name
                .chars()
                .any(|character| character.is_control() || "\\/:*?\"<>|".contains(character))
        {
            bail!("enumerated spreadsheet name is not one safe Windows leaf component");
        }
        let mut utf16 = display_name.encode_utf16().collect::<Vec<_>>();
        let byte_len = utf16
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(|| anyhow!("spreadsheet name exceeds UNICODE_STRING bounds"))?;
        let unicode_name = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: PWSTR(utf16.as_mut_ptr()),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: HANDLE(directory.as_raw_handle()),
            ObjectName: &unicode_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = HANDLE::default();
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_GENERIC_READ | SYNCHRONIZE,
                &attributes,
                &mut io_status,
                None,
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                FILE_OPEN,
                FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
                None,
                0,
            )
        };
        if status != STATUS_SUCCESS {
            bail!(
                "open directory spreadsheet failed closed with NTSTATUS {:#x}",
                status.0 as u32
            );
        }
        let mut file = unsafe { File::from_raw_handle(handle.0) };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information)
                .context("read directory spreadsheet attributes")?;
        }
        if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            bail!("directory spreadsheet child is a reparse point");
        }
        let metadata = file
            .metadata()
            .context("read directory spreadsheet metadata")?;
        if !metadata.is_file() || metadata.len() > max_file_bytes {
            bail!("directory spreadsheet child exceeds its type or size ceiling");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(max_file_bytes + 1)
            .read_to_end(&mut bytes)
            .context("read directory spreadsheet handle")?;
        if bytes.len() as u64 > max_file_bytes {
            bail!("directory spreadsheet child grew beyond its size ceiling");
        }
        Ok(VerifiedFileBytes {
            display_name: display_name.to_string(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes,
        })
    }

    if roots.is_empty() || max_files == 0 || max_files > 8 || max_file_bytes == 0 {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "spreadsheet directory expansion exceeds its frozen ceiling",
            false,
        ));
    }
    let filter = ValidatedDirectoryFilter {
        file_extensions: vec![".xlsx".into(), ".csv".into(), ".tsv".into()],
        ..Default::default()
    };
    let mut files = Vec::new();
    let mut truncated = false;
    for root in roots {
        if files.len() >= max_files {
            truncated = true;
            break;
        }
        match root.object_kind {
            ObjectKind::File => files.push(read_verified_bytes(root, max_file_bytes)?),
            ObjectKind::Directory => {
                let stored = resolve(root)?;
                let opened = open_verified_for_read(&stored.path)?;
                if opened.identity != stored.identity || !opened.metadata.is_dir() {
                    return Err(error(
                        AgentErrorKind::InvalidInput,
                        "selected spreadsheet directory changed after reference issuance",
                        false,
                    ));
                }
                let (mut entries, directory_truncated) =
                    enumerate_directory(&stored, &opened, MAX_DIRECTORY_ENTRIES, &filter)?;
                entries.sort_by(|left, right| {
                    left.display_name
                        .to_ascii_lowercase()
                        .cmp(&right.display_name.to_ascii_lowercase())
                        .then_with(|| left.display_name.cmp(&right.display_name))
                });
                let remaining = max_files - files.len();
                truncated |= directory_truncated || entries.len() > remaining;
                for entry in entries.into_iter().take(remaining) {
                    files.push(
                        read_relative(&opened.handle, &entry.display_name, max_file_bytes)
                            .map_err(|cause| {
                                error(
                                    AgentErrorKind::InvalidInput,
                                    format!("read selected directory spreadsheet: {cause}"),
                                    false,
                                )
                            })?,
                    );
                }
            }
            _ => {
                return Err(error(
                    AgentErrorKind::InvalidInput,
                    "spreadsheet input requires file or directory references",
                    false,
                ));
            }
        }
    }
    Ok((files, truncated))
}

#[cfg(not(windows))]
pub fn read_verified_spreadsheet_inputs(
    _roots: &[ObjectRef],
    _max_files: usize,
    _max_file_bytes: u64,
) -> Result<(Vec<VerifiedFileBytes>, bool), AgentError> {
    Err(error(
        AgentErrorKind::UnsupportedCapability,
        "handle-bound spreadsheet directory expansion is currently Windows-only",
        false,
    ))
}

/// Create one new artifact relative to the retained selected-directory handle.
/// The local allowlist is intentionally stricter than path containment for this
/// first production slice: the selected directory must have the same filesystem
/// identity as one exact host-configured root. No string path is used after the
/// handles have been opened and compared.
#[cfg(windows)]
pub fn create_binary_artifact(
    directory: &ObjectRef,
    allowed_roots: &[String],
    file_name: &str,
    content_bytes: &[u8],
) -> Result<CreatedTextArtifact, AgentError> {
    use anyhow::{Context, anyhow, bail};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_THROUGH, NTCREATEFILE_CREATE_DISPOSITION,
        NtCreateFile,
    };
    use windows::Win32::Foundation::{
        HANDLE, OBJ_CASE_INSENSITIVE, STATUS_SUCCESS, UNICODE_STRING,
    };
    use windows::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FileIdInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
    };
    use windows::Win32::System::IO::IO_STATUS_BLOCK;
    use windows::core::PWSTR;

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Identity {
        volume_serial: u64,
        file_id: [u8; 16],
    }

    fn identity(handle: &File) -> anyhow::Result<Identity> {
        let mut information = FILE_ID_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                HANDLE(handle.as_raw_handle()),
                FileIdInfo,
                std::ptr::addr_of_mut!(information).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
            .context("read handle file identity")?;
        }
        Ok(Identity {
            volume_serial: information.VolumeSerialNumber,
            file_id: information.FileId.Identifier,
        })
    }

    fn validate_name(name: &str) -> anyhow::Result<()> {
        if name.is_empty()
            || name.len() > 200
            || matches!(name, "." | "..")
            || name.ends_with(['.', ' '])
            || name
                .chars()
                .any(|character| character.is_control() || "\\/:*?\"<>|".contains(character))
        {
            bail!("artifact name is not one safe Windows leaf component");
        }
        Ok(())
    }

    fn relative_file(
        root: &File,
        name: &str,
        disposition: NTCREATEFILE_CREATE_DISPOSITION,
    ) -> anyhow::Result<File> {
        let mut utf16 = name.encode_utf16().collect::<Vec<_>>();
        let byte_len = utf16
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|length| u16::try_from(length).ok())
            .ok_or_else(|| anyhow!("artifact name exceeds UNICODE_STRING bounds"))?;
        let unicode_name = UNICODE_STRING {
            Length: byte_len,
            MaximumLength: byte_len,
            Buffer: PWSTR(utf16.as_mut_ptr()),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: HANDLE(root.as_raw_handle()),
            ObjectName: &unicode_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: std::ptr::null(),
            SecurityQualityOfService: std::ptr::null(),
        };
        let mut handle = HANDLE::default();
        let mut io_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | SYNCHRONIZE,
                &attributes,
                &mut io_status,
                None,
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                disposition,
                FILE_NON_DIRECTORY_FILE
                    | FILE_OPEN_REPARSE_POINT
                    | FILE_SYNCHRONOUS_IO_NONALERT
                    | FILE_WRITE_THROUGH,
                None,
                0,
            )
        };
        if status != STATUS_SUCCESS {
            bail!(
                "create-new artifact failed closed with NTSTATUS {:#x}",
                status.0 as u32
            );
        }
        Ok(unsafe { File::from_raw_handle(handle.0) })
    }

    let stored = resolve(directory)?;
    if stored.object_kind != ObjectKind::Directory {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "artifact creation requires one selected directory reference",
            false,
        ));
    }
    if content_bytes.len() > 4 * 1024 * 1024 {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "artifact content exceeds the 4 MiB binary artifact ceiling",
            false,
        ));
    }
    let selected = open_verified(&stored.path)?;
    if selected.identity != stored.identity || !selected.metadata.is_dir() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected directory changed after reference issuance",
            false,
        ));
    }
    let selected_identity = identity(&selected.handle).map_err(|cause| {
        io_error(
            "read selected directory identity",
            std::io::Error::other(cause),
        )
    })?;
    let allowlisted = allowed_roots.iter().any(|root| {
        open_verified(Path::new(root))
            .ok()
            .filter(|opened| opened.metadata.is_dir())
            .and_then(|opened| identity(&opened.handle).ok())
            == Some(selected_identity)
    });
    if !allowlisted {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "selected directory is not an exact host-approved artifact root",
            false,
        ));
    }
    let result = (|| -> anyhow::Result<CreatedTextArtifact> {
        validate_name(file_name)?;
        let parent_identity = identity(&selected.handle)?;
        let mut created = relative_file(&selected.handle, file_name, FILE_CREATE)?;
        created.write_all(content_bytes)?;
        created.sync_all()?;
        let created_identity = identity(&created)?;
        drop(created);
        #[cfg(test)]
        run_artifact_after_close_hook();
        if identity(&selected.handle)? != parent_identity {
            bail!("target parent identity changed during artifact creation");
        }
        let mut verified = relative_file(&selected.handle, file_name, FILE_OPEN)?;
        if identity(&verified)? != created_identity {
            bail!("artifact identity changed before read-back verification");
        }
        let mut bytes = Vec::new();
        verified.seek(SeekFrom::Start(0))?;
        verified.read_to_end(&mut bytes)?;
        if bytes != content_bytes {
            bail!("artifact read-back differs from requested bytes");
        }
        Ok(CreatedTextArtifact {
            file_name: file_name.to_string(),
            byte_len: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        })
    })();
    result.map_err(|cause| {
        error(
            AgentErrorKind::InvalidInput,
            format!("create verified artifact: {cause}"),
            false,
        )
    })
}

#[cfg(windows)]
pub fn create_text_artifact(
    directory: &ObjectRef,
    allowed_roots: &[String],
    file_name: &str,
    content_utf8: &str,
) -> Result<CreatedTextArtifact, AgentError> {
    if content_utf8.len() > 64 * 1024 {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "artifact content exceeds the 64 KiB text artifact ceiling",
            false,
        ));
    }
    create_binary_artifact(directory, allowed_roots, file_name, content_utf8.as_bytes())
}

#[cfg(not(windows))]
pub fn create_binary_artifact(
    _directory: &ObjectRef,
    _allowed_roots: &[String],
    _file_name: &str,
    _content_bytes: &[u8],
) -> Result<CreatedTextArtifact, AgentError> {
    Err(error(
        AgentErrorKind::UnsupportedCapability,
        "verified artifact creation is currently Windows-only",
        false,
    ))
}

#[cfg(not(windows))]
pub fn create_text_artifact(
    _directory: &ObjectRef,
    _allowed_roots: &[String],
    _file_name: &str,
    _content_utf8: &str,
) -> Result<CreatedTextArtifact, AgentError> {
    Err(error(
        AgentErrorKind::UnsupportedCapability,
        "verified artifact creation is currently Windows-only",
        false,
    ))
}

fn resolve(object_ref: &ObjectRef) -> Result<StoredFile, AgentError> {
    if !matches!(
        object_ref.object_kind,
        ObjectKind::File | ObjectKind::Directory
    ) {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "file metadata inspection requires file or directory references",
            false,
        ));
    }
    let mut state = store().lock().map_err(|_| {
        error(
            AgentErrorKind::Internal,
            "file reference store is unavailable",
            true,
        )
    })?;
    let now = Utc::now();
    state.objects.retain(|_, object| object.expires_at > now);
    let stored = state.objects.get(&object_ref.token).ok_or_else(|| {
        error(
            AgentErrorKind::InvalidInput,
            "file reference is stale or unknown",
            false,
        )
    })?;
    if stored.snapshot_id != object_ref.snapshot_id
        || stored.object_kind != object_ref.object_kind
        || stored.expires_at.to_rfc3339() != object_ref.expires_at
        || !object_ref.snapshot_id.starts_with(&state.incarnation)
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "file reference does not belong to this worker incarnation",
            false,
        ));
    }
    Ok(stored.clone())
}

struct OpenedFile {
    #[allow(dead_code)]
    handle: File,
    metadata: std::fs::Metadata,
    identity: FileIdentity,
}

#[cfg(windows)]
fn open_verified(path: &Path) -> Result<OpenedFile, AgentError> {
    use windows::Win32::Storage::FileSystem::{
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    open_verified_with_access(
        path,
        FILE_READ_ATTRIBUTES.0,
        (FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0,
    )
}

#[cfg(windows)]
fn open_verified_for_read(path: &Path) -> Result<OpenedFile, AgentError> {
    use windows::Win32::Storage::FileSystem::{
        FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ,
    };

    // Excluding FILE_SHARE_WRITE turns the retained read handle into a stable
    // snapshot boundary: a concurrent writer must finish before this open or
    // wait until the bounded read/enumeration closes the handle. Rename/delete
    // remains allowed, but the handle continues to name the same identity.
    open_verified_with_access(
        path,
        FILE_GENERIC_READ.0,
        (FILE_SHARE_READ | FILE_SHARE_DELETE).0,
    )
}

#[cfg(windows)]
fn open_verified_with_access(
    path: &Path,
    access_mode: u32,
    share_mode: u32,
) -> Result<OpenedFile, AgentError> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
    };

    let file = OpenOptions::new()
        .access_mode(access_mode)
        .share_mode(share_mode)
        .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).0)
        .open(path)
        .map_err(|cause| io_error("open selected filesystem object", cause))?;
    let metadata = file
        .metadata()
        .map_err(|cause| io_error("read selected filesystem metadata", cause))?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information).map_err(
            |cause| {
                error(
                    AgentErrorKind::InvalidInput,
                    format!("read selected filesystem identity: {cause}"),
                    false,
                )
            },
        )?;
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "reparse-point file selections are not supported",
            false,
        ));
    }
    let primary = information.dwVolumeSerialNumber as u64;
    let secondary = ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64;
    Ok(OpenedFile {
        handle: file,
        metadata,
        identity: FileIdentity { primary, secondary },
    })
}

#[cfg(not(windows))]
fn open_verified(path: &Path) -> Result<OpenedFile, AgentError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|cause| io_error("inspect selected filesystem object", cause))?;
    if metadata.file_type().is_symlink() {
        return Err(error(
            AgentErrorKind::PermissionDenied,
            "symbolic-link file selections are not supported",
            false,
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|cause| io_error("open selected filesystem object", cause))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0);
    Ok(OpenedFile {
        handle: file,
        identity: FileIdentity {
            primary: metadata.len(),
            secondary: modified,
        },
        metadata,
    })
}

#[cfg(not(windows))]
fn open_verified_for_read(path: &Path) -> Result<OpenedFile, AgentError> {
    open_verified(path)
}

fn io_error(operation: &str, cause: std::io::Error) -> AgentError {
    error(
        AgentErrorKind::InvalidInput,
        format!("{operation}: {cause}"),
        false,
    )
}

fn error(kind: AgentErrorKind, message: impl Into<String>, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.into(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
pub(super) fn file_store_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

#[cfg(all(test, windows))]
type ArtifactAfterCloseHook = Box<dyn FnOnce() + Send>;

#[cfg(all(test, windows))]
fn artifact_after_close_hook() -> &'static Mutex<Option<ArtifactAfterCloseHook>> {
    static HOOK: OnceLock<Mutex<Option<ArtifactAfterCloseHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(all(test, windows))]
fn set_artifact_after_close_hook(hook: ArtifactAfterCloseHook) {
    *artifact_after_close_hook().lock().unwrap() = Some(hook);
}

#[cfg(all(test, windows))]
fn run_artifact_after_close_hook() {
    if let Some(hook) = artifact_after_close_hook().lock().unwrap().take() {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn create_junction(link: &Path, target: &Path) {
        let output = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("launch mklink junction fixture");
        assert!(
            output.status.success(),
            "junction fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn issued_reference_reads_metadata_and_tampering_fails_closed() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selected.txt");
        std::fs::write(&path, b"metadata only").unwrap();
        reset_worker_incarnation();
        let object_ref = issue(&path).unwrap();
        let output = inspect(&FileMetadataInspectParams {
            roots: vec![object_ref.clone()],
            max_entries: 1,
            max_bytes: 4096,
            enumerate_directories: false,
            file_extensions: vec![],
            min_file_bytes: None,
            max_file_bytes: None,
            modified_after: None,
            modified_before: None,
        })
        .unwrap();
        assert_eq!(output.entries.len(), 1);
        assert_eq!(output.entries[0].display_name, "selected.txt");
        assert_eq!(output.entries[0].byte_len, Some(13));

        let mut tampered = object_ref;
        tampered.snapshot_id.push_str("-tampered");
        let error = inspect(&FileMetadataInspectParams {
            roots: vec![tampered],
            max_entries: 1,
            max_bytes: 4096,
            enumerate_directories: false,
            file_extensions: vec![],
            min_file_bytes: None,
            max_file_bytes: None,
            modified_after: None,
            modified_before: None,
        })
        .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    }

    #[test]
    fn worker_reset_makes_old_file_reference_stale() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selected.txt");
        std::fs::write(&path, b"metadata only").unwrap();
        reset_worker_incarnation();
        let object_ref = issue(&path).unwrap();
        reset_worker_incarnation();
        assert!(
            inspect(&FileMetadataInspectParams {
                roots: vec![object_ref],
                max_entries: 1,
                max_bytes: 4096,
                enumerate_directories: false,
                file_extensions: vec![],
                min_file_bytes: None,
                max_file_bytes: None,
                modified_after: None,
                modified_before: None,
            })
            .is_err()
        );
    }

    #[cfg(windows)]
    #[test]
    fn selected_exact_allowlisted_directory_creates_new_and_reads_back() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        reset_worker_incarnation();
        let directory = issue(temp.path()).unwrap();
        let allowed = vec![temp.path().to_string_lossy().to_string()];
        let created =
            create_text_artifact(&directory, &allowed, "stage3-r2.txt", "verified artifact")
                .unwrap();
        assert_eq!(created.byte_len, 17);
        assert_eq!(
            std::fs::read_to_string(temp.path().join("stage3-r2.txt")).unwrap(),
            "verified artifact"
        );
        assert!(
            create_text_artifact(&directory, &allowed, "stage3-r2.txt", "overwrite").is_err(),
            "FILE_CREATE must never overwrite an existing artifact"
        );
    }

    #[cfg(windows)]
    #[test]
    fn artifact_target_rename_competition_fails_identity_verification() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        reset_worker_incarnation();
        let directory = issue(temp.path()).unwrap();
        let allowed = vec![temp.path().to_string_lossy().to_string()];
        let target = temp.path().join("raced.txt");
        let displaced = temp.path().join("raced-original.txt");
        set_artifact_after_close_hook(Box::new(move || {
            std::fs::rename(&target, &displaced).unwrap();
            std::fs::write(&target, b"same bytes").unwrap();
        }));

        let error = create_text_artifact(&directory, &allowed, "raced.txt", "same bytes")
            .expect_err("a same-byte replacement must not pass identity verification");
        assert!(error.message.contains("artifact identity changed"));
    }

    #[cfg(windows)]
    #[test]
    fn selected_directory_enumeration_is_bounded_and_non_recursive() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), b"alpha").unwrap();
        std::fs::write(temp.path().join("b.csv"), b"x,y").unwrap();
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        std::fs::write(temp.path().join("nested").join("hidden.txt"), b"hidden").unwrap();
        reset_worker_incarnation();
        let directory = issue(temp.path()).unwrap();
        let output = inspect(&FileMetadataInspectParams {
            roots: vec![directory],
            max_entries: 256,
            max_bytes: 64 * 1024,
            enumerate_directories: true,
            file_extensions: vec![],
            min_file_bytes: None,
            max_file_bytes: None,
            modified_after: None,
            modified_before: None,
        })
        .unwrap();
        let names = output
            .directory_entries
            .iter()
            .map(|entry| entry.display_name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            names,
            std::collections::BTreeSet::from(["a.txt", "b.csv", "nested"])
        );
        assert!(!names.contains("hidden.txt"));
        assert!(!output.truncated);

        let bounded = inspect(&FileMetadataInspectParams {
            roots: vec![issue(temp.path()).unwrap()],
            max_entries: 2,
            max_bytes: 64 * 1024,
            enumerate_directories: true,
            file_extensions: vec![],
            min_file_bytes: None,
            max_file_bytes: None,
            modified_after: None,
            modified_before: None,
        })
        .unwrap();
        assert_eq!(bounded.directory_entries.len(), 1);
        assert!(bounded.truncated);
    }

    #[cfg(windows)]
    #[test]
    fn large_directory_enumeration_stops_at_the_shared_entry_ceiling() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        for index in 0..600 {
            std::fs::write(temp.path().join(format!("entry-{index:04}.txt")), b"x").unwrap();
        }
        reset_worker_incarnation();
        let output = inspect(&FileMetadataInspectParams {
            roots: vec![issue(temp.path()).unwrap()],
            max_entries: MAX_DIRECTORY_ENTRIES as u32,
            max_bytes: 256 * 1024,
            enumerate_directories: true,
            file_extensions: vec![],
            min_file_bytes: None,
            max_file_bytes: None,
            modified_after: None,
            modified_before: None,
        })
        .unwrap();

        assert_eq!(output.entries.len(), 1);
        assert_eq!(output.directory_entries.len(), MAX_DIRECTORY_ENTRIES - 1);
        assert!(output.truncated);
        assert!(
            output
                .directory_entries
                .iter()
                .all(|entry| entry.display_name.starts_with("entry-"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn parent_junction_replacement_cannot_rebind_a_selected_file() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let parent = temp.path().join("selected-parent");
        let retained = temp.path().join("retained-parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("selected.txt"), b"approved bytes").unwrap();
        std::fs::write(outside.path().join("selected.txt"), b"outside bytes").unwrap();
        reset_worker_incarnation();
        let selected = issue(&parent.join("selected.txt")).unwrap();

        std::fs::rename(&parent, &retained).unwrap();
        create_junction(&parent, outside.path());
        let error = read_text(&FileContentReadParams {
            file: selected,
            max_bytes: MAX_TEXT_READ_BYTES,
        })
        .expect_err("the stored path must not rebind through a replacement junction");
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
        assert!(!error.message.contains("outside bytes"));
    }

    #[cfg(windows)]
    #[test]
    fn retained_read_handle_excludes_concurrent_writers() {
        use std::os::windows::fs::OpenOptionsExt;

        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selected.txt");
        std::fs::write(&path, b"stable bytes").unwrap();
        let opened = open_verified_for_read(&path).unwrap();
        let concurrent_write = OpenOptions::new()
            .write(true)
            .share_mode(
                (windows::Win32::Storage::FileSystem::FILE_SHARE_READ
                    | windows::Win32::Storage::FileSystem::FILE_SHARE_WRITE
                    | windows::Win32::Storage::FileSystem::FILE_SHARE_DELETE)
                    .0,
            )
            .open(&path);
        assert!(
            concurrent_write.is_err(),
            "a bounded read handle must prevent in-place mutation during the snapshot"
        );
        drop(opened);
        assert!(OpenOptions::new().write(true).open(&path).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn selected_directory_filters_extensions_sizes_and_times_at_the_edge() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), b"alpha").unwrap();
        std::fs::write(temp.path().join("B.CSV"), b"x,y").unwrap();
        std::fs::write(temp.path().join("c.csv"), b"0123456789").unwrap();
        std::fs::create_dir(temp.path().join("nested.csv")).unwrap();
        reset_worker_incarnation();
        let directory = issue(temp.path()).unwrap();
        let output = inspect(&FileMetadataInspectParams {
            roots: vec![directory.clone()],
            max_entries: 256,
            max_bytes: 64 * 1024,
            enumerate_directories: true,
            file_extensions: vec![".csv".into()],
            min_file_bytes: Some(4),
            max_file_bytes: Some(16),
            modified_after: Some((Utc::now() - Duration::days(1)).to_rfc3339()),
            modified_before: Some((Utc::now() + Duration::days(1)).to_rfc3339()),
        })
        .unwrap();
        assert_eq!(output.directory_entries.len(), 1);
        assert_eq!(output.directory_entries[0].display_name, "c.csv");
        assert!(!output.directory_entries[0].is_directory);
        assert_eq!(output.directory_entries[0].byte_len, Some(10));
        assert!(!output.truncated);

        for (extensions, minimum, maximum, after) in [
            (vec!["csv".into()], None, None, None),
            (vec![], Some(2), Some(1), None),
            (vec![], None, None, Some("not-a-time".into())),
        ] {
            let error = inspect(&FileMetadataInspectParams {
                roots: vec![directory.clone()],
                max_entries: 256,
                max_bytes: 64 * 1024,
                enumerate_directories: true,
                file_extensions: extensions,
                min_file_bytes: minimum,
                max_file_bytes: maximum,
                modified_after: after,
                modified_before: None,
            })
            .unwrap_err();
            assert_eq!(error.kind, AgentErrorKind::InvalidInput);
        }
    }

    #[cfg(windows)]
    #[test]
    fn selected_text_read_uses_verified_handle_and_rejects_non_text_or_replacement() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("selected.txt");
        std::fs::write(&path, "hello, 世界").unwrap();
        reset_worker_incarnation();
        let selected = issue(&path).unwrap();
        let output = read_text(&FileContentReadParams {
            file: selected.clone(),
            max_bytes: 64 * 1024,
        })
        .unwrap();
        assert_eq!(output.display_name, "selected.txt");
        assert_eq!(output.content_utf8, "hello, 世界");
        assert_eq!(output.byte_len, 13);
        assert_eq!(
            output.sha256,
            format!("{:x}", Sha256::digest("hello, 世界"))
        );

        let binary_path = temp.path().join("binary.bin");
        std::fs::write(&binary_path, [0xff, 0xfe, 0x00]).unwrap();
        let binary = issue(&binary_path).unwrap();
        assert!(
            read_text(&FileContentReadParams {
                file: binary,
                max_bytes: 64 * 1024,
            })
            .is_err()
        );

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        let error = read_text(&FileContentReadParams {
            file: selected,
            max_bytes: 64 * 1024,
        })
        .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::InvalidInput);
    }

    #[cfg(windows)]
    #[test]
    fn different_allowlisted_directory_identity_is_rejected() {
        let _guard = file_store_test_lock();
        let selected = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        reset_worker_incarnation();
        let directory = issue(selected.path()).unwrap();
        let error = create_text_artifact(
            &directory,
            &[other.path().to_string_lossy().to_string()],
            "denied.txt",
            "no",
        )
        .unwrap_err();
        assert_eq!(error.kind, AgentErrorKind::PermissionDenied);
        assert!(!selected.path().join("denied.txt").exists());
        assert!(!other.path().join("denied.txt").exists());
    }
}
