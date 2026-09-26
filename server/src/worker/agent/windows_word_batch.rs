//! DOCX observations and unpublished copies bound to Windows file-store snapshots.
use super::file_reference_store::windows_batch::{self, Format, Snapshot};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    computer_use::{BatchDocumentSourceProjection, DocumentLivePatchAction, ObjectRef},
};
use desk_office_batch::{
    docx_copy,
    docx_inspect::{self, Projection},
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub struct DocumentObservation {
    snapshot: Arc<Snapshot>,
    projection: Projection,
}

pub struct PreparedDocument {
    source: Arc<Snapshot>,
    bytes: Vec<u8>,
    sha256: String,
}
impl PreparedDocument {
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

pub fn observe_document(
    source: &ObjectRef,
    max_text_bytes: usize,
) -> Result<DocumentObservation, AgentError> {
    let snapshot = windows_batch::capture(source, Format::Document)?;
    let projection =
        docx_inspect::inspect(snapshot.bytes(), max_text_bytes).map_err(format_error)?;
    snapshot.revalidate()?;
    Ok(DocumentObservation {
        snapshot: Arc::new(snapshot),
        projection,
    })
}

impl DocumentObservation {
    pub fn source(&self) -> BatchDocumentSourceProjection {
        self.snapshot.projection()
    }
    pub fn projection(&self) -> &Projection {
        &self.projection
    }
    pub fn body_sha256(&self) -> String {
        format!("{:x}", Sha256::digest(self.projection.body_text.as_bytes()))
    }

    /// The broker must separately authorize the action and publish once into a
    /// current approved directory. Preparing bytes is not an execution receipt.
    pub fn prepare_copy(
        &self,
        action: DocumentLivePatchAction,
    ) -> Result<PreparedDocument, AgentError> {
        self.snapshot.revalidate()?;
        let bytes = docx_copy::copy(self.snapshot.bytes(), action).map_err(format_error)?;
        self.snapshot.revalidate()?;
        Ok(PreparedDocument {
            source: Arc::clone(&self.snapshot),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes,
        })
    }
}

fn format_error(cause: impl std::fmt::Display) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("Word batch: {cause}"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::file_reference_store;
    use super::*;
    use std::io::{Cursor, Write};
    fn fixture() -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, text) in [
            (
                "[Content_Types].xml",
                "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>",
            ),
            (
                "_rels/.rels",
                "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"main\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>",
            ),
            (
                "word/document.xml",
                "<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>原文</w:t></w:r></w:p><w:sectPr/></w:body></w:document>",
            ),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }
    #[tokio::test]
    async fn admitted_word_executor_returns_typed_receipt_and_releases_lease() {
        use super::super::{
            computer_use_broker::ComputerUseBroker,
            computer_use_writer::{WriterLeaseRequest, WriterLeaseScope},
            windows_office_batch,
        };
        use crate::model::settings::ComputerUseSettings;
        use desk_agent_protocol::computer_use::*;
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.docx");
        let original = fixture();
        for scenario in [
            "success",
            "missing-lease",
            "changed-source",
            "wrong-adapter",
            "cancelled",
            "collision",
        ] {
            std::fs::write(&path, &original).unwrap();
            let source = file_reference_store::issue(&path).unwrap();
            let broker = std::sync::Arc::new(ComputerUseBroker::new());
            let ceiling = ComputerUseSettings {
                enabled: true,
                observe: true,
                office_semantic: true,
                ..Default::default()
            };
            let readiness = broker.readiness(&ceiling, false, false);
            let params: LiveDocumentInspectParams = serde_json::from_value(
                serde_json::json!({"batch_file": source, "max_bytes":16384}),
            )
            .unwrap();
            let observed = broker.inspect_word_batch(&params, &ceiling).unwrap();
            let LiveDocumentProjection::Document { document, .. } = observed.projection else {
                panic!()
            };
            let expires_at = chrono::Utc::now() + chrono::Duration::seconds(30);
            let file_name = format!("{scenario}.docx");
            let mut plan = SealedComputerActionPlan {
                turn_scope: None,
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
                timeout_ms: 30000,
                actions: vec![ComputerActionStep {
                    target: document,
                    action: ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
                        output: BatchDocumentOutput {
                            destination_parent: file_reference_store::issue(temp.path()).unwrap(),
                            native_file_name: file_name.clone(),
                        },
                        action: DocumentLivePatchAction::ReplaceBodyText {
                            text: "批准正文\n".into(),
                        },
                    }),
                    before_summary: "Original body".into(),
                    after_intent: "Create approved copy".into(),
                    verification: "Read back body".into(),
                }],
            };
            plan.validate().unwrap();
            windows_office_batch::preflight(&broker, &plan, &ceiling).unwrap();
            let lease = WriterLeaseRequest {
                scope: WriterLeaseScope::FileWorker,
                work_id: plan.work_id.clone(),
                action_request_id: plan.action_request_id.clone(),
                execution_generation: scenario.into(),
                approved_actor_id: "owner".into(),
                interactive_session_incarnation: readiness.interactive_session_incarnation,
                expires_at,
            };
            if scenario != "missing-lease" {
                broker.acquire_writer_lease(lease.clone()).unwrap();
            }
            if scenario == "changed-source" {
                std::fs::write(&path, b"external change").unwrap();
            }
            if scenario == "wrong-adapter" {
                plan.adapter.kind = ComputerUseAdapterKind::OfficePowerPoint;
            }
            if scenario == "cancelled" {
                assert!(broker.cancel_writer_lease(
                    &ComputerActionCancel {
                        work_id: plan.work_id.clone(),
                        action_request_id: plan.action_request_id.clone(),
                        execution_generation: plan.execution_generation.clone(),
                        reason: "synthetic owner cancellation".into(),
                    },
                    "owner"
                ));
            }
            if scenario == "collision" {
                std::fs::write(temp.path().join(&file_name), b"existing output").unwrap();
            }
            if matches!(scenario, "changed-source" | "wrong-adapter") {
                assert!(windows_office_batch::preflight(&broker, &plan, &ceiling).is_err());
            }
            let receipt = windows_office_batch::execute_admitted(
                broker.clone(),
                plan.clone(),
                ceiling.clone(),
            )
            .await;
            assert_eq!(receipt.work_id, plan.work_id);
            assert_eq!(receipt.action_request_id, plan.action_request_id);
            assert_eq!(receipt.execution_generation, plan.execution_generation);
            if scenario != "wrong-adapter" {
                desk_diagnose_core::provider_preflight::office_file_completion::validate(
                    &plan.adapter,
                    &plan.actions[0].action,
                    &receipt,
                    chrono::Utc::now().timestamp_millis() as u64,
                )
                .unwrap();
            }
            if scenario == "success" {
                assert_eq!(receipt.result, ComputerActionResultClass::Verified);
                let Some(ComputerActionOutput::FileArtifact(artifact)) = receipt.output else {
                    panic!()
                };
                assert_eq!(artifact.media_type, office_batch::DOCX_MEDIA_TYPE);
                let bytes = std::fs::read(temp.path().join(&file_name)).unwrap();
                assert_eq!(
                    artifact.digest_sha256,
                    format!("{:x}", Sha256::digest(&bytes))
                );
                assert_eq!(
                    docx_inspect::inspect(&bytes, 16384).unwrap().body_text,
                    "批准正文\n"
                );
                assert_eq!(std::fs::read(&path).unwrap(), original);
            } else {
                assert_eq!(
                    receipt.result,
                    ComputerActionResultClass::DefinitelyNotStarted
                );
                assert!(receipt.output.is_none());
                if scenario == "collision" {
                    assert_eq!(
                        std::fs::read(temp.path().join(&file_name)).unwrap(),
                        b"existing output"
                    );
                } else {
                    assert!(!temp.path().join(&file_name).exists());
                }
            }
            if scenario != "changed-source" {
                assert_eq!(std::fs::read(&path).unwrap(), original);
            }
            let mut next_lease = lease;
            next_lease.execution_generation = format!("next-{scenario}");
            broker.acquire_writer_lease(next_lease.clone()).unwrap();
            assert!(broker.release_writer_lease(&next_lease.execution_generation));
        }
    }

    #[test]
    fn word_broker_publishes_selected_copy_under_current_lease() {
        use super::super::{
            computer_use_broker::ComputerUseBroker,
            computer_use_writer::{WriterLeaseRequest, WriterLeaseScope},
        };
        use crate::model::settings::ComputerUseSettings;
        use desk_agent_protocol::computer_use::*;
        use file_reference_store::windows_publish::PublishFailure;
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("选中 source.docx");
        let original = fixture();
        std::fs::write(&path, &original).unwrap();
        let source = file_reference_store::issue(&path).unwrap();
        let directory = file_reference_store::issue(temp.path()).unwrap();
        let broker = ComputerUseBroker::new();
        let ceiling = ComputerUseSettings {
            enabled: true,
            observe: true,
            office_semantic: true,
            ..Default::default()
        };
        let params: LiveDocumentInspectParams =
            serde_json::from_value(serde_json::json!({"batch_file":source,"max_bytes":16384}))
                .unwrap();
        let readiness = broker.readiness(&ceiling, false, false);
        let output = broker.inspect_word_batch(&params, &ceiling).unwrap();
        assert_eq!(output.batch_source.as_ref().unwrap().file, source);
        let LiveDocumentProjection::Document {
            document,
            body_text,
            ..
        } = &output.projection
        else {
            panic!()
        };
        assert_eq!(body_text, "原文");
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let (_, worker_incarnation) = readiness
            .interactive_session_incarnation
            .split_once(':')
            .unwrap();
        desk_diagnose_core::provider_preflight::word_read_binding::WordReadBinding::from_authenticated_read(
            &source, worker_incarnation, &output, now).unwrap()
            .validate_target(document, now).unwrap();
        let mut batch = DocumentLiveBatchPatchAction {
            output: BatchDocumentOutput {
                destination_parent: directory,
                native_file_name: format!("{}.DOCX", "a".repeat(250)),
            },
            action: DocumentLivePatchAction::ReplaceBodyText {
                text: "批准正文\n\n值\t中文\n".into(),
            },
        };
        assert!(matches!(
            broker.publish_word_copy_with_lease(document, &batch, &ceiling, "word-generation"),
            Err(PublishFailure::NotCreated(_))
        ));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        broker
            .acquire_writer_lease(WriterLeaseRequest {
                scope: WriterLeaseScope::InteractiveSession,
                work_id: "word-work".into(),
                action_request_id: "word-call".into(),
                execution_generation: "word-generation".into(),
                approved_actor_id: "owner".into(),
                interactive_session_incarnation: readiness.interactive_session_incarnation,
                expires_at: chrono::Utc::now() + chrono::Duration::seconds(30),
            })
            .unwrap();
        assert!(matches!(
            broker.publish_word_copy_with_lease(document, &batch, &ceiling, "wrong-generation"),
            Err(PublishFailure::NotCreated(_))
        ));
        let artifact = broker
            .publish_word_copy_with_lease(document, &batch, &ceiling, "word-generation")
            .unwrap();
        let bytes = std::fs::read(temp.path().join(&artifact.file_name)).unwrap();
        assert_eq!(artifact.sha256, format!("{:x}", Sha256::digest(&bytes)));
        assert_eq!(
            docx_inspect::inspect(&bytes, 16384).unwrap().body_text,
            "批准正文\n\n值\t中文\n"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(matches!(
            broker.publish_word_copy_with_lease(document, &batch, &ceiling, "word-generation"),
            Err(PublishFailure::NotCreated(_))
        ));
        batch.output.native_file_name = "changed-source.docx".into();
        std::fs::write(&path, b"external change").unwrap();
        assert!(matches!(
            broker.publish_word_copy_with_lease(document, &batch, &ceiling, "word-generation"),
            Err(PublishFailure::NotCreated(_))
        ));
        assert!(!temp.path().join("changed-source.docx").exists());
        std::fs::write(&path, original).unwrap();
        assert!(broker.release_writer_lease("word-generation"));
        assert!(matches!(
            broker.publish_word_copy_with_lease(document, &batch, &ceiling, "word-generation"),
            Err(PublishFailure::NotCreated(_))
        ));
        let mut disabled = ceiling.clone();
        disabled.office_semantic = false;
        assert!(broker.inspect_word_batch(&params, &disabled).is_err());
        assert!(
            broker
                .prepare_word_copy(document, batch.action.clone(), &disabled)
                .is_err()
        );
        broker.reset_worker_incarnation();
        assert!(
            broker
                .prepare_word_copy(document, batch.action, &ceiling)
                .is_err()
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
    }

    #[test]
    fn selected_word_snapshot_prepares_only_a_copy_and_rejects_changed_source() {
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("源文件 source.docx");
        let original = fixture();
        std::fs::write(&path, &original).unwrap();
        let reference = file_reference_store::issue(&path).unwrap();
        let observed = observe_document(&reference, 16384).unwrap();
        assert_eq!(observed.source().file, reference);
        assert_eq!(observed.projection().body_text, "原文");
        assert_eq!(
            observed.body_sha256(),
            format!("{:x}", Sha256::digest("原文".as_bytes()))
        );
        let action = DocumentLivePatchAction::ReplaceBodyText {
            text: "正文\n\n结尾\t值\n".into(),
        };
        let copy = observed.prepare_copy(action.clone()).unwrap();
        assert_eq!(
            docx_inspect::inspect(copy.bytes(), 16384)
                .unwrap()
                .body_text,
            "正文\n\n结尾\t值\n"
        );
        assert_eq!(copy.sha256(), format!("{:x}", Sha256::digest(copy.bytes())));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        std::fs::write(&path, b"changed after observation").unwrap();
        assert!(observed.prepare_copy(action).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"changed after observation");
    }
}
