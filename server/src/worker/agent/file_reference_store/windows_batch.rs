//! Handle-anchored Office source snapshots. No native host or output writes.
use super::*;
use desk_file_recovery::windows::{FileIdentity as NativeIdentity, FileKind, file_identity};

const MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
pub enum Format {
    Spreadsheet,
    Document,
    Presentation,
}

impl Format {
    fn extension(self) -> &'static str {
        match self {
            Self::Spreadsheet => "xlsx",
            Self::Document => "docx",
            Self::Presentation => "pptx",
        }
    }
}

/// Immutable bytes captured through a checked source handle.
pub struct Snapshot {
    source: ObjectRef,
    format: Format,
    identity: NativeIdentity,
    content: VerifiedFileBytes,
}

/// Keep the selected source and its ancestors fixed during native calculation
/// and publication. The source handle allows reads, but not writes or deletion.
pub struct PinnedSource {
    _file: File,
    _ancestors: Vec<File>,
}

impl Snapshot {
    pub fn pin(&self) -> Result<PinnedSource, AgentError> {
        let stored = resolve(&self.source)?;
        let (opened, ancestors, identity) =
            super::windows_path_anchor::open_anchored(&stored.path, FileKind::File)?;
        if identity != self.identity || opened.identity != stored.identity {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Office source changed before publication",
                false,
            ));
        }
        // These handles remain held while revalidation opens the same source,
        // closing the gap between comparing its digest and excluding writers.
        self.revalidate()?;
        Ok(PinnedSource {
            _file: opened.handle,
            _ancestors: ancestors,
        })
    }
    pub fn bytes(&self) -> &[u8] {
        &self.content.bytes
    }
    pub fn projection(&self) -> desk_agent_protocol::computer_use::BatchDocumentSourceProjection {
        desk_agent_protocol::computer_use::BatchDocumentSourceProjection {
            file: self.source.clone(),
            display_name: self.content.display_name.clone(),
            byte_len: self.content.bytes.len() as u64,
            sha256: self.content.sha256.clone(),
        }
    }
    pub fn revalidate(&self) -> Result<(), AgentError> {
        let current = capture(&self.source, self.format)?;
        if current.identity != self.identity
            || current.content.sha256 != self.content.sha256
            || current.content.bytes.len() != self.content.bytes.len()
        {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "Office source changed after batch observation",
                false,
            ));
        }
        Ok(())
    }
}

pub fn capture(source: &ObjectRef, format: Format) -> Result<Snapshot, AgentError> {
    if source.object_kind != ObjectKind::File {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "Office source requires a selected file",
            false,
        ));
    }
    let stored = resolve(source)?;
    if !stored
        .path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case(format.extension()))
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "Office source extension does not match the batch format",
            false,
        ));
    }
    let (mut opened, _anchors, identity) =
        super::windows_path_anchor::open_anchored(&stored.path, FileKind::File)?;
    if opened.identity != stored.identity || opened.metadata.len() > MAX_BYTES {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "Office source identity or size changed",
            false,
        ));
    }
    let mut bytes = Vec::with_capacity(opened.metadata.len() as usize);
    Read::by_ref(&mut opened.handle)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("read Office source snapshot", e))?;
    if bytes.len() as u64 != opened.metadata.len()
        || file_identity(&opened.handle, FileKind::File)
            .map_err(|e| io_error("recheck Office source identity", e))?
            != identity
    {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "Office source changed during snapshot",
            false,
        ));
    }
    Ok(Snapshot {
        source: source.clone(),
        format,
        identity,
        content: VerifiedFileBytes {
            display_name: stored
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshot_revalidation_rejects_content_and_identity_changes() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("中文 deck.pptx");
        std::fs::write(&path, b"first").unwrap();
        let source = issue(&path).unwrap();
        let snapshot = capture(&source, Format::Presentation).unwrap();
        snapshot.revalidate().unwrap();
        assert_eq!(snapshot.bytes(), b"first");
        assert_eq!(snapshot.projection().file, source);
        std::fs::write(&path, b"other").unwrap();
        assert!(snapshot.revalidate().is_err());
        assert_eq!(snapshot.bytes(), b"first");
        std::fs::rename(&path, temp.path().join("old.pptx")).unwrap();
        std::fs::write(&path, b"first").unwrap();
        assert!(snapshot.revalidate().is_err());
        assert!(capture(&source, Format::Presentation).is_err());
    }
    #[test]
    fn publication_pin_excludes_writers_and_renames_until_released() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("输入目录");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("source.xlsx");
        std::fs::write(&path, b"approved").unwrap();
        let snapshot = capture(&issue(&path).unwrap(), Format::Spreadsheet).unwrap();
        let pin = snapshot.pin().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"approved");
        assert!(OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::rename(&path, parent.join("moved.xlsx")).is_err());
        assert!(std::fs::rename(&parent, temp.path().join("moved-parent")).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        drop(pin);
        std::fs::write(&path, b"changed").unwrap();
        assert!(snapshot.pin().is_err());
        std::fs::rename(&parent, temp.path().join("moved-parent")).unwrap();
    }

    #[test]
    fn publication_pin_rejects_same_content_replacement_and_existing_writer() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.docx");
        std::fs::write(&path, b"approved").unwrap();
        let snapshot = capture(&issue(&path).unwrap(), Format::Document).unwrap();
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(snapshot.pin().is_err());
        drop(writer);
        std::fs::rename(&path, temp.path().join("old.docx")).unwrap();
        std::fs::write(&path, b"approved").unwrap();
        assert!(snapshot.pin().is_err());
    }

    #[test]
    fn snapshots_reject_wrong_formats_hardlinks_and_active_writers() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.pptx");
        std::fs::write(&path, b"snapshot").unwrap();
        let source = issue(&path).unwrap();
        assert!(capture(&source, Format::Document).is_err());
        let writer = OpenOptions::new().write(true).open(&path).unwrap();
        assert!(capture(&source, Format::Presentation).is_err());
        drop(writer);
        std::fs::hard_link(&path, temp.path().join("link.pptx")).unwrap();
        assert!(capture(&source, Format::Presentation).is_err());
        assert!(capture(&issue(temp.path()).unwrap(), Format::Presentation).is_err());
    }
}
