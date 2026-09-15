//! Windows file-only Office observation, copy execution and publication receipts.
use super::file_reference_store::windows_batch::{self, Format, Snapshot};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{BatchDocumentSourceProjection, ObjectRef, PresentationLivePatchAction},
};
use desk_office_batch::{
    pptx_copy,
    pptx_inspect::{self, Projection},
};
use sha2::{Digest, Sha256};

use super::computer_use_writer::spawn_writer_task as spawn_publication;

pub(crate) fn preflight(
    broker: &super::computer_use_broker::ComputerUseBroker,
    plan: &desk_agent_protocol::computer_use::SealedComputerActionPlan,
    ceiling: &crate::model::settings::ComputerUseSettings,
) -> Result<(), AgentError> {
    use desk_agent_protocol::computer_use::{ComputerActionKind, office_batch};
    let [step] = plan.actions.as_slice() else {
        return Err(format_error("invalid Office batch plan"));
    };
    match &step.action {
        ComputerActionKind::SpreadsheetLiveBatch(batch) if office_batch::is_xlsx(&plan.adapter) => {
            broker
                .prepare_excel_copy(&step.target, &batch.action, ceiling)
                .map(|_| ())
        }
        ComputerActionKind::PresentationLiveBatch(batch)
            if office_batch::is_pptx(&plan.adapter) =>
        {
            broker
                .prepare_powerpoint_copy(&step.target, batch.action.clone(), ceiling)
                .map(|_| ())
        }
        ComputerActionKind::DocumentLiveBatch(batch) if office_batch::is_docx(&plan.adapter) => {
            broker
                .prepare_word_copy(&step.target, batch.action.clone(), ceiling)
                .map(|_| ())
        }
        _ => Err(format_error(
            "the requested Office batch adapter or action is unavailable",
        )),
    }
}

pub(crate) async fn execute_admitted(
    broker: std::sync::Arc<super::computer_use_broker::ComputerUseBroker>,
    plan: desk_agent_protocol::computer_use::SealedComputerActionPlan,
    ceiling: crate::model::settings::ComputerUseSettings,
) -> desk_agent_protocol::computer_use::ComputerActionCompleted {
    use super::file_reference_store::windows_publish::PublishFailure;
    use desk_agent_protocol::computer_use::{ComputerActionKind, office_batch};
    let work_id = plan.work_id.clone();
    let call_id = plan.action_request_id.clone();
    let generation = plan.execution_generation.clone();
    let media_type = if office_batch::is_docx(&plan.adapter) {
        office_batch::DOCX_MEDIA_TYPE
    } else if office_batch::is_xlsx(&plan.adapter) {
        office_batch::XLSX_MEDIA_TYPE
    } else {
        office_batch::PPTX_MEDIA_TYPE
    };
    let worker = broker.clone();
    let result = spawn_publication(broker, generation.clone(), move || {
        if plan.actions.len() != 1 {
            return Err(PublishFailure::NotCreated(format_error(
                "invalid Office batch plan",
            )));
        }
        let step = &plan.actions[0];
        match &step.action {
            ComputerActionKind::SpreadsheetLiveBatch(batch)
                if office_batch::is_xlsx(&plan.adapter) =>
            {
                worker.publish_excel_copy_with_lease(
                    &step.target,
                    batch,
                    &ceiling,
                    &plan.execution_generation,
                )
            }
            ComputerActionKind::PresentationLiveBatch(batch)
                if office_batch::is_pptx(&plan.adapter) =>
            {
                worker.publish_powerpoint_copy_with_lease(
                    &step.target,
                    batch,
                    &ceiling,
                    &plan.execution_generation,
                )
            }
            ComputerActionKind::DocumentLiveBatch(batch)
                if office_batch::is_docx(&plan.adapter) =>
            {
                worker.publish_word_copy_with_lease(
                    &step.target,
                    batch,
                    &ceiling,
                    &plan.execution_generation,
                )
            }
            _ => Err(PublishFailure::NotCreated(format_error(
                "invalid Office batch action",
            ))),
        }
    })
    .await
    .unwrap_or_else(|_| {
        Err(PublishFailure::OutcomeUnknown(format_error(
            "Office batch executor interrupted",
        )))
    });
    publication_receipt_for(media_type, work_id, call_id, generation, result)
}

/// Convert a publication outcome without losing whether a file may exist.
/// The caller provides correlation from its admitted, sealed action plan.
#[cfg(test)]
pub(crate) fn publication_receipt(
    work_id: String,
    action_request_id: String,
    execution_generation: String,
    result: Result<
        super::file_reference_store::CreatedTextArtifact,
        super::file_reference_store::windows_publish::PublishFailure,
    >,
) -> desk_agent_protocol::computer_use::ComputerActionCompleted {
    publication_receipt_for(
        desk_agent_protocol::computer_use::office_batch::PPTX_MEDIA_TYPE,
        work_id,
        action_request_id,
        execution_generation,
        result,
    )
}

fn publication_receipt_for(
    media_type: &'static str,
    work_id: String,
    action_request_id: String,
    execution_generation: String,
    result: Result<
        super::file_reference_store::CreatedTextArtifact,
        super::file_reference_store::windows_publish::PublishFailure,
    >,
) -> desk_agent_protocol::computer_use::ComputerActionCompleted {
    use super::file_reference_store::windows_publish::PublishFailure;
    use desk_agent_protocol::computer_use::{
        ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass as Class,
        ComputerActionStepFact, CreatedFileArtifactOutput,
    };
    let (class, changed, output, message) = match result {
        Ok(artifact) => {
            let output = CreatedFileArtifactOutput {
                file: artifact.file.clone(),
                file_name: artifact.file_name,
                media_type: media_type.into(),
                size_bytes: artifact.byte_len,
                digest_sha256: artifact.sha256.clone(),
                content: desk_agent_protocol::data_lineage::ContentRef::Artifact {
                    artifact_id: artifact.file.token,
                    sha256: artifact.sha256,
                    size_bytes: artifact.byte_len,
                    media_type: media_type.into(),
                },
            };
            if output.validate().is_ok() {
                (
                    Class::Verified,
                    true,
                    Some(ComputerActionOutput::FileArtifact(output)),
                    None,
                )
            } else {
                (
                    Class::OutcomeUnknown,
                    true,
                    None,
                    Some(
                        "Published file receipt could not be validated; do not retry automatically"
                            .into(),
                    ),
                )
            }
        }
        Err(PublishFailure::NotCreated(cause)) => {
            // Keep native diagnostics local and bounded; the model-facing
            // receipt must not include COM messages or filesystem details.
            tracing::warn!(reason = %cause.message.chars().take(512).collect::<String>(),
                "Office batch stopped before creating the output file");
            (
                Class::DefinitelyNotStarted,
                false,
                None,
                Some("Office file copy was not created".into()),
            )
        }
        Err(PublishFailure::OutcomeUnknown(_)) => (
            Class::OutcomeUnknown,
            true,
            None,
            Some("Office file copy may exist; do not retry automatically".into()),
        ),
    };
    ComputerActionCompleted {
        work_id,
        action_request_id,
        execution_generation,
        result: class,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed,
            verified: class == Class::Verified,
            summary: match class {
                Class::Verified => "Created and verified Office file copy",
                Class::DefinitelyNotStarted => "No Office file copy created",
                _ => "Office file copy outcome unknown",
            }
            .into(),
        }],
        message,
        output,
    }
}

pub struct PresentationObservation {
    snapshot: std::sync::Arc<Snapshot>,
    projection: Projection,
}

/// An unpublished artifact. Its existence never proves authorization or delivery.
pub struct PreparedPresentation {
    source: std::sync::Arc<Snapshot>,
    bytes: Vec<u8>,
    sha256: String,
}

impl PreparedPresentation {
    pub fn pin_source(&self) -> Result<windows_batch::PinnedSource, AgentError> {
        self.source.pin()
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

pub fn observe_presentation(
    source: &ObjectRef,
    slide_number: usize,
    max_text_bytes: usize,
) -> Result<PresentationObservation, AgentError> {
    let snapshot = windows_batch::capture(source, Format::Presentation)?;
    let projection = pptx_inspect::inspect(snapshot.bytes(), slide_number, max_text_bytes)
        .map_err(format_error)?;
    snapshot.revalidate()?;
    Ok(PresentationObservation {
        snapshot: std::sync::Arc::new(snapshot),
        projection,
    })
}

impl PresentationObservation {
    pub fn source(&self) -> BatchDocumentSourceProjection {
        self.snapshot.projection()
    }
    pub fn projection(&self) -> &Projection {
        &self.projection
    }

    /// Call only through the host's current target and exact-input authorization.
    /// The host must separately revalidate its output reference and publish once.
    pub fn prepare_copy(
        &self,
        action: PresentationLivePatchAction,
    ) -> Result<PreparedPresentation, AgentError> {
        let supported = match &action {
            PresentationLivePatchAction::ReplaceSlideTitle { .. } => {
                self.projection.can_replace_title
            }
            PresentationLivePatchAction::SetPresenterNotes { .. } => self.projection.can_set_notes,
        };
        if !supported {
            return Err(format_error(
                "observed placeholder does not support this batch edit",
            ));
        }
        self.snapshot.revalidate()?;
        let bytes = pptx_copy::copy(
            self.snapshot.bytes(),
            self.projection.slide_number,
            action.clone(),
        )
        .map_err(format_error)?;
        // Read back semantic text rather than treating ZIP creation as verification.
        let after = pptx_inspect::inspect(&bytes, self.projection.slide_number, 16384)
            .map_err(format_error)?;
        let verified = match &action {
            PresentationLivePatchAction::ReplaceSlideTitle { text } => {
                after.title.as_ref() == Some(text)
                    && after.presenter_notes == self.projection.presenter_notes
            }
            PresentationLivePatchAction::SetPresenterNotes { text } => {
                after.presenter_notes.as_ref() == Some(text) && after.title == self.projection.title
            }
        };
        if !verified {
            return Err(format_error(
                "prepared presentation semantic readback mismatch",
            ));
        }
        self.snapshot.revalidate()?;
        Ok(PreparedPresentation {
            source: std::sync::Arc::clone(&self.snapshot),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes,
        })
    }
}

fn format_error(cause: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("Office batch: {cause}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::file_reference_store;
    use super::*;
    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/office/title-notes.pptx"
    ));

    #[test]
    fn source_observation_and_single_action_copy_share_one_snapshot() {
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("演示 source.pptx");
        std::fs::write(&path, FIXTURE).unwrap();
        let source = file_reference_store::issue(&path).unwrap();
        let observed = observe_presentation(&source, 1, 16384).unwrap();
        assert_eq!(observed.source().file, source);
        assert_eq!(
            observed.source().sha256,
            format!("{:x}", Sha256::digest(FIXTURE))
        );
        assert_eq!(observed.projection().title.as_deref(), Some("原始标题"));
        let action = PresentationLivePatchAction::SetPresenterNotes {
            text: "第一段\n\n最后一段\n".into(),
        };
        let prepared = observed.prepare_copy(action).unwrap();
        let readback = pptx_inspect::inspect(prepared.bytes(), 1, 16384).unwrap();
        assert_eq!(
            readback.presenter_notes.as_deref(),
            Some("第一段\n\n最后一段\n")
        );
        assert_eq!(readback.title, observed.projection().title);
        assert_eq!(
            prepared.sha256(),
            format!("{:x}", Sha256::digest(prepared.bytes()))
        );
        assert_eq!(std::fs::read(&path).unwrap(), FIXTURE);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        std::fs::write(&path, b"changed after approval").unwrap();
        assert!(
            observed
                .prepare_copy(PresentationLivePatchAction::ReplaceSlideTitle { text: "new".into() })
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"changed after approval");
    }

    #[tokio::test]
    async fn admitted_copy_executor_publishes_verified_receipt_and_releases_every_path() {
        use super::super::computer_use_broker::ComputerUseBroker;
        use super::super::computer_use_writer::{WriterLeaseRequest, WriterLeaseScope};
        use crate::model::settings::ComputerUseSettings;
        use desk_agent_protocol::computer_use::*;
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.pptx");
        let ceiling = ComputerUseSettings {
            enabled: true,
            observe: true,
            office_semantic: true,
            ..Default::default()
        };
        for scenario in [
            "success",
            "success-long-ascii",
            "success-long-unicode",
            "changed-source",
            "missing-lease",
        ] {
            let file_name = match scenario {
                "success-long-ascii" => format!("{}.pptx", "a".repeat(250)),
                "success-long-unicode" => format!("{}.PPTX", "中".repeat(83)),
                _ => format!("{scenario}.pptx"),
            };
            std::fs::write(&path, FIXTURE).unwrap();
            let source = file_reference_store::issue(&path).unwrap();
            let broker = std::sync::Arc::new(ComputerUseBroker::new());
            let readiness = broker.readiness(&ceiling, false, false);
            let params: LiveDocumentInspectParams = serde_json::from_value(
                serde_json::json!({"batch_file": source, "max_bytes": 16384}),
            )
            .unwrap();
            let observed = broker.inspect_powerpoint_batch(&params, &ceiling).unwrap();
            let LiveDocumentProjection::Presentation { slide, .. } = observed.projection else {
                panic!()
            };
            let expires_at = chrono::Utc::now() + chrono::Duration::seconds(30);
            let plan = SealedComputerActionPlan {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                work_id: "work".into(),
                action_request_id: "call".into(),
                execution_generation: scenario.into(),
                device_id: "device".into(),
                interactive_session_incarnation: readiness.interactive_session_incarnation.clone(),
                adapter: observed.adapter,
                approval_id: "approval".into(),
                approved_actor_id: "owner".into(),
                draft_hash: "a".repeat(64),
                expires_at: expires_at.to_rfc3339(),
                timeout_ms: 30_000,
                actions: vec![ComputerActionStep {
                    target: slide,
                    action: ComputerActionKind::PresentationLiveBatch(
                        PresentationLiveBatchPatchAction {
                            output: BatchDocumentOutput {
                                destination_parent: file_reference_store::issue(temp.path())
                                    .unwrap(),
                                native_file_name: file_name.clone(),
                            },
                            action: PresentationLivePatchAction::ReplaceSlideTitle {
                                text: "Approved title".into(),
                            },
                        },
                    ),
                    before_summary: "Original title".into(),
                    after_intent: "Create reviewed copy".into(),
                    verification: "Read back title and unchanged notes".into(),
                }],
            };
            plan.validate().unwrap();
            let lease = WriterLeaseRequest {
                scope: WriterLeaseScope::InteractiveSession,
                work_id: plan.work_id.clone(),
                action_request_id: plan.action_request_id.clone(),
                execution_generation: scenario.into(),
                approved_actor_id: plan.approved_actor_id.clone(),
                interactive_session_incarnation: readiness.interactive_session_incarnation,
                expires_at,
            };
            if scenario != "missing-lease" {
                broker.acquire_writer_lease(lease.clone()).unwrap();
            }
            if scenario == "changed-source" {
                std::fs::write(&path, b"external change").unwrap();
            }
            let receipt = execute_admitted(broker.clone(), plan.clone(), ceiling.clone()).await;
            assert_eq!(receipt.work_id, plan.work_id);
            assert_eq!(receipt.action_request_id, plan.action_request_id);
            assert_eq!(receipt.execution_generation, plan.execution_generation);
            desk_diagnose_core::provider_preflight::batch_document::validate_pptx_completion(
                &plan.adapter,
                &plan.actions[0].action,
                &receipt,
                chrono::Utc::now().timestamp_millis() as u64,
            )
            .unwrap();
            if scenario.starts_with("success") {
                assert_eq!(receipt.result, ComputerActionResultClass::Verified);
                let Some(ComputerActionOutput::FileArtifact(artifact)) = receipt.output else {
                    panic!()
                };
                assert_eq!(artifact.file_name, file_name);
                let bytes =
                    file_reference_store::read_verified_bytes(&artifact.file, artifact.size_bytes)
                        .unwrap();
                assert_eq!(bytes.sha256, artifact.digest_sha256);
                assert_eq!(
                    pptx_inspect::inspect(&bytes.bytes, 1, 16384)
                        .unwrap()
                        .title
                        .as_deref(),
                    Some("Approved title")
                );
                assert_eq!(std::fs::read(&path).unwrap(), FIXTURE);
            } else {
                assert_eq!(
                    receipt.result,
                    ComputerActionResultClass::DefinitelyNotStarted
                );
                assert!(receipt.output.is_none());
                assert!(!temp.path().join(&file_name).exists());
            }
            assert!(broker.require_writer_lease(scenario).is_err());
            broker
                .acquire_writer_lease(WriterLeaseRequest {
                    execution_generation: "next".into(),
                    ..lease
                })
                .unwrap();
            assert!(broker.release_writer_lease("next"));
        }
    }
}
