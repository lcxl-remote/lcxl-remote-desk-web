//! Validate file-only Office results; hosts still own admission and correlation.
use desk_agent_protocol::{AgentError, AgentErrorKind, computer_use::*};

pub fn validate(
    adapter: &ComputerUseAdapterRef,
    action: &ComputerActionKind,
    completed: &ComputerActionCompleted,
    now: u64,
) -> Result<(), AgentError> {
    let (name, mime) = match action {
        ComputerActionKind::SpreadsheetLiveBatch(batch)
            if office_batch::is_xlsx(adapter)
                && office_batch::valid_xlsx_leaf(&batch.output.native_file_name) =>
        {
            (
                &batch.output.native_file_name,
                office_batch::XLSX_MEDIA_TYPE,
            )
        }
        ComputerActionKind::PresentationLiveBatch(batch)
            if office_batch::is_pptx(adapter)
                && office_batch::valid_pptx_leaf(&batch.output.native_file_name) =>
        {
            (
                &batch.output.native_file_name,
                office_batch::PPTX_MEDIA_TYPE,
            )
        }
        ComputerActionKind::DocumentLiveBatch(batch)
            if office_batch::is_docx(adapter)
                && office_batch::valid_docx_leaf(&batch.output.native_file_name) =>
        {
            (
                &batch.output.native_file_name,
                office_batch::DOCX_MEDIA_TYPE,
            )
        }
        _ => return Err(invalid()),
    };
    if now == 0 || completed.facts.len() > 1 || completed.facts.iter().any(|fact| fact.index != 0) {
        return Err(invalid());
    }
    if completed.result != ComputerActionResultClass::Verified {
        return if completed.output.is_none()
            && completed.facts.iter().all(|fact| !fact.verified)
            && (completed.result == ComputerActionResultClass::OutcomeUnknown
                || completed.facts.iter().all(|fact| !fact.changed))
        {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    let Some(ComputerActionOutput::FileArtifact(artifact)) = &completed.output else {
        return Err(invalid());
    };
    if completed.facts.len() != 1
        || !completed.facts[0].changed
        || !completed.facts[0].verified
        || artifact.validate().is_err()
        || &artifact.file_name != name
        || artifact.media_type != mime
        || artifact.size_bytes > 16 * 1024 * 1024
        || chrono::DateTime::parse_from_rfc3339(&artifact.file.expires_at)
            .ok()
            .and_then(|value| u64::try_from(value.timestamp_millis()).ok())
            .is_none_or(|expiry| expiry <= now)
    {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> AgentError {
    AgentError {
        kind: AgentErrorKind::PermissionDenied,
        message: "Office batch completion does not match the reviewed file action".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xlsx_completion_binds_reviewed_artifact_and_never_upgrades_unknown_outcome() {
        let object = |kind| ObjectRef {
            token: "artifact".into(),
            snapshot_id: "worker:1".into(),
            object_kind: kind,
            expires_at: "2030-01-01T00:00:00Z".into(),
        };
        let name = format!("{}.XLSX", "a".repeat(250));
        let adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::OfficeExcel,
            version: office_batch::XLSX_ADAPTER_VERSION.into(),
        };
        let action = ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
            output: BatchDocumentOutput {
                destination_parent: object(ObjectKind::Directory),
                native_file_name: name.clone(),
            },
            action: SpreadsheetLivePatchAction::SetCellFormula {
                formula: "=A1*2".into(),
            },
        });
        let artifact = CreatedFileArtifactOutput {
            file: object(ObjectKind::File),
            file_name: name,
            media_type: office_batch::XLSX_MEDIA_TYPE.into(),
            size_bytes: 10,
            digest_sha256: "a".repeat(64),
            content: desk_agent_protocol::data_lineage::ContentRef::Artifact {
                artifact_id: "artifact".into(),
                sha256: "a".repeat(64),
                size_bytes: 10,
                media_type: office_batch::XLSX_MEDIA_TYPE.into(),
            },
        };
        let good = ComputerActionCompleted {
            work_id: "work".into(),
            action_request_id: "call".into(),
            execution_generation: "run".into(),
            result: ComputerActionResultClass::Verified,
            facts: vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: true,
                summary: "verified copy".into(),
            }],
            message: None,
            output: Some(ComputerActionOutput::FileArtifact(artifact)),
        };
        validate(&adapter, &action, &good, 1).unwrap();
        for case in 0..7 {
            let mut bad = good.clone();
            match case {
                0 => bad.output = None,
                1 => bad.facts[0].verified = false,
                2 => bad.result = ComputerActionResultClass::OutcomeUnknown,
                _ => {
                    let Some(ComputerActionOutput::FileArtifact(artifact)) = &mut bad.output else {
                        unreachable!()
                    };
                    match case {
                        3 => artifact.file_name = "other.xlsx".into(),
                        4 => artifact.media_type = office_batch::DOCX_MEDIA_TYPE.into(),
                        5 => artifact.digest_sha256 = "b".repeat(64),
                        _ => artifact.file.expires_at = "1970-01-01T00:00:00Z".into(),
                    }
                }
            }
            assert!(validate(&adapter, &action, &bad, 1).is_err(), "{case}");
        }
        let mut unknown = good;
        unknown.result = ComputerActionResultClass::OutcomeUnknown;
        unknown.output = None;
        unknown.facts[0].verified = false;
        validate(&adapter, &action, &unknown, 1).unwrap();
        let wrong_adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::OfficeExcel,
            version: "office-xlsx-batch/v2".into(),
        };
        assert!(validate(&wrong_adapter, &action, &unknown, 1).is_err());
    }
    #[test]
    fn docx_completion_binds_reviewed_name_content_and_outcome() {
        let object = |kind| ObjectRef {
            token: "object".into(),
            snapshot_id: "worker:1".into(),
            object_kind: kind,
            expires_at: "2030-01-01T00:00:00Z".into(),
        };
        let name = format!("{}.docx", "a".repeat(250));
        let adapter = ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::OfficeWord,
            version: office_batch::DOCX_ADAPTER_VERSION.into(),
        };
        let action = ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
            output: BatchDocumentOutput {
                destination_parent: object(ObjectKind::Directory),
                native_file_name: name.clone(),
            },
            action: DocumentLivePatchAction::ReplaceBodyText {
                text: "body".into(),
            },
        });
        let artifact = CreatedFileArtifactOutput {
            file: object(ObjectKind::File),
            file_name: name,
            media_type: office_batch::DOCX_MEDIA_TYPE.into(),
            size_bytes: 10,
            digest_sha256: "a".repeat(64),
            content: desk_agent_protocol::data_lineage::ContentRef::Artifact {
                artifact_id: "object".into(),
                sha256: "a".repeat(64),
                size_bytes: 10,
                media_type: office_batch::DOCX_MEDIA_TYPE.into(),
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
                summary: "created".into(),
            }],
            message: None,
            output: Some(ComputerActionOutput::FileArtifact(artifact)),
        };
        validate(&adapter, &action, &completed, 1).unwrap();
        for case in 0..6 {
            let mut wrong = completed.clone();
            let Some(ComputerActionOutput::FileArtifact(artifact)) = &mut wrong.output else {
                unreachable!()
            };
            match case {
                0 => artifact.file_name = "other.docx".into(),
                1 => artifact.media_type = office_batch::PPTX_MEDIA_TYPE.into(),
                2 => artifact.file.expires_at = "1970-01-01T00:00:00Z".into(),
                3 => artifact.digest_sha256 = "b".repeat(64),
                4 => wrong.facts[0].verified = false,
                5 => wrong.result = ComputerActionResultClass::OutcomeUnknown,
                _ => unreachable!(),
            }
            assert!(validate(&adapter, &action, &wrong, 1).is_err());
        }
        let mut unknown = completed;
        unknown.result = ComputerActionResultClass::OutcomeUnknown;
        unknown.output = None;
        unknown.facts[0].verified = false;
        validate(&adapter, &action, &unknown, 1).unwrap();
        unknown.result = ComputerActionResultClass::DefinitelyNotStarted;
        assert!(validate(&adapter, &action, &unknown, 1).is_err());
    }
}
