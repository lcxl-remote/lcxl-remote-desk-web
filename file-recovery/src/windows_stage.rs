//! Replacement preparation stays in the registered private directory. No user
//! pathname changes here; authorization and durable commit intent belong to the
//! executor. A failed prepare retains the fixed leaf for identity-based cleanup.
use super::*;
use ::windows::Wdk::Storage::FileSystem::FILE_CREATE;
use std::io::Write;

pub struct StagedReplacement {
    file: File,
    identity: FileIdentity,
    source_identity: FileIdentity,
    source_parent: FileIdentity,
    source_name: String,
    source_content_sha256: String,
    source_metadata_sha256: String,
    content: Vec<u8>,
    streams: Vec<HeldStream>,
}
impl SourceSnapshot {
    /// Called only after this staged handle has been published in the authorized
    /// original parent. The executor treats every failure here as OutcomeUnknown.
    /// Applying inherited ACLs inside the private staging parent would change
    /// their meaning; keep the replacement private until this finalization step.
    pub fn finish_replacement_metadata(&self, staged: &StagedReplacement) -> io::Result<()> {
        staged.verify_for_source(self)?;
        self.revalidate_after_owned_move()?;
        security::apply_owner_group_dacl(staged.handle(), &self.security)?;
        let basic = FILE_BASIC_INFO {
            CreationTime: self.basic.created,
            LastAccessTime: self.basic.accessed,
            LastWriteTime: self.basic.modified,
            ChangeTime: 0,
            FileAttributes: self.basic.attributes,
        };
        unsafe {
            SetFileInformationByHandle(
                HANDLE(staged.handle().as_raw_handle()),
                FileBasicInfo,
                (&basic as *const FILE_BASIC_INFO).cast(),
                size_of::<FILE_BASIC_INFO>() as u32,
            )
        }
        .map_err(io::Error::other)?;
        staged.handle().sync_all()?;
        if !self
            .basic
            .same_content_metadata(&Basic::read(staged.handle())?)
        {
            return Err(crate::invalid(
                "replacement file metadata differs from the original",
            ));
        }
        staged.revalidate()?;
        self.revalidate_after_owned_move()
    }

    pub fn prepare_replacement(
        &self,
        directory: &super::super::PrivateDirectory,
        content: &[u8],
        register: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<StagedReplacement> {
        if content.len() > crate::MAX_TEXT_BYTES
            || content.contains(&0)
            || std::str::from_utf8(content).is_err()
        {
            return Err(crate::invalid("replacement requires bounded UTF-8 text"));
        }
        self.revalidate()?;
        directory.validate()?;
        if file_identity(directory.handle(), FileKind::Directory)?.volume_serial
            != self.identity.volume_serial
        {
            return Err(crate::invalid("replacement directory crossed volumes"));
        }
        let mut file = directory.create_replacement()?;
        let identity = file_identity(&file, FileKind::File)?;
        // Persist the identity before content/ADS writes so ordinary preparation
        // failures leave a known object that aborted-transaction cleanup can remove.
        // A failed registration retains the file without adopting it on a retry.
        register(&file)?;
        security::prepare_owner_group(&file, &self.security)?;
        file.write_all(content)?;
        file.sync_all()?;
        let mut streams = Vec::new();
        for original in self
            .streams
            .iter()
            .filter(|stream| stream.name != "::$DATA")
        {
            let mut stream = create_stream(directory.handle(), &original.name)?;
            if file_identity(&stream, FileKind::File)? != identity {
                return Err(crate::invalid("replacement stream identity changed"));
            }
            stream.write_all(&original.bytes)?;
            stream.sync_all()?;
            drop(stream);
            let stream = open(directory.handle(), "replacement", Some(&original.name))?;
            streams.push(HeldStream {
                name: original.name.clone(),
                file: stream,
                bytes: original.bytes.clone(),
            });
        }
        let result = StagedReplacement {
            file,
            identity,
            source_identity: self.identity(),
            source_parent: self.parent_identity(),
            source_name: self.file_name().to_owned(),
            source_content_sha256: self.content_sha256(),
            source_metadata_sha256: crate::hash(&self.metadata()?),
            content: content.to_vec(),
            streams,
        };
        result.revalidate()?;
        directory.validate()?;
        self.revalidate()?;
        Ok(result)
    }
}
impl StagedReplacement {
    /// A replacement prepared for another snapshot must never be combined with
    /// this transaction, even when filenames or ordinary text happen to match.
    pub fn verify_for_source(&self, source: &SourceSnapshot) -> io::Result<()> {
        if self.source_identity != source.identity()
            || self.source_parent != source.parent_identity()
            || self.source_name != source.file_name()
            || self.source_content_sha256 != source.content_sha256()
            || self.source_metadata_sha256 != crate::hash(&source.metadata()?)
        {
            return Err(crate::invalid(
                "replacement was prepared for another source snapshot",
            ));
        }
        self.revalidate()
    }
    pub fn handle(&self) -> &File {
        &self.file
    }
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }
    pub fn content(&self) -> &[u8] {
        &self.content
    }
    pub fn content_sha256(&self) -> String {
        crate::hash(&self.content)
    }
    pub fn revalidate(&self) -> io::Result<()> {
        if file_identity(&self.file, FileKind::File)? != self.identity
            || read(&self.file, crate::MAX_TEXT_BYTES)? != self.content
        {
            return Err(crate::invalid("replacement content changed"));
        }
        let before = stream_inventory(&self.file, SourceSnapshot::byte_limit())?;
        let mut expected: Vec<_> = self
            .streams
            .iter()
            .map(|stream| super::super::StreamInfo {
                name: stream.name.clone(),
                bytes: stream.bytes.len() as u64,
            })
            .collect();
        expected.push(super::super::StreamInfo {
            name: "::$DATA".into(),
            bytes: self.content.len() as u64,
        });
        expected.sort_by(|a, b| a.name.cmp(&b.name));
        if before.streams != expected {
            return Err(crate::invalid("replacement stream set changed"));
        }
        for stream in &self.streams {
            if file_identity(&stream.file, FileKind::File)? != self.identity
                || read(&stream.file, stream.bytes.len())? != stream.bytes
            {
                return Err(crate::invalid("replacement stream changed"));
            }
        }
        if stream_inventory(&self.file, SourceSnapshot::byte_limit())? != before {
            return Err(crate::invalid("replacement changed during observation"));
        }
        Ok(())
    }
}

fn create_stream(parent: &File, stream: &str) -> io::Result<File> {
    let component = stream
        .strip_prefix(':')
        .and_then(|s| s.strip_suffix(":$DATA"))
        .ok_or_else(|| crate::invalid("invalid replacement stream type"))?;
    private::leaf_name(component)?;
    let mut name: Vec<u16> = format!("replacement{stream}").encode_utf16().collect();
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
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | SYNCHRONIZE,
            &attrs,
            &mut status,
            None,
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_DELETE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            None,
            0,
        )
    };
    if result.0 != 0 {
        return Err(io::Error::other(format!(
            "replacement stream create NTSTATUS {:#x}",
            result.0
        )));
    }
    Ok(unsafe { File::from_raw_handle(handle.0) })
}
