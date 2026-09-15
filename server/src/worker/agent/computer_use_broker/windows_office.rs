//! File-only PowerPoint observation behind the shared broker's object authority.
use super::*;
use desk_agent_protocol::computer_use::{
    LiveDocumentInspectOutput, LiveDocumentInspectParams, LiveDocumentProjection, office_batch,
};

pub(super) fn readiness(
    ceiling: &ComputerUseSettings,
    session_ready: bool,
    session_reason: Option<ComputerUseReadinessReason>,
) -> ComputerUseCapabilityReadiness {
    let enabled = ceiling.observation_enabled() && ceiling.office_semantic;
    let ready = enabled && session_ready;
    ComputerUseCapabilityReadiness {
        capability: Capability::PresentationLiveInspect,
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::OfficePowerPoint,
            version: office_batch::PPTX_ADAPTER_VERSION.into(),
        },
        supported: true,
        ready,
        reason: if !enabled {
            Some(ComputerUseReadinessReason::DisabledByLocalCeiling)
        } else if !session_ready {
            Some(session_reason.unwrap_or(ComputerUseReadinessReason::NoInteractiveSession))
        } else {
            None
        },
    }
}

pub(super) fn mutation_readiness(
    ceiling: &ComputerUseSettings,
    session_ready: bool,
    session_reason: Option<ComputerUseReadinessReason>,
) -> ComputerUseCapabilityReadiness {
    let mut report = readiness(ceiling, session_ready, session_reason);
    report.capability = Capability::PresentationLivePatchConfirmed;
    report
}

impl ComputerUseBroker {
    /// The dispatcher must have acquired the writer lease and exact grant.
    /// A publication failure retains its effect classification for the receipt.
    pub(crate) fn publish_powerpoint_copy_with_lease(
        &self,
        target: &ObjectRef,
        batch: &desk_agent_protocol::computer_use::PresentationLiveBatchPatchAction,
        ceiling: &ComputerUseSettings,
        generation: &str,
    ) -> Result<
        super::super::file_reference_store::CreatedTextArtifact,
        super::super::file_reference_store::windows_publish::PublishFailure,
    > {
        self.publish_powerpoint_copy_checked(target, batch, ceiling, || {
            self.require_writer_lease(generation).map(|_| ())
        })
    }

    #[cfg(test)]
    fn publish_powerpoint_copy(
        &self,
        target: &ObjectRef,
        batch: &desk_agent_protocol::computer_use::PresentationLiveBatchPatchAction,
        ceiling: &ComputerUseSettings,
    ) -> Result<
        super::super::file_reference_store::CreatedTextArtifact,
        super::super::file_reference_store::windows_publish::PublishFailure,
    > {
        self.publish_powerpoint_copy_checked(target, batch, ceiling, || Ok(()))
    }

    fn publish_powerpoint_copy_checked(
        &self,
        target: &ObjectRef,
        batch: &desk_agent_protocol::computer_use::PresentationLiveBatchPatchAction,
        ceiling: &ComputerUseSettings,
        guard: impl Fn() -> Result<(), AgentError>,
    ) -> Result<
        super::super::file_reference_store::CreatedTextArtifact,
        super::super::file_reference_store::windows_publish::PublishFailure,
    > {
        use super::super::file_reference_store::windows_publish::{self, PublishFailure};
        guard().map_err(PublishFailure::NotCreated)?;
        if batch.output.destination_parent.object_kind != ObjectKind::Directory
            || !office_batch::valid_pptx_leaf(&batch.output.native_file_name)
        {
            return Err(PublishFailure::NotCreated(error(
                AgentErrorKind::InvalidInput,
                "PowerPoint copy requires a selected output directory and safe PPTX name",
                false,
            )));
        }
        let prepared = self
            .prepare_powerpoint_copy(target, batch.action.clone(), ceiling)
            .map_err(PublishFailure::NotCreated)?;
        let _source = prepared.pin_source().map_err(PublishFailure::NotCreated)?;
        guard().map_err(PublishFailure::NotCreated)?;
        let published = windows_publish::publish_pptx(
            &batch.output.destination_parent,
            &batch.output.native_file_name,
            prepared.bytes(),
        )?;
        guard().map_err(PublishFailure::OutcomeUnknown)?;
        Ok(published)
    }

    /// Prepare bytes only after exact-action admission. Publication remains a
    /// separate effect and must not treat preparation as a completed mutation.
    pub(crate) fn prepare_powerpoint_copy(
        &self,
        target: &ObjectRef,
        action: desk_agent_protocol::computer_use::PresentationLivePatchAction,
        ceiling: &ComputerUseSettings,
    ) -> Result<super::super::windows_office_batch::PreparedPresentation, AgentError> {
        ensure_observation_enabled(ceiling)?;
        if !ceiling.office_semantic || target.object_kind != ObjectKind::Slide {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "PowerPoint copy requires an enabled batch adapter and an observed slide",
                false,
            ));
        }
        let resolved = self.resolve_ref(target)?;
        let ResolvedObject::PowerPointBatch {
            source_file,
            source_sha256,
            source_byte_len,
            slide_number,
        } = &resolved
        else {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "PowerPoint copy target is not a batch file observation",
                false,
            ));
        };
        let slide_number = usize::try_from(*slide_number).map_err(|_| {
            error(
                AgentErrorKind::InvalidInput,
                "invalid PowerPoint slide index",
                false,
            )
        })?;
        let observed = super::super::windows_office_batch::observe_presentation(
            source_file,
            slide_number,
            16_384,
        )?;
        let source = observed.source();
        if source.sha256 != *source_sha256 || source.byte_len != *source_byte_len {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "selected PowerPoint source changed after observation",
                false,
            ));
        }
        let prepared = observed.prepare_copy(action)?;
        if self.resolve_ref(target)? != resolved {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "PowerPoint target changed during copy preparation",
                false,
            ));
        }
        Ok(prepared)
    }

    pub(crate) fn inspect_powerpoint_batch(
        &self,
        params: &LiveDocumentInspectParams,
        ceiling: &ComputerUseSettings,
    ) -> Result<LiveDocumentInspectOutput, AgentError> {
        ensure_observation_enabled(ceiling)?;
        if !ceiling.office_semantic {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "Office batch observation is disabled in device-local settings",
                false,
            ));
        }
        if params.target.is_some()
            || params.batch_file.is_none()
            || !(1024..=MAX_COMPUTER_USE_INSPECT_BYTES).contains(&params.max_bytes)
        {
            return Err(error(
                AgentErrorKind::InvalidInput,
                "PowerPoint batch observation requires a selected file and bounded output",
                false,
            ));
        }
        let desktop = observe_interactive_desktop()?;
        let incarnation = format!(
            "{}:{}",
            desktop.session_id,
            self.current_incarnation_nonce()
        );
        let file = params.batch_file.as_ref().expect("checked selected file");
        // The shared inspection contract selects the first slide for file reads.
        let observed = super::super::windows_office_batch::observe_presentation(
            file,
            1,
            (params.max_bytes as usize).min(16_384),
        )?;
        let source = observed.source();
        let projection = observed.projection();
        let slide_number = i64::try_from(projection.slide_number).map_err(|_| {
            error(
                AgentErrorKind::InvalidInput,
                "PowerPoint slide index exceeds the protocol limit",
                false,
            )
        })?;
        let resolved = ResolvedObject::PowerPointBatch {
            source_file: file.clone(),
            source_sha256: source.sha256.clone(),
            source_byte_len: source.byte_len,
            slide_number,
        };
        let after = observe_interactive_desktop()?;
        if after.session_id != desktop.session_id
            || incarnation != format!("{}:{}", after.session_id, self.current_incarnation_nonce())
        {
            return Err(error(
                AgentErrorKind::PermissionDenied,
                "interactive session changed during PowerPoint observation",
                false,
            ));
        }
        let snapshot_id = self.next_snapshot_id();
        let presentation = self.issue_ref(
            &snapshot_id,
            &incarnation,
            ObjectKind::Presentation,
            resolved.clone(),
        )?;
        let slide = self.issue_ref(&snapshot_id, &incarnation, ObjectKind::Slide, resolved)?;
        let output = LiveDocumentInspectOutput {
            snapshot_id,
            adapter: ComputerUseAdapterRef {
                kind: ComputerUseAdapterKind::OfficePowerPoint,
                version: office_batch::PPTX_ADAPTER_VERSION.into(),
            },
            projection: LiveDocumentProjection::Presentation {
                presentation,
                slide,
                slide_number,
                title: projection.title.clone().unwrap_or_default(),
                presenter_notes: projection.presenter_notes.clone().unwrap_or_default(),
            },
            batch_source: Some(source),
        };
        let bytes = serde_json::to_vec(&output).map_err(|_| {
            error(
                AgentErrorKind::Internal,
                "cannot encode PowerPoint batch projection",
                true,
            )
        })?;
        if bytes.len() > params.max_bytes as usize {
            return Err(error(
                AgentErrorKind::OutputLimitExceeded,
                "PowerPoint batch projection exceeds the requested byte budget",
                false,
            ));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::file_reference_store;
    use super::*;
    #[test]
    fn pptx_readiness_requires_local_consent_and_current_session() {
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
                        let report = readiness(&ceiling, session_ready, None);
                        let mutation = mutation_readiness(&ceiling, session_ready, None);
                        assert_eq!(
                            mutation.capability,
                            Capability::PresentationLivePatchConfirmed
                        );
                        assert_eq!(mutation.ready, report.ready);
                        assert_eq!(mutation.reason, report.reason);
                        assert!(office_batch::is_pptx(&mutation.adapter));
                        assert!(report.supported);
                        assert!(office_batch::is_pptx(&report.adapter));
                        assert_eq!(
                            report.ready,
                            enabled && observe && office_semantic && session_ready
                        );
                        assert_eq!(report.reason.is_none(), report.ready);
                        if !enabled || !observe || !office_semantic {
                            assert_eq!(
                                report.reason,
                                Some(ComputerUseReadinessReason::DisabledByLocalCeiling)
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn pptx_broker_read_binds_file_slide_and_worker_without_live() {
        let _guard = file_reference_store::file_store_test_lock();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("演示 source.pptx");
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/office/title-notes.pptx"
        ));
        std::fs::write(&path, bytes).unwrap();
        let source = file_reference_store::issue(&path).unwrap();
        let broker = ComputerUseBroker::new();
        let ceiling = ComputerUseSettings {
            enabled: true,
            observe: true,
            office_semantic: true,
            ..Default::default()
        };
        let mut params: LiveDocumentInspectParams =
            serde_json::from_value(serde_json::json!({"batch_file":source,"max_bytes":16384}))
                .unwrap();
        let readiness = broker.readiness(&ceiling, false, false);
        let output = broker.inspect_powerpoint_batch(&params, &ceiling).unwrap();
        assert_eq!(output.batch_source.as_ref().unwrap().file, source);
        assert!(office_batch::is_pptx(&output.adapter));
        let LiveDocumentProjection::Presentation {
            presentation,
            slide,
            title,
            ..
        } = &output.projection
        else {
            panic!()
        };
        assert_eq!(title, "原始标题");
        assert_ne!(presentation.token, slide.token);
        assert_eq!(
            broker.resolve_ref(slide).unwrap(),
            broker.resolve_ref(presentation).unwrap()
        );
        assert!(
            matches!(broker.resolve_ref(slide).unwrap(), ResolvedObject::PowerPointBatch { source_file, slide_number: 1, .. } if source_file == source)
        );
        let action =
            desk_agent_protocol::computer_use::PresentationLivePatchAction::ReplaceSlideTitle {
                text: "Updated title".into(),
            };
        let prepared = broker
            .prepare_powerpoint_copy(slide, action.clone(), &ceiling)
            .unwrap();
        let readback =
            desk_office_batch::pptx_inspect::inspect(prepared.bytes(), 1, 16384).unwrap();
        assert_eq!(readback.title.as_deref(), Some("Updated title"));
        let output_dir = tempfile::tempdir().unwrap();
        let batch = desk_agent_protocol::computer_use::PresentationLiveBatchPatchAction {
            output: desk_agent_protocol::computer_use::BatchDocumentOutput {
                destination_parent: file_reference_store::issue(output_dir.path()).unwrap(),
                native_file_name: "copy.pptx".into(),
            },
            action: action.clone(),
        };
        assert!(matches!(
            broker.publish_powerpoint_copy_with_lease(slide, &batch, &ceiling, "no-active-lease"),
            Err(file_reference_store::windows_publish::PublishFailure::NotCreated(_))
        ));
        assert!(!output_dir.path().join("copy.pptx").exists());
        broker
            .acquire_writer_lease(WriterLeaseRequest {
                scope: WriterLeaseScope::InteractiveSession,
                work_id: "pptx-work".into(),
                action_request_id: "pptx-call".into(),
                execution_generation: "pptx-generation".into(),
                approved_actor_id: "owner".into(),
                interactive_session_incarnation: readiness.interactive_session_incarnation,
                expires_at: Utc::now() + Duration::seconds(30),
            })
            .unwrap();
        assert!(matches!(
            broker.publish_powerpoint_copy_with_lease(slide, &batch, &ceiling, "other-generation"),
            Err(file_reference_store::windows_publish::PublishFailure::NotCreated(_))
        ));
        assert!(!output_dir.path().join("copy.pptx").exists());
        let published = broker
            .publish_powerpoint_copy_with_lease(slide, &batch, &ceiling, "pptx-generation")
            .unwrap();
        assert!(broker.release_writer_lease("pptx-generation"));
        let mut released_batch = batch.clone();
        released_batch.output.native_file_name = "released.pptx".into();
        assert!(matches!(
            broker.publish_powerpoint_copy_with_lease(
                slide,
                &released_batch,
                &ceiling,
                "pptx-generation"
            ),
            Err(file_reference_store::windows_publish::PublishFailure::NotCreated(_))
        ));
        assert!(!output_dir.path().join("released.pptx").exists());
        for fail_at in 1..=3 {
            let mut guarded_batch = batch.clone();
            guarded_batch.output.native_file_name = format!("guard-{fail_at}.pptx");
            let calls = std::cell::Cell::new(0);
            let result =
                broker.publish_powerpoint_copy_checked(slide, &guarded_batch, &ceiling, || {
                    calls.set(calls.get() + 1);
                    if calls.get() == fail_at {
                        Err(error(AgentErrorKind::PermissionDenied, "lease lost", false))
                    } else {
                        Ok(())
                    }
                });
            assert_eq!(
                matches!(
                    result,
                    Err(file_reference_store::windows_publish::PublishFailure::OutcomeUnknown(_))
                ),
                fail_at == 3
            );
            assert!(result.is_err());
            assert_eq!(
                output_dir
                    .path()
                    .join(&guarded_batch.output.native_file_name)
                    .exists(),
                fail_at == 3
            );
        }
        let published_bytes =
            file_reference_store::read_verified_bytes(&published.file, published.byte_len).unwrap();
        assert_eq!(published_bytes.sha256, prepared.sha256());
        assert_eq!(
            desk_office_batch::pptx_inspect::inspect(&published_bytes.bytes, 1, 16384)
                .unwrap()
                .title
                .as_deref(),
            Some("Updated title")
        );
        assert!(matches!(
            broker.publish_powerpoint_copy(slide, &batch, &ceiling),
            Err(file_reference_store::windows_publish::PublishFailure::NotCreated(_))
        ));
        assert_eq!(std::fs::read_dir(output_dir.path()).unwrap().count(), 2);
        let receipt = super::super::super::windows_office_batch::publication_receipt(
            "work".into(),
            "call".into(),
            "generation".into(),
            Ok(published),
        );
        let batch_action =
            desk_agent_protocol::computer_use::ComputerActionKind::PresentationLiveBatch(
                batch.clone(),
            );
        let now = Utc::now().timestamp_millis() as u64;
        desk_diagnose_core::provider_preflight::batch_document::validate_pptx_completion(
            &output.adapter,
            &batch_action,
            &receipt,
            now,
        )
        .unwrap();
        for failure in [
            file_reference_store::windows_publish::PublishFailure::NotCreated(error(
                AgentErrorKind::InvalidInput,
                "collision",
                false,
            )),
            file_reference_store::windows_publish::PublishFailure::OutcomeUnknown(error(
                AgentErrorKind::Internal,
                "private native detail",
                false,
            )),
        ] {
            let unknown = matches!(
                failure,
                file_reference_store::windows_publish::PublishFailure::OutcomeUnknown(_)
            );
            let receipt = super::super::super::windows_office_batch::publication_receipt(
                "work".into(),
                "call".into(),
                "generation".into(),
                Err(failure),
            );
            assert_eq!(
                receipt.result
                    == desk_agent_protocol::computer_use::ComputerActionResultClass::OutcomeUnknown,
                unknown
            );
            assert!(receipt.output.is_none());
            assert!(
                !receipt
                    .message
                    .as_ref()
                    .unwrap()
                    .contains("private native detail")
            );
            desk_diagnose_core::provider_preflight::batch_document::validate_pptx_completion(
                &output.adapter,
                &batch_action,
                &receipt,
                now,
            )
            .unwrap();
        }
        assert!(
            broker
                .prepare_powerpoint_copy(presentation, action.clone(), &ceiling)
                .is_err()
        );
        std::fs::write(&path, b"source changed after read").unwrap();
        let mut stale_batch = batch.clone();
        stale_batch.output.native_file_name = "stale.pptx".into();
        assert!(matches!(
            broker.publish_powerpoint_copy(slide, &stale_batch, &ceiling),
            Err(file_reference_store::windows_publish::PublishFailure::NotCreated(_))
        ));
        assert!(!output_dir.path().join("stale.pptx").exists());
        assert!(
            broker
                .prepare_powerpoint_copy(slide, action.clone(), &ceiling)
                .is_err()
        );
        std::fs::write(&path, bytes).unwrap();
        let mut disabled = ceiling.clone();
        disabled.office_semantic = false;
        assert!(
            broker
                .prepare_powerpoint_copy(slide, action.clone(), &disabled)
                .is_err()
        );
        assert!(broker.inspect_powerpoint_batch(&params, &disabled).is_err());
        params.target = Some(slide.clone());
        assert!(broker.inspect_powerpoint_batch(&params, &ceiling).is_err());
        params.batch_file = None;
        assert!(broker.inspect_powerpoint_batch(&params, &ceiling).is_err());
        broker.reset_worker_incarnation();
        assert!(broker.resolve_ref(slide).is_err());
        assert!(
            broker
                .prepare_powerpoint_copy(slide, action, &ceiling)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
