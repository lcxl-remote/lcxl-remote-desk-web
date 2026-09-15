//! Preserve publication outcome through the dispatcher instead of flattening
//! a possibly-created file into an ordinary retryable failure message.
use super::*;
use desk_agent_protocol::computer_use::{
    ComputerActionOutput, ComputerActionResultClass, ComputerActionStepFact,
    CreatedFileArtifactOutput,
};

#[derive(Debug)]
pub(crate) struct Failure {
    pub class: ComputerActionResultClass,
    pub message: String,
}

pub(crate) type Receipt = (
    ComputerActionResultClass,
    Vec<ComputerActionStepFact>,
    Option<String>,
    Option<ComputerActionOutput>,
);

pub(crate) fn receipt(result: Result<(CreatedTextArtifact, &str), Failure>) -> Receipt {
    let (artifact, media_type) = match result {
        Ok(value) => value,
        Err(failure) => return (failure.class, vec![], Some(failure.message), None),
    };
    let output = CreatedFileArtifactOutput {
        file: artifact.file.clone(),
        file_name: artifact.file_name.clone(),
        media_type: media_type.into(),
        size_bytes: artifact.byte_len,
        digest_sha256: artifact.sha256.clone(),
        content: desk_agent_protocol::data_lineage::ContentRef::Artifact {
            artifact_id: artifact.file.token.clone(),
            sha256: artifact.sha256.clone(),
            size_bytes: artifact.byte_len,
            media_type: media_type.into(),
        },
    };
    if output.validate().is_err() {
        // Publication already returned a created file. A malformed projection
        // must not panic and lose the receipt or imply that retrying is safe.
        return (
            ComputerActionResultClass::OutcomeUnknown,
            vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: false,
                summary: "file created but its artifact receipt could not be validated".into(),
            }],
            Some("Artifact receipt is invalid; check the output directory before retrying".into()),
            None,
        );
    }
    (
        ComputerActionResultClass::Verified,
        vec![ComputerActionStepFact {
            index: 0, changed: true, verified: true,
            summary: format!("created {} ({} bytes, sha256={})", artifact.file_name, artifact.byte_len, artifact.sha256),
        }],
        Some("artifact created with create-new semantics and verified by independent handle read-back".into()),
        Some(ComputerActionOutput::FileArtifact(output)),
    )
}
impl Failure {
    pub fn unknown(message: String) -> Self {
        Self {
            class: ComputerActionResultClass::OutcomeUnknown,
            message,
        }
    }
}
impl From<AgentError> for Failure {
    fn from(error: AgentError) -> Self {
        Self {
            class: ComputerActionResultClass::Failed,
            message: error.message,
        }
    }
}
impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self {
            class: ComputerActionResultClass::Failed,
            message,
        }
    }
}

pub(crate) fn create_binary_artifact(
    directory: &ObjectRef,
    name: &str,
    bytes: &[u8],
) -> Result<CreatedTextArtifact, Failure> {
    #[cfg(windows)]
    {
        super::windows_publish::publish_artifact(directory, name, bytes).map_err(|failure| {
            match failure {
                super::windows_publish::PublishFailure::NotCreated(error) => error.into(),
                super::windows_publish::PublishFailure::OutcomeUnknown(error) => {
                    Failure::unknown(error.message)
                }
            }
        })
    }
    #[cfg(target_os = "macos")]
    {
        let mut published = false;
        super::macos_publish::publish(directory, name, bytes, &mut published).map_err(|error| {
            if published {
                Failure::unknown(error.message)
            } else {
                error.into()
            }
        })
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    super::create_binary_artifact(directory, name, bytes).map_err(Failure::from)
}

pub(crate) fn create_text_artifact(
    directory: &ObjectRef,
    name: &str,
    text: &str,
) -> Result<CreatedTextArtifact, Failure> {
    if text.len() > 64 * 1024 {
        return Err(error(
            AgentErrorKind::OutputLimitExceeded,
            "artifact content exceeds the 64 KiB text artifact ceiling",
            false,
        )
        .into());
    }
    create_binary_artifact(directory, name, text.as_bytes())
}

#[cfg(all(test, any(windows, target_os = "macos")))]
mod tests {
    use super::*;

    #[test]
    fn ordinary_artifact_preserves_collision_and_post_create_outcomes() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let directory = issue(temp.path()).unwrap();
        let created = create_text_artifact(&directory, "copy.txt", "approved").unwrap();
        assert_eq!(
            receipt(Ok((created, "text/plain;charset=utf-8"))).0,
            ComputerActionResultClass::Verified
        );
        let collision = create_text_artifact(&directory, "copy.txt", "replacement");
        assert_eq!(
            receipt(collision.map(|value| (value, "text/plain"))).0,
            ComputerActionResultClass::Failed
        );
        assert_eq!(
            std::fs::read(temp.path().join("copy.txt")).unwrap(),
            b"approved"
        );

        let target = temp.path().join("interrupted.txt");
        set_artifact_after_close_hook(Box::new(move || {
            std::fs::write(target, b"changed before readback").unwrap();
        }));
        let interrupted = create_text_artifact(&directory, "interrupted.txt", "approved");
        let result = receipt(interrupted.map(|value| (value, "text/plain")));
        assert_eq!(result.0, ComputerActionResultClass::OutcomeUnknown);
        assert!(result.3.is_none());
        assert_eq!(
            std::fs::read(temp.path().join("interrupted.txt")).unwrap(),
            b"changed before readback"
        );
    }

    #[test]
    fn invalid_created_receipt_returns_unknown_without_panicking() {
        let _guard = file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let directory = issue(temp.path()).unwrap();
        let mut artifact = create_text_artifact(&directory, "copy.txt", "approved").unwrap();
        artifact.sha256 = "invalid digest".into();
        let result = receipt(Ok((artifact, "text/plain")));
        assert_eq!(result.0, ComputerActionResultClass::OutcomeUnknown);
        assert_eq!(result.1.len(), 1);
        assert!(result.1[0].changed);
        assert!(!result.1[0].verified);
        assert!(result.3.is_none());
        assert_eq!(
            std::fs::read(temp.path().join("copy.txt")).unwrap(),
            b"approved"
        );
    }
}
