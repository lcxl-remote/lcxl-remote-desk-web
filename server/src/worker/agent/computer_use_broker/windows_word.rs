//! Selected DOCX observation and publication under the shared broker authority.
use super::super::{
    file_reference_store::{
        CreatedTextArtifact,
        windows_publish::{self, PublishFailure},
    },
    windows_word_batch,
};
use super::*;
use desk_agent_protocol::computer_use::{
    DocumentLiveBatchPatchAction, DocumentLivePatchAction, LiveDocumentInspectOutput,
    LiveDocumentInspectParams, LiveDocumentProjection, office_batch,
};

pub(super) fn readiness(
    ceiling: &ComputerUseSettings,
    session_ready: bool,
    session_reason: Option<ComputerUseReadinessReason>,
) -> ComputerUseCapabilityReadiness {
    let mut report = super::windows_office::readiness(ceiling, session_ready, session_reason);
    report.capability = Capability::DocumentLiveInspect;
    report.adapter = ComputerUseAdapterRef {
        kind: ComputerUseAdapterKind::OfficeWord,
        version: office_batch::DOCX_ADAPTER_VERSION.into(),
    };
    report
}

pub(super) fn mutation_readiness(
    ceiling: &ComputerUseSettings,
    session_ready: bool,
    session_reason: Option<ComputerUseReadinessReason>,
) -> ComputerUseCapabilityReadiness {
    let mut report = readiness(ceiling, session_ready, session_reason);
    report.capability = Capability::DocumentLivePatchConfirmed;
    report
}

impl ComputerUseBroker {
    pub(crate) fn inspect_word_batch(
        &self,
        params: &LiveDocumentInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<LiveDocumentInspectOutput, AgentError> {
        ensure_observation_enabled(ceiling)?;
        if !ceiling.office_semantic {
            return Err(word_error("Office batch observation is disabled"));
        }
        if params.target.is_some()
            || params.batch_file.is_none()
            || !(1024..=MAX_COMPUTER_USE_INSPECT_BYTES).contains(&params.max_bytes)
        {
            return Err(word_error(
                "Word batch observation requires a selected file and bounded output",
            ));
        }
        let desktop = observe_interactive_desktop()?;
        let incarnation = format!(
            "{}:{}",
            desktop.session_id,
            self.current_incarnation_nonce()
        );
        let file = params.batch_file.as_ref().expect("checked selected file");
        let observed =
            windows_word_batch::observe_document(file, (params.max_bytes as usize).min(64 * 1024))?;
        let source = observed.source();
        let after = observe_interactive_desktop()?;
        if after.session_id != desktop.session_id
            || incarnation != format!("{}:{}", after.session_id, self.current_incarnation_nonce())
        {
            return Err(word_error(
                "interactive session changed during Word observation",
            ));
        }
        let snapshot_id = self.next_snapshot_id();
        let document = self.issue_ref(
            &snapshot_id,
            &incarnation,
            ObjectKind::Document,
            ResolvedObject::WordBatch {
                source_file: file.clone(),
                source_sha256: source.sha256.clone(),
                source_byte_len: source.byte_len,
            },
        )?;
        let output = LiveDocumentInspectOutput {
            snapshot_id,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficeWord,
                version: office_batch::DOCX_ADAPTER_VERSION.into(),
            },
            projection: LiveDocumentProjection::Document {
                document,
                body_text: observed.projection().body_text.clone(),
                body_sha256: observed.body_sha256(),
            },
            batch_source: Some(source),
        };
        let bytes = serde_json::to_vec(&output)
            .map_err(|_| word_error("cannot encode Word batch projection"))?;
        if bytes.len() > params.max_bytes as usize {
            return Err(error(
                AgentErrorKind::OutputLimitExceeded,
                "Word batch projection exceeds the requested byte budget",
                false,
            ));
        }
        Ok(output)
    }

    pub(crate) fn prepare_word_copy(
        &self,
        target: &ObjectRef,
        action: DocumentLivePatchAction,
        ceiling: &ComputerUseSettings,
    ) -> Result<windows_word_batch::PreparedDocument, AgentError> {
        ensure_observation_enabled(ceiling)?;
        if !ceiling.office_semantic || target.object_kind != ObjectKind::Document {
            return Err(word_error(
                "Word copy requires an enabled batch adapter and an observed document",
            ));
        }
        let resolved = self.resolve_ref(target)?;
        let ResolvedObject::WordBatch {
            source_file,
            source_sha256,
            source_byte_len,
        } = &resolved
        else {
            return Err(word_error(
                "Word copy target is not a batch file observation",
            ));
        };
        let observed = windows_word_batch::observe_document(source_file, 64 * 1024)?;
        let source = observed.source();
        if source.sha256 != *source_sha256 || source.byte_len != *source_byte_len {
            return Err(word_error("selected Word source changed after observation"));
        }
        let prepared = observed.prepare_copy(action)?;
        if self.resolve_ref(target)? != resolved {
            return Err(word_error("Word target changed during copy preparation"));
        }
        Ok(prepared)
    }

    /// Exact-action admission and lease acquisition remain dispatcher duties.
    pub(crate) fn publish_word_copy_with_lease(
        &self,
        target: &ObjectRef,
        batch: &DocumentLiveBatchPatchAction,
        ceiling: &ComputerUseSettings,
        generation: &str,
    ) -> Result<CreatedTextArtifact, PublishFailure> {
        self.require_writer_lease(generation)
            .map_err(PublishFailure::NotCreated)?;
        if batch.output.destination_parent.object_kind != ObjectKind::Directory
            || !office_batch::valid_docx_leaf(&batch.output.native_file_name)
        {
            return Err(PublishFailure::NotCreated(word_error(
                "Word copy requires a selected output directory and safe DOCX name",
            )));
        }
        let prepared = self
            .prepare_word_copy(target, batch.action.clone(), ceiling)
            .map_err(PublishFailure::NotCreated)?;
        let _source = prepared.pin_source().map_err(PublishFailure::NotCreated)?;
        self.require_writer_lease(generation)
            .map_err(PublishFailure::NotCreated)?;
        let published = windows_publish::publish_docx(
            &batch.output.destination_parent,
            &batch.output.native_file_name,
            prepared.bytes(),
        )?;
        self.require_writer_lease(generation)
            .map_err(PublishFailure::OutcomeUnknown)?;
        Ok(published)
    }
}

fn word_error(message: &str) -> AgentError {
    error(AgentErrorKind::PermissionDenied, message, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn word_readiness_requires_consent_and_session_without_live_targets() {
        for enabled in [false, true] {
            for observe in [false, true] {
                for office_semantic in [false, true] {
                    for session_ready in [false, true] {
                        let ceiling = ComputerUseSettings {
                            enabled,
                            observe,
                            office_semantic,
                            ..Default::default()
                        };
                        let read = readiness(&ceiling, session_ready, None);
                        let write = mutation_readiness(&ceiling, session_ready, None);
                        assert!(read.supported && write.supported);
                        assert_eq!(
                            read.ready,
                            enabled && observe && office_semantic && session_ready
                        );
                        assert_eq!(write.ready, read.ready);
                        assert_eq!(read.reason, write.reason);
                        assert_eq!(read.reason.is_none(), read.ready);
                        assert!(
                            office_batch::is_docx(&read.adapter)
                                && office_batch::is_docx(&write.adapter)
                        );
                        assert_eq!(read.capability, Capability::DocumentLiveInspect);
                        assert_eq!(write.capability, Capability::DocumentLivePatchConfirmed);
                        if !enabled || !observe || !office_semantic {
                            assert_eq!(
                                read.reason,
                                Some(ComputerUseReadinessReason::DisabledByLocalCeiling)
                            );
                        } else if !session_ready {
                            assert_eq!(
                                read.reason,
                                Some(ComputerUseReadinessReason::NoInteractiveSession)
                            );
                        }
                    }
                }
            }
        }
        let broker = ComputerUseBroker::new();
        let report = broker.readiness(&ComputerUseSettings::default(), false, false);
        report.validate().unwrap();
        let reports =
            desk_diagnose_core::device_assistant::provider_readiness_reports(&report).unwrap();
        let word = reports
            .iter()
            .filter(|entry| {
                entry.provider_id == desk_diagnose_core::device_assistant::windows_word::PROVIDER_ID
            })
            .collect::<Vec<_>>();
        assert_eq!(word.len(), 2);
        assert!(word.iter().all(|entry| !entry.ready && !entry.enabled));
        assert!(reports.iter().all(|entry| entry.provider_id
            != desk_diagnose_core::device_assistant::DOCUMENT_LIVE_PROVIDER_ID));
        assert!(report.context_references.iter().all(|entry| !matches!(
            entry.capability,
            Capability::DocumentLiveInspect | Capability::DocumentLivePatchConfirmed
        )));
    }
}
