//! Bind selected file authority to a semantic target returned by the same read.
//! Hosts must supply authenticated edge output, never model-supplied JSON.
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{
        BatchDocumentSourceProjection, ComputerUseAdapterKind, ComputerUseAdapterRef,
        LiveDocumentInspectOutput, LiveDocumentProjection, ObjectKind, ObjectRef, office_batch,
    },
};

pub struct PresentationReadBinding {
    source: BatchDocumentSourceProjection,
    target: ObjectRef,
    adapter: ComputerUseAdapterRef,
    valid_until: u64,
}

pub use desk_agent_protocol::computer_use::office_batch::PPTX_MEDIA_TYPE;

/// File-only batch receipts carry the actual artifact, never a synthetic native
/// validation export. Correlation and dispatch admission remain host checks.
pub fn validate_pptx_completion(
    adapter: &ComputerUseAdapterRef,
    action: &desk_agent_protocol::computer_use::ComputerActionKind,
    completed: &desk_agent_protocol::computer_use::ComputerActionCompleted,
    now: u64,
) -> Result<(), AgentError> {
    if !office_batch::is_pptx(adapter) {
        return Err(denied());
    }
    super::office_file_completion::validate(adapter, action, completed, now)
}
impl PresentationReadBinding {
    /// Original selection and incarnation must come from the owning run/device.
    /// This identity proof does not grant access, persist a receipt, or seal a plan.
    pub fn from_authenticated_read(
        selected_file: &ObjectRef,
        worker_incarnation: &str,
        output: &LiveDocumentInspectOutput,
        now: u64,
    ) -> Result<Self, AgentError> {
        let source = output.batch_source.as_ref().ok_or_else(denied)?;
        if selected_file.object_kind != ObjectKind::File
            || &source.file != selected_file
            || source.sha256.len() != 64
            || !source.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || source.byte_len == 0
            || source.byte_len > 128 * 1024 * 1024
            || worker_incarnation.is_empty()
            || worker_incarnation.len() > 4096
        {
            return Err(denied());
        }
        let supported = office_batch::is_pptx(&output.adapter)
            || (output.adapter.kind == ComputerUseAdapterKind::IworkKeynote
                && output.adapter.version == crate::device_assistant::IWORK_ADAPTER_VERSION);
        if !supported
            || (office_batch::is_pptx(&output.adapter) && source.byte_len > 16 * 1024 * 1024)
        {
            return Err(denied());
        }
        let LiveDocumentProjection::Presentation {
            presentation,
            slide,
            slide_number,
            ..
        } = &output.projection
        else {
            return Err(denied());
        };
        if *slide_number < 1
            || presentation.object_kind != ObjectKind::Presentation
            || slide.object_kind != ObjectKind::Slide
            || presentation.token == slide.token
            || presentation.snapshot_id != output.snapshot_id
            || slide.snapshot_id != output.snapshot_id
            || !output
                .snapshot_id
                .strip_prefix(worker_incarnation)
                .and_then(|s| s.strip_prefix(':'))
                .and_then(|s| s.parse::<u64>().ok())
                .is_some_and(|n| n > 0)
        {
            return Err(denied());
        }
        let valid_until = [selected_file, presentation, slide]
            .into_iter()
            .map(|reference| expiry(reference, now))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .min()
            .ok_or_else(denied)?;
        Ok(Self {
            source: source.clone(),
            target: slide.clone(),
            adapter: output.adapter.clone(),
            valid_until,
        })
    }

    pub fn source(&self) -> &BatchDocumentSourceProjection {
        &self.source
    }
    pub fn adapter(&self) -> &ComputerUseAdapterRef {
        &self.adapter
    }
    pub fn valid_until_unix_ms(&self) -> u64 {
        self.valid_until
    }
    pub fn validate_target(&self, target: &ObjectRef, now: u64) -> Result<(), AgentError> {
        if target != &self.target || now == 0 || now >= self.valid_until {
            return Err(denied());
        }
        Ok(())
    }
}

fn expiry(reference: &ObjectRef, now: u64) -> Result<u64, AgentError> {
    if now == 0
        || reference.token.is_empty()
        || reference.token.len() > 4096
        || reference.snapshot_id.is_empty()
        || reference.snapshot_id.len() > 4096
    {
        return Err(denied());
    }
    chrono::DateTime::parse_from_rfc3339(&reference.expires_at)
        .ok()
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
        .filter(|expiry| *expiry > now)
        .ok_or_else(denied)
}
fn denied() -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "batch presentation read is not bound to the original file and current worker"
            .into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

/// Resolve evidence only from the authoritative session's recorded tool result.
/// The caller supplies the selected file and current worker from trusted state.
pub fn resolve_presentation_read(
    session: &crate::session::PersistedAgentSession,
    selected_file: &ObjectRef,
    worker_incarnation: &str,
    target: &ObjectRef,
    now: u64,
) -> Result<PresentationReadBinding, AgentError> {
    use crate::chat::ChatRole;
    use sha2::{Digest, Sha256};
    let registry = crate::device_assistant::device_assistant_provider_registry();
    let mut selected = None;
    for message in &session.conversation {
        if !matches!(message.role, ChatRole::Tool | ChatRole::UntrustedOutput) {
            continue;
        }
        let Some(call_id) = message.tool_call_id.as_deref() else {
            continue;
        };
        let mut calls = session
            .conversation
            .iter()
            .filter(|entry| entry.role == ChatRole::Assistant)
            .flat_map(|entry| &entry.tool_calls)
            .filter(|call| call.id == call_id);
        let Some(call) = calls.next() else { continue };
        if calls.next().is_some()
            || !matches!(
                call.name.as_str(),
                "inspect_keynote_file" | "inspect_powerpoint_file"
            )
        {
            continue;
        }
        let Some(capability) = registry.capability_for_tool(&call.name) else {
            continue;
        };
        let Some(provider) = registry.provider_for_capability(&capability.wire.capability_id)
        else {
            continue;
        };
        let Some(envelope) = &message.data_envelope else {
            continue;
        };
        if envelope.validate().is_err()
            || crate::model_egress::envelope_expires_by(envelope, now)
            || envelope.provenance.source_tool_name != call.name
            || envelope.provenance.source_provider_id != provider.wire.provider_id
            || envelope.digest_sha256 != format!("{:x}", Sha256::digest(message.text.as_bytes()))
        {
            continue;
        }
        let Ok(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::PresentationLiveInspect(output),
        )) = serde_json::from_str(&message.text)
        else {
            continue;
        };
        let expected_adapter = if call.name == "inspect_keynote_file" {
            ComputerUseAdapterKind::IworkKeynote
        } else {
            ComputerUseAdapterKind::OfficePowerPoint
        };
        if output.adapter.kind != expected_adapter {
            continue;
        }
        let Ok(binding) = PresentationReadBinding::from_authenticated_read(
            selected_file,
            worker_incarnation,
            &output,
            now,
        ) else {
            continue;
        };
        if binding.validate_target(target, now).is_err() {
            continue;
        }
        // More than one result for a target is ambiguous, even if text matches.
        if selected.is_some() {
            return Err(denied());
        }
        selected = Some(binding);
    }
    selected.ok_or_else(denied)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pptx_receipt_requires_exact_file_artifact_and_preserves_unknown() {
        use desk_agent_protocol::computer_use::*;
        use desk_agent_protocol::data_lineage::ContentRef;
        let adapter = output().adapter;
        let action = ComputerActionKind::PresentationLiveBatch(PresentationLiveBatchPatchAction {
            output: BatchDocumentOutput {
                destination_parent: object(ObjectKind::Directory, "directory"),
                native_file_name: "copy.pptx".into(),
            },
            action: PresentationLivePatchAction::ReplaceSlideTitle {
                text: "title".into(),
            },
        });
        let artifact = CreatedFileArtifactOutput {
            file: object(ObjectKind::File, "artifact"),
            file_name: "copy.pptx".into(),
            media_type: PPTX_MEDIA_TYPE.into(),
            size_bytes: 100,
            digest_sha256: "a".repeat(64),
            content: ContentRef::Artifact {
                artifact_id: "artifact".into(),
                sha256: "a".repeat(64),
                size_bytes: 100,
                media_type: PPTX_MEDIA_TYPE.into(),
            },
        };
        let completed = ComputerActionCompleted {
            work_id: "work".into(),
            action_request_id: "call".into(),
            execution_generation: "generation".into(),
            result: ComputerActionResultClass::Verified,
            facts: vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: true,
                summary: "copy created".into(),
            }],
            message: None,
            output: Some(ComputerActionOutput::FileArtifact(artifact)),
        };
        validate_pptx_completion(&adapter, &action, &completed, 1000).unwrap();
        for case in 0..5 {
            let mut bad = completed.clone();
            let Some(ComputerActionOutput::FileArtifact(artifact)) = &mut bad.output else {
                panic!()
            };
            match case {
                0 => artifact.file_name = "other.pptx".into(),
                1 => artifact.media_type = "application/pdf".into(),
                2 => artifact.file.expires_at = "1970-01-01T00:00:00Z".into(),
                3 => bad.facts[0].changed = false,
                4 => bad.result = ComputerActionResultClass::OutcomeUnknown,
                _ => unreachable!(),
            }
            assert!(validate_pptx_completion(&adapter, &action, &bad, 1000).is_err());
        }
        let mut unknown = completed;
        unknown.result = ComputerActionResultClass::OutcomeUnknown;
        unknown.output = None;
        unknown.facts[0].verified = false;
        validate_pptx_completion(&adapter, &action, &unknown, 1000).unwrap();
        unknown.result = ComputerActionResultClass::DefinitelyNotStarted;
        assert!(validate_pptx_completion(&adapter, &action, &unknown, 1000).is_err());
    }
    #[test]
    fn session_resolution_requires_unique_authenticated_read_evidence() {
        use crate::chat::{ChatMessage, ChatRole, ToolCallRef};
        use desk_agent_protocol::data_lineage::*;
        use sha2::{Digest, Sha256};
        let output = output();
        let file = output.batch_source.as_ref().unwrap().file.clone();
        let LiveDocumentProjection::Presentation { ref slide, .. } = output.projection else {
            panic!()
        };
        let target = slide.clone();
        let text = serde_json::to_string(&desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::PresentationLiveInspect(output),
        ))
        .unwrap();
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        let tool = "inspect_powerpoint_file";
        let mut proposal = ChatMessage::text("proposal", ChatRole::Assistant, "");
        proposal.tool_calls.push(ToolCallRef {
            id: "read".into(),
            name: tool.into(),
            arguments_json: "{}".into(),
        });
        let mut result = ChatMessage::tool_result("result", "read", text.clone());
        result.data_envelope = Some(DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: "read-envelope".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "blob".into(),
                sha256: hash.clone(),
                size_bytes: text.len() as u64,
                media_type: "application/json".into(),
            },
            provenance: DataProvenance {
                source_provider_id: crate::device_assistant::windows_office::PROVIDER_ID.into(),
                source_tool_name: tool.into(),
                source_object_id: Some("device:read".into()),
                source_envelope_ids: vec![],
            },
            digest_sha256: hash,
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(5000),
                delete_with_run: true,
            },
        });
        let mut session = crate::session::PersistedAgentSession::new(
            "conversation",
            "owner",
            "device",
            1,
            desk_agent_protocol::AgentScope {
                granted: vec![],
                expires_at: None,
                mode: desk_agent_protocol::ExecutionMode::ReadOnly,
                policy_name: None,
            },
            "2026-09-14T00:00:00Z",
        );
        session.conversation = vec![proposal, result];
        assert!(resolve_presentation_read(&session, &file, "worker", &target, 1000).is_ok());
        assert!(resolve_presentation_read(&session, &file, "old-worker", &target, 1000).is_err());
        assert!(resolve_presentation_read(&session, &file, "worker", &target, 5000).is_err());
        for change in 0..8 {
            let mut invalid = session.clone();
            match change {
                0 => invalid.conversation[1].role = ChatRole::User,
                1 => invalid.conversation[1].data_envelope = None,
                2 => invalid.conversation[1].text.push(' '),
                3 => {
                    invalid.conversation[1]
                        .data_envelope
                        .as_mut()
                        .unwrap()
                        .provenance
                        .source_provider_id = "other".into()
                }
                4 => {
                    invalid.conversation[0].tool_calls[0].name = "inspect_live_presentation".into()
                }
                5 => invalid.conversation.push(invalid.conversation[1].clone()),
                6 => invalid.conversation.push(invalid.conversation[0].clone()),
                7 => invalid.conversation[1].tool_call_id = Some("unmatched".into()),
                _ => unreachable!(),
            }
            assert!(
                resolve_presentation_read(&invalid, &file, "worker", &target, 1000).is_err(),
                "case {change}"
            );
        }
    }
    fn object(kind: ObjectKind, token: &str) -> ObjectRef {
        ObjectRef {
            object_kind: kind,
            token: token.into(),
            snapshot_id: "worker:1".into(),
            expires_at: "2030-01-01T00:00:00Z".into(),
        }
    }
    fn output() -> LiveDocumentInspectOutput {
        LiveDocumentInspectOutput {
            snapshot_id: "worker:1".into(),
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficePowerPoint,
                version: office_batch::PPTX_ADAPTER_VERSION.into(),
            },
            projection: LiveDocumentProjection::Presentation {
                presentation: object(ObjectKind::Presentation, "presentation"),
                slide: object(ObjectKind::Slide, "slide"),
                slide_number: 1,
                title: "title".into(),
                presenter_notes: "notes".into(),
            },
            batch_source: Some(BatchDocumentSourceProjection {
                file: object(ObjectKind::File, "file"),
                display_name: "source.pptx".into(),
                byte_len: 100,
                sha256: "a".repeat(64),
            }),
        }
    }
    #[test]
    fn binds_distinct_source_and_slide_on_both_batch_adapters() {
        for adapter in [
            output().adapter,
            ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::IworkKeynote,
                version: crate::device_assistant::IWORK_ADAPTER_VERSION.into(),
            },
        ] {
            let mut output = output();
            output.adapter = adapter;
            let file = output.batch_source.as_ref().unwrap().file.clone();
            let binding =
                PresentationReadBinding::from_authenticated_read(&file, "worker", &output, 1000)
                    .unwrap();
            binding
                .validate_target(&object(ObjectKind::Slide, "slide"), 1000)
                .unwrap();
            assert!(binding.validate_target(&file, 1000).is_err());
            assert!(
                binding
                    .validate_target(&object(ObjectKind::Slide, "other"), 1000)
                    .is_err()
            );
            assert!(
                binding
                    .validate_target(&object(ObjectKind::Slide, "slide"), 1_893_456_000_000)
                    .is_err()
            );
            assert_eq!(binding.source().file, file);
        }
    }
    #[test]
    fn rejects_substituted_source_worker_snapshot_and_live_adapter() {
        let good = output();
        let file = good.batch_source.as_ref().unwrap().file.clone();
        assert!(
            PresentationReadBinding::from_authenticated_read(&file, "other-worker", &good, 1000)
                .is_err()
        );
        for change in 0..5 {
            let mut bad = good.clone();
            match change {
                0 => bad.batch_source.as_mut().unwrap().file.token = "other-file".into(),
                1 => bad.snapshot_id = "worker:2".into(),
                2 => bad.adapter.version = "office-js-bridge-read/v1".into(),
                3 => bad.batch_source = None,
                _ => bad.batch_source.as_mut().unwrap().sha256 = "not-a-digest".into(),
            }
            assert!(
                PresentationReadBinding::from_authenticated_read(&file, "worker", &bad, 1000)
                    .is_err()
            );
        }
    }
}
