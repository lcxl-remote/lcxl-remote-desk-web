//! Windows artifact publication and effect-aware failure classification.
use super::*;

#[derive(Debug)]
pub enum PublishFailure {
    NotCreated(AgentError),
    OutcomeUnknown(AgentError),
}

impl PublishFailure {
    pub fn into_error(self) -> AgentError {
        match self {
            Self::NotCreated(error) | Self::OutcomeUnknown(error) => error,
        }
    }
}

pub fn create_binary_artifact(
    directory: &ObjectRef,
    file_name: &str,
    bytes: &[u8],
) -> Result<CreatedTextArtifact, AgentError> {
    publish_artifact(directory, file_name, bytes).map_err(PublishFailure::into_error)
}

pub(crate) fn publish_artifact(
    directory: &ObjectRef,
    file_name: &str,
    bytes: &[u8],
) -> Result<CreatedTextArtifact, PublishFailure> {
    publish(directory, file_name, bytes, 4 * 1024 * 1024, 200)
}

/// Call only after exact-action admission; never retry OutcomeUnknown.
pub fn publish_pptx(
    directory: &ObjectRef,
    file_name: &str,
    bytes: &[u8],
) -> Result<CreatedTextArtifact, PublishFailure> {
    if !desk_agent_protocol::computer_use::office_batch::valid_pptx_leaf(file_name) {
        return Err(PublishFailure::NotCreated(error(
            AgentErrorKind::InvalidInput,
            "PPTX output requires one safe .pptx leaf name",
            false,
        )));
    }
    publish(directory, file_name, bytes, 16 * 1024 * 1024, 255)
}

/// Call only after exact-action admission; the caller verifies DOCX semantics.
pub fn publish_docx(
    directory: &ObjectRef,
    file_name: &str,
    bytes: &[u8],
) -> Result<CreatedTextArtifact, PublishFailure> {
    if !desk_agent_protocol::computer_use::office_batch::valid_docx_leaf(file_name) {
        return Err(PublishFailure::NotCreated(error(
            AgentErrorKind::InvalidInput,
            "DOCX output requires one safe .docx leaf name",
            false,
        )));
    }
    publish(directory, file_name, bytes, 16 * 1024 * 1024, 255)
}

/// Caller owns formula calculation and content checks before create-new publication.
pub fn publish_xlsx(
    directory: &ObjectRef,
    file_name: &str,
    bytes: &[u8],
) -> Result<CreatedTextArtifact, PublishFailure> {
    if !desk_agent_protocol::computer_use::office_batch::valid_xlsx_leaf(file_name) {
        return Err(PublishFailure::NotCreated(error(
            AgentErrorKind::InvalidInput,
            "XLSX output requires one safe .xlsx leaf name",
            false,
        )));
    }
    publish(directory, file_name, bytes, 16 * 1024 * 1024, 255)
}

fn publish(
    directory: &ObjectRef,
    file_name: &str,
    bytes: &[u8],
    max_bytes: usize,
    max_name_bytes: usize,
) -> Result<CreatedTextArtifact, PublishFailure> {
    let mut created = false;
    publish_inner(
        directory,
        file_name,
        bytes,
        max_bytes,
        max_name_bytes,
        &mut created,
    )
    .map_err(|error| {
        if created {
            PublishFailure::OutcomeUnknown(error)
        } else {
            PublishFailure::NotCreated(error)
        }
    })
}

/// Create relative to the selected directory. Keep the readback handle pinned
/// until durable reference issuance has checked the reopened full file identity.
fn publish_inner(
    directory: &ObjectRef,
    file_name: &str,
    content_bytes: &[u8],
    max_bytes: usize,
    max_name_bytes: usize,
    created_effect: &mut bool,
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

    fn validate_name(name: &str, max_bytes: usize) -> anyhow::Result<()> {
        if name.is_empty()
            || name.len() > max_bytes
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
                if disposition == FILE_CREATE {
                    FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | SYNCHRONIZE
                } else {
                    FILE_GENERIC_READ | SYNCHRONIZE
                },
                &attributes,
                &mut io_status,
                None,
                FILE_ATTRIBUTE_NORMAL,
                if disposition == FILE_CREATE {
                    FILE_SHARE_READ | FILE_SHARE_DELETE
                } else {
                    FILE_SHARE_READ
                },
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
    if content_bytes.len() > max_bytes {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "artifact content exceeds the requested binary artifact ceiling",
            false,
        ));
    }
    let (selected, _ancestor_handles, _) = super::windows_path_anchor::open_anchored(
        &stored.path,
        desk_file_recovery::windows::FileKind::Directory,
    )?;
    if selected.identity != stored.identity || !selected.metadata.is_dir() {
        return Err(error(
            AgentErrorKind::InvalidInput,
            "selected directory changed after reference issuance",
            false,
        ));
    }
    let result = (|| -> anyhow::Result<CreatedTextArtifact> {
        validate_name(file_name, max_name_bytes)?;
        let parent_identity = identity(&selected.handle)?;
        let mut created = relative_file(&selected.handle, file_name, FILE_CREATE)?;
        *created_effect = true;
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
        #[cfg(test)]
        run_before_issue_hook();
        let path = stored.path.join(file_name);
        let opened = open_verified(&path).map_err(|error| anyhow!(error.message))?;
        if identity(&opened.handle)? != created_identity || !opened.metadata.is_file() {
            bail!("artifact identity changed before durable reference issuance");
        }
        let file = issue_opened_with_lifetime(&path, opened, DURABLE_ARTIFACT_REF_TTL_SECS, true)
            .map_err(|error| anyhow!(error.message))?;
        drop(verified);
        Ok(CreatedTextArtifact {
            file,
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

#[cfg(test)]
type BeforeIssueHook = Box<dyn FnOnce() + Send>;
#[cfg(test)]
fn before_issue_hook() -> &'static Mutex<Option<BeforeIssueHook>> {
    static HOOK: OnceLock<Mutex<Option<BeforeIssueHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}
#[cfg(test)]
fn run_before_issue_hook() {
    let hook = before_issue_hook().lock().unwrap().take();
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docx_publication_preserves_post_create_unknown_and_rejects_unsafe_names() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let directory = issue(temp.path()).unwrap();
        for name in [
            "../escape.docx",
            "copy.pptx",
            "NUL.docx",
            "copy.docx:stream",
        ] {
            assert!(matches!(
                publish_docx(&directory, name, b"content"),
                Err(PublishFailure::NotCreated(_))
            ));
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        let target = temp.path().join("interrupted.docx");
        set_artifact_after_close_hook(Box::new(move || {
            std::fs::write(&target, b"changed before readback").unwrap();
        }));
        assert!(matches!(
            publish_docx(&directory, "interrupted.docx", b"content"),
            Err(PublishFailure::OutcomeUnknown(_))
        ));
        assert_eq!(
            std::fs::read(temp.path().join("interrupted.docx")).unwrap(),
            b"changed before readback"
        );
        assert!(matches!(
            publish_docx(&directory, "interrupted.docx", b"content"),
            Err(PublishFailure::NotCreated(_))
        ));
    }

    #[test]
    fn verified_artifact_stays_pinned_until_reference_is_persisted() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let ancestor = temp.path().join("ancestor");
        let parent = ancestor.join("output");
        std::fs::create_dir_all(&parent).unwrap();
        let directory = issue(&parent).unwrap();
        let target = parent.join("pinned.pptx");
        let displaced = parent.join("displaced.pptx");
        let moved_parent = ancestor.join("moved-output");
        let moved_ancestor = temp.path().join("moved-ancestor");
        *before_issue_hook().lock().unwrap() = Some(Box::new(move || {
            assert!(std::fs::rename(&target, &displaced).is_err());
            assert!(std::fs::write(&target, b"replacement").is_err());
            assert!(std::fs::rename(&parent, &moved_parent).is_err());
            assert!(std::fs::rename(&ancestor, &moved_ancestor).is_err());
        }));
        let artifact = publish_pptx(&directory, "pinned.pptx", b"verified bytes").unwrap();
        let reopened = read_verified_bytes(&artifact.file, artifact.byte_len).unwrap();
        assert_eq!(reopened.bytes, b"verified bytes");
        assert_eq!(reopened.sha256, artifact.sha256);
    }

    #[test]
    fn pptx_publication_distinguishes_collision_from_post_create_failure() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let directory = issue(temp.path()).unwrap();
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/office/title-notes.pptx"
        ));
        let result = publish_pptx(&directory, "copy.pptx", bytes).unwrap();
        assert_eq!(result.byte_len, bytes.len() as u64);
        assert_eq!(std::fs::read(temp.path().join("copy.pptx")).unwrap(), bytes);
        assert!(matches!(
            publish_pptx(&directory, "copy.pptx", bytes),
            Err(PublishFailure::NotCreated(_))
        ));
        assert!(matches!(
            publish_pptx(&directory, "../escape.pptx", bytes),
            Err(PublishFailure::NotCreated(_))
        ));
        let target = temp.path().join("interrupted.pptx");
        set_artifact_after_close_hook(Box::new(move || {
            std::fs::write(&target, b"changed before readback").unwrap();
        }));
        assert!(matches!(
            publish_pptx(&directory, "interrupted.pptx", bytes),
            Err(PublishFailure::OutcomeUnknown(_))
        ));
        assert_eq!(
            std::fs::read(temp.path().join("interrupted.pptx")).unwrap(),
            b"changed before readback"
        );
    }

    #[test]
    fn pptx_names_match_approved_utf8_bounds_without_expanding_legacy_names() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let directory = issue(temp.path()).unwrap();
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/office/title-notes.pptx"
        ));
        for name in [
            format!("{}.pptx", "a".repeat(250)),
            format!("{}x.pptx", "报告 ".repeat(35)),
        ] {
            assert!(name.len() > 200 && name.len() <= 255);
            assert!(desk_agent_protocol::computer_use::office_batch::valid_pptx_leaf(&name));
            assert!(create_binary_artifact(&directory, &name, bytes).is_err());
            assert!(!temp.path().join(&name).exists());
            let artifact = publish_pptx(&directory, &name, bytes).unwrap();
            assert_eq!(artifact.file_name, name);
            assert_eq!(
                read_verified_bytes(&artifact.file, artifact.byte_len)
                    .unwrap()
                    .bytes,
                bytes
            );
            assert!(matches!(
                publish_pptx(&directory, &name, bytes),
                Err(PublishFailure::NotCreated(_))
            ));
        }
        for name in [
            format!("{}.pptx", "a".repeat(251)),
            format!("{}.pptx", "中".repeat(84)),
        ] {
            assert!(name.len() > 255);
            assert!(!desk_agent_protocol::computer_use::office_batch::valid_pptx_leaf(&name));
            assert!(matches!(
                publish_pptx(&directory, &name, bytes),
                Err(PublishFailure::NotCreated(_))
            ));
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[test]
    fn pptx_limit_does_not_expand_legacy_artifact_limit() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let directory = issue(temp.path()).unwrap();
        let bytes = vec![0; 4 * 1024 * 1024 + 1];
        assert!(create_binary_artifact(&directory, "legacy.bin", &bytes).is_err());
        assert!(!temp.path().join("legacy.bin").exists());
        assert_eq!(
            publish_pptx(&directory, "large.pptx", &bytes)
                .unwrap()
                .byte_len,
            bytes.len() as u64
        );
        assert!(matches!(
            publish_pptx(&directory, "oversized.pptx", &vec![0; 16 * 1024 * 1024 + 1]),
            Err(PublishFailure::NotCreated(_))
        ));
        assert!(!temp.path().join("oversized.pptx").exists());
    }
}
