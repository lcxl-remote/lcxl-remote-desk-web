//! Bounded source evidence held through the transaction. Known streams remain
//! read-locked; a stream inventory is not a whole-file namespace lock. Executors
//! must revalidate after moving the original into the registered private directory
//! and before publishing its replacement. This module never changes user files.
use super::{FileIdentity, FileKind, file_identity, private, security, stream_inventory};
use ::windows::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
            FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
        },
    },
    Win32::{
        Foundation::{HANDLE, OBJ_CASE_INSENSITIVE, UNICODE_STRING},
        Storage::FileSystem::*,
        System::IO::IO_STATUS_BLOCK,
    },
    core::PWSTR,
};
use serde::Serialize;
#[path = "windows_stage.rs"]
mod stage;
pub use stage::StagedReplacement;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Basic {
    created: i64,
    accessed: i64,
    modified: i64,
    changed: i64,
    attributes: u32,
}
impl Basic {
    fn read(file: &File) -> io::Result<Self> {
        let mut info = FILE_BASIC_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                HANDLE(file.as_raw_handle()),
                FileBasicInfo,
                (&mut info as *mut FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        }
        .map_err(io::Error::other)?;
        // These storage semantics require dedicated preservation implementations.
        if info.FileAttributes
            & (FILE_ATTRIBUTE_ENCRYPTED
                | FILE_ATTRIBUTE_SPARSE_FILE
                | FILE_ATTRIBUTE_COMPRESSED
                | FILE_ATTRIBUTE_OFFLINE)
                .0
            != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported recovery file storage attributes",
            ));
        }
        Ok(Self {
            created: info.CreationTime,
            accessed: info.LastAccessTime,
            modified: info.LastWriteTime,
            changed: info.ChangeTime,
            attributes: info.FileAttributes,
        })
    }
    fn same_content_metadata(&self, other: &Self) -> bool {
        // Reading may update last-access time. The caller separately compares
        // ChangeTime except immediately after its own confirmed rename.
        self.created == other.created
            && self.modified == other.modified
            && self.attributes == other.attributes
    }
}

struct HeldStream {
    name: String,
    file: File,
    bytes: Vec<u8>,
}

pub struct SourceSnapshot {
    source: File,
    identity: FileIdentity,
    parent_identity: FileIdentity,
    leaf: String,
    basic: Basic,
    security: String,
    streams: Vec<HeldStream>,
}

impl SourceSnapshot {
    /// `parent` must be authorized and pinned by the caller's containment guard;
    /// `expected` comes from its authenticated selected-file reference.
    pub fn capture(parent: &File, leaf: &str, expected: FileIdentity) -> io::Result<Self> {
        let parent_identity = file_identity(parent, FileKind::Directory)?;
        if parent_identity.volume_serial != expected.volume_serial {
            return Err(crate::invalid("source parent volume changed"));
        }
        let source = open(parent, leaf, None)?;
        if file_identity(&source, FileKind::File)? != expected {
            return Err(crate::invalid("selected source identity changed"));
        }
        let basic = Basic::read(&source)?;
        let security = security::capture_owner_group_dacl(&source)?;
        let inventory = stream_inventory(&source, Self::byte_limit())?;
        let mut streams = Vec::with_capacity(inventory.streams.len());
        for stream in inventory.streams {
            let file = if stream.name == "::$DATA" {
                source.try_clone()?
            } else {
                open(parent, leaf, Some(&stream.name))?
            };
            if file_identity(&file, FileKind::File)? != expected {
                return Err(crate::invalid("source stream identity changed"));
            }
            let bound = if stream.name == "::$DATA" {
                crate::MAX_TEXT_BYTES
            } else {
                crate::MAX_METADATA_BYTES / 2
            };
            let bytes = read(&file, bound)?;
            if bytes.len() as u64 != stream.bytes {
                return Err(crate::invalid("source stream length changed"));
            }
            streams.push(HeldStream {
                name: stream.name,
                file,
                bytes,
            });
        }
        let snapshot = Self {
            source,
            identity: expected,
            parent_identity,
            leaf: leaf.to_owned(),
            basic,
            security,
            streams,
        };
        let content = snapshot.content();
        if content.contains(&0) || std::str::from_utf8(content).is_err() {
            return Err(crate::invalid("recovery requires complete UTF-8 text"));
        }
        snapshot.revalidate()?;
        // Enforce the actual serialized metadata budget before a backup is begun.
        snapshot.metadata()?;
        Ok(snapshot)
    }
    fn byte_limit() -> u64 {
        (crate::MAX_TEXT_BYTES + crate::MAX_METADATA_BYTES / 2) as u64
    }
    pub fn handle(&self) -> &File {
        &self.source
    }
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }
    pub fn parent_identity(&self) -> FileIdentity {
        self.parent_identity
    }
    pub fn file_name(&self) -> &str {
        &self.leaf
    }
    pub fn content(&self) -> &[u8] {
        &self
            .streams
            .iter()
            .find(|stream| stream.name == "::$DATA")
            .expect("validated inventory contains the unnamed data stream")
            .bytes
    }
    pub fn content_sha256(&self) -> String {
        crate::hash(self.content())
    }
    pub fn metadata(&self) -> io::Result<Vec<u8>> {
        let streams: BTreeMap<_, _> = self
            .streams
            .iter()
            .filter(|stream| stream.name != "::$DATA")
            .map(|stream| {
                (
                    stream.name.as_str(),
                    stream
                        .bytes
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>(),
                )
            })
            .collect();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "platform": "windows", "format_version": 1,
            "volume_serial": self.identity.volume_serial, "file_id": self.identity.file_id,
            "basic": self.basic, "owner_group_dacl_sddl": self.security,
            "alternate_streams_hex": streams,
        }))
        .map_err(io::Error::other)?;
        if bytes.len() > crate::MAX_METADATA_BYTES {
            return Err(crate::invalid("recovery metadata exceeds bounds"));
        }
        Ok(bytes)
    }
    /// Re-read content and metadata, not just length or identity. This works on
    /// the held original after a handle-relative rename without trusting a path.
    pub fn revalidate(&self) -> io::Result<()> {
        self.revalidate_inner(false)
    }
    /// Only the transaction executor may use this after its own confirmed
    /// handle-relative rename into the registered private directory. Identity,
    /// bytes, stream set, permissions and other metadata must still match.
    pub fn revalidate_after_owned_move(&self) -> io::Result<()> {
        self.revalidate_inner(true)
    }
    fn revalidate_inner(&self, moved: bool) -> io::Result<()> {
        let basic = Basic::read(&self.source)?;
        if file_identity(&self.source, FileKind::File)? != self.identity
            || !self.basic.same_content_metadata(&basic)
            || (!moved && basic.changed != self.basic.changed)
            || security::capture_owner_group_dacl(&self.source)? != self.security
        {
            return Err(crate::invalid("source metadata changed"));
        }
        let before = stream_inventory(&self.source, Self::byte_limit())?;
        if before.streams.len() != self.streams.len() {
            return Err(crate::invalid("source stream set changed"));
        }
        for (observed, held) in before.streams.iter().zip(&self.streams) {
            if observed.name != held.name
                || observed.bytes != held.bytes.len() as u64
                || file_identity(&held.file, FileKind::File)? != self.identity
                || read(&held.file, held.bytes.len())? != held.bytes
            {
                return Err(crate::invalid("source stream content changed"));
            }
        }
        let after = Basic::read(&self.source)?;
        if stream_inventory(&self.source, Self::byte_limit())? != before
            || !self.basic.same_content_metadata(&after)
            || after.changed != basic.changed
            || security::capture_owner_group_dacl(&self.source)? != self.security
        {
            return Err(crate::invalid("source changed during observation"));
        }
        Ok(())
    }
}

fn read(file: &File, bound: usize) -> io::Result<Vec<u8>> {
    if file.metadata()?.len() > bound as u64 {
        return Err(crate::invalid("source stream exceeds bounds"));
    }
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(bound as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > bound {
        return Err(crate::invalid("source stream exceeds bounds"));
    }
    Ok(bytes)
}

fn open(parent: &File, leaf: &str, stream: Option<&str>) -> io::Result<File> {
    let mut name = private::leaf_name(leaf)?;
    if let Some(stream) = stream {
        let component = stream
            .strip_prefix(':')
            .and_then(|s| s.strip_suffix(":$DATA"))
            .ok_or_else(|| crate::invalid("invalid source stream type"))?;
        // Use the same bounded leaf grammar; append only a validated NTFS stream.
        private::leaf_name(component)?;
        name.extend(stream.encode_utf16());
    }
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
        ..Default::default()
    };
    let mut handle = HANDLE::default();
    let mut status = IO_STATUS_BLOCK::default();
    let access = FILE_GENERIC_READ
        | if stream.is_none() {
            DELETE
        } else {
            FILE_ACCESS_RIGHTS(0)
        };
    let share = FILE_SHARE_READ
        | if stream.is_some() {
            FILE_SHARE_DELETE
        } else {
            FILE_SHARE_MODE(0)
        };
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attrs,
            &mut status,
            None,
            FILE_ATTRIBUTE_NORMAL,
            share,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    };
    if result.0 != 0 {
        return Err(io::Error::other(format!(
            "source open NTSTATUS {:#x}",
            result.0
        )));
    }
    Ok(unsafe { File::from_raw_handle(handle.0) })
}
