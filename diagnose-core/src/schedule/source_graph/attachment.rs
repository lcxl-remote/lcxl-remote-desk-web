//! Bind a current-run immutable attachment receipt to its complete source graph.
use super::*;
use desk_agent_protocol::communication::ImmutableAttachmentSnapshot;

/// Supplied only after verifying the original owner/device-bound artifact receipt.
/// This is an internal evidence input, not a model-facing deserialization contract.
pub struct TaskAttachmentReceipt<'a> {
    pub run_id: &'a str,
    pub envelope_id: &'a str,
    /// Digest of the authenticated tool receipt envelope, not of the attachment bytes.
    pub receipt_digest_sha256: &'a str,
    pub attachment: &'a ImmutableAttachmentSnapshot,
}

/// The caller verifies artifact ownership and receipt authenticity before calling.
/// Exact snapshot equality includes content identity, filename, type, length and
/// digest. Scope resolution follows every parent, including derived artifacts.
/// No grant is issued here; destination and attachment budgets remain separate.
pub fn resolve_task_attachment_sources(
    run_id: &str,
    attachment: &ImmutableAttachmentSnapshot,
    receipt: &TaskAttachmentReceipt<'_>,
    nodes: &[ModelInputLineage],
    bindings: &[TaskSourceBinding],
) -> Result<ResolvedTaskSources, TaskSourceError> {
    if !valid_id(run_id)
        || receipt.run_id != run_id
        || receipt.attachment != attachment
        || attachment.validate().is_err()
    {
        return Err(TaskSourceError::InvalidRoot);
    }
    resolve_task_sources(
        receipt.envelope_id,
        receipt.receipt_digest_sha256,
        nodes,
        bindings,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::data_lineage::ContentRef;

    fn attachment() -> ImmutableAttachmentSnapshot {
        ImmutableAttachmentSnapshot {
            content: ContentRef::Artifact {
                artifact_id: "artifact-1".into(),
                sha256: "a".repeat(64),
                size_bytes: 4,
                media_type: "text/plain".into(),
            },
            file_name: "report.txt".into(),
            media_type: "text/plain".into(),
            size_bytes: 4,
            digest_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn rejects_previous_run_and_changed_filename() {
        let original = attachment();
        let receipt = TaskAttachmentReceipt {
            run_id: "run-1",
            envelope_id: "artifact-envelope",
            receipt_digest_sha256: &"b".repeat(64),
            attachment: &original,
        };
        assert_eq!(
            resolve_task_attachment_sources("run-2", &original, &receipt, &[], &[]),
            Err(TaskSourceError::InvalidRoot)
        );
        let mut changed = original.clone();
        changed.file_name = "different.txt".into();
        assert_eq!(
            resolve_task_attachment_sources("run-1", &changed, &receipt, &[], &[]),
            Err(TaskSourceError::InvalidRoot)
        );
    }

    #[test]
    fn traverses_artifact_sources_and_rejects_missing_parent() {
        let original = attachment();
        let receipt_digest = "b".repeat(64);
        let receipt = TaskAttachmentReceipt {
            run_id: "run",
            envelope_id: "artifact",
            receipt_digest_sha256: &receipt_digest,
            attachment: &original,
        };
        let node = |id: &str, parents: Vec<String>| ModelInputLineage {
            public_system_prompt: false,
            envelope_id: id.into(),
            digest_sha256: "b".repeat(64),
            source_provider_id: "device".into(),
            source_tool_name: "read".into(),
            source_envelope_ids: parents,
        };
        let nodes = vec![
            node("artifact", vec!["source".into()]),
            node("source", vec![]),
        ];
        let roots = vec![TaskSourceBinding {
            envelope_id: "source".into(),
            digest_sha256: "b".repeat(64),
            authority: TaskSourceAuthority::Scopes(vec!["file:report".into()]),
        }];
        let resolved =
            resolve_task_attachment_sources("run", &original, &receipt, &nodes, &roots).unwrap();
        assert_eq!(resolved.scopes, vec!["file:report"]);
        assert_eq!(
            resolve_task_attachment_sources("run", &original, &receipt, &nodes[..1], &[]),
            Err(TaskSourceError::MissingSource)
        );
    }
}

/// Match only artifacts projected from verified actions in the caller's frozen run.
/// Matching by filename or digest alone is insufficient; all immutable fields agree.
pub fn unique_attachment_artifact<'a>(
    requested: &ImmutableAttachmentSnapshot,
    artifacts: &'a [desk_agent_protocol::computer_use::CreatedFileArtifactOutput],
) -> Result<&'a desk_agent_protocol::computer_use::CreatedFileArtifactOutput, TaskSourceError> {
    requested
        .validate()
        .map_err(|_| TaskSourceError::InvalidNode)?;
    let mut matched = None;
    for artifact in artifacts {
        artifact
            .validate()
            .map_err(|_| TaskSourceError::InvalidRoot)?;
        if artifact.content == requested.content
            && artifact.file_name == requested.file_name
            && artifact.media_type == requested.media_type
            && artifact.size_bytes == requested.size_bytes
            && artifact.digest_sha256 == requested.digest_sha256
        {
            if matched.is_some() {
                return Err(TaskSourceError::ConflictingNode);
            }
            matched = Some(artifact);
        }
    }
    matched.ok_or(TaskSourceError::MissingSource)
}

#[cfg(test)]
mod artifact_matching_tests {
    use super::*;
    use desk_agent_protocol::{
        computer_use::{CreatedFileArtifactOutput, ObjectKind, ObjectRef},
        data_lineage::ContentRef,
    };

    #[test]
    fn exact_artifact_required_and_duplicate_receipts_are_ambiguous() {
        let content = ContentRef::Artifact {
            artifact_id: "artifact".into(),
            sha256: "a".repeat(64),
            size_bytes: 4,
            media_type: "text/plain".into(),
        };
        let artifact = CreatedFileArtifactOutput {
            file: ObjectRef {
                token: "artifact".into(),
                snapshot_id: "snapshot".into(),
                object_kind: ObjectKind::File,
                expires_at: "2099-01-01T00:00:00Z".into(),
            },
            file_name: "report.txt".into(),
            media_type: "text/plain".into(),
            size_bytes: 4,
            digest_sha256: "a".repeat(64),
            content: content.clone(),
        };
        let mut requested = ImmutableAttachmentSnapshot {
            content,
            file_name: artifact.file_name.clone(),
            media_type: artifact.media_type.clone(),
            size_bytes: 4,
            digest_sha256: artifact.digest_sha256.clone(),
        };
        assert!(unique_attachment_artifact(&requested, std::slice::from_ref(&artifact)).is_ok());
        assert_eq!(
            unique_attachment_artifact(&requested, &[artifact.clone(), artifact.clone()]),
            Err(TaskSourceError::ConflictingNode)
        );
        let mut text_artifact = artifact.clone();
        text_artifact.media_type = crate::provider_preflight::TEXT_ARTIFACT_MEDIA_TYPE.into();
        text_artifact.digest_sha256 = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(b"test"))
        };
        text_artifact.content = ContentRef::Artifact {
            artifact_id: text_artifact.file.token.clone(),
            sha256: text_artifact.digest_sha256.clone(),
            size_bytes: text_artifact.size_bytes,
            media_type: text_artifact.media_type.clone(),
        };
        let input = r#"{"file_name":"report.txt","content_utf8":"test"}"#;
        assert!(
            verify_text_artifact_output(
                "create_text_artifact_in_selected_directory",
                input,
                &text_artifact
            )
            .is_ok()
        );
        text_artifact.digest_sha256 = "b".repeat(64);
        assert!(
            verify_text_artifact_output(
                "create_text_artifact_in_selected_directory",
                input,
                &text_artifact
            )
            .is_err()
        );
        requested.file_name = "renamed.txt".into();
        assert_eq!(
            unique_attachment_artifact(&requested, &[artifact]),
            Err(TaskSourceError::MissingSource)
        );
    }
}

/// Verify a text artifact against the original, store-authenticated create call.
/// Other artifact formats require their own native generation evidence.
pub fn verify_text_artifact_output(
    tool_name: &str,
    canonical_input: &str,
    artifact: &desk_agent_protocol::computer_use::CreatedFileArtifactOutput,
) -> Result<(), super::TaskSourceError> {
    use sha2::{Digest, Sha256};
    artifact
        .validate()
        .map_err(|_| super::TaskSourceError::InvalidNode)?;
    if tool_name != "create_text_artifact_in_selected_directory" {
        return Ok(());
    }
    let call = crate::chat::ToolCall {
        id: "artifact-evidence".into(),
        name: tool_name.into(),
        arguments_json: canonical_input.into(),
    };
    let action = crate::provider_preflight::artifact_action_from_call(&call)
        .map_err(|_| super::TaskSourceError::InvalidNode)?;
    let desk_agent_protocol::computer_use::FilePatchAction::CreateTextArtifact {
        file_name,
        content_utf8,
    } = action
    else {
        return Err(super::TaskSourceError::InvalidNode);
    };
    if artifact.file_name != file_name
        || artifact.size_bytes != content_utf8.len() as u64
        || artifact.media_type != crate::provider_preflight::TEXT_ARTIFACT_MEDIA_TYPE
        || artifact.digest_sha256 != format!("{:x}", Sha256::digest(content_utf8.as_bytes()))
    {
        return Err(super::TaskSourceError::ConflictingNode);
    }
    Ok(())
}
