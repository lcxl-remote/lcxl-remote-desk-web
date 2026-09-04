use super::*;
use crate::remote_tool_edge::completion::project;
use desk_agent_protocol::{communication::*, computer_use::*, data_lineage::ContentRef};

fn file_ref(plan: &SealedComputerActionPlan, kind: ObjectKind) -> ObjectRef {
    ObjectRef {
        token: "result-token".into(),
        snapshot_id: "result-snapshot".into(),
        object_kind: kind,
        expires_at: plan.expires_at.clone(),
    }
}

#[tokio::test]
async fn file_and_iwork_projections_require_exact_artifact_type_leaf_and_verified_fact() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("artifacts.db")).await).await;
    for family in ["text", "numbers", "pages", "keynote"] {
        let mut plan = f.plan.clone();
        let name = match family {
            "text" => "copy.txt",
            "numbers" => "copy.numbers",
            "pages" => "copy.pages",
            _ => "copy.key",
        };
        let destination = BatchDocumentOutput {
            destination_parent: file_ref(&plan, ObjectKind::Directory),
            native_file_name: name.into(),
        };
        let (kind, target, action) = match family {
            "text" => (
                ComputerUseAdapterKind::FileSystem,
                ObjectKind::Directory,
                ComputerActionKind::File(FilePatchAction::CreateTextArtifact {
                    file_name: name.into(),
                    content_utf8: "hello".into(),
                }),
            ),
            "numbers" => (
                ComputerUseAdapterKind::IworkNumbers,
                ObjectKind::Range,
                ComputerActionKind::SpreadsheetLiveBatch(SpreadsheetLiveBatchPatchAction {
                    output: destination,
                    action: SpreadsheetLivePatchAction::SetCellValue {
                        value: "new".into(),
                    },
                }),
            ),
            "pages" => (
                ComputerUseAdapterKind::IworkPages,
                ObjectKind::Document,
                ComputerActionKind::DocumentLiveBatch(DocumentLiveBatchPatchAction {
                    output: destination,
                    action: DocumentLivePatchAction::ReplaceBodyText { text: "new".into() },
                }),
            ),
            _ => (
                ComputerUseAdapterKind::IworkKeynote,
                ObjectKind::Slide,
                ComputerActionKind::PresentationLiveBatch(PresentationLiveBatchPatchAction {
                    output: destination,
                    action: PresentationLivePatchAction::ReplaceSlideTitle { text: "new".into() },
                }),
            ),
        };
        plan.adapter.kind = kind;
        plan.actions[0].target.object_kind = target;
        plan.actions[0].action = action;
        plan.validate().unwrap();
        let file = file_ref(&plan, ObjectKind::File);
        let output = if family == "text" {
            ComputerActionOutput::FileArtifact(CreatedFileArtifactOutput {
                file: file.clone(),
                file_name: name.into(),
                media_type: "text/plain".into(),
                size_bytes: 5,
                digest_sha256: "a".repeat(64),
                content: ContentRef::Artifact {
                    artifact_id: file.token,
                    sha256: "a".repeat(64),
                    size_bytes: 5,
                    media_type: "text/plain".into(),
                },
            })
        } else {
            ComputerActionOutput::BatchDocumentArtifact(BatchDocumentArtifact {
                file,
                file_name: name.into(),
                byte_len: 5,
                sha256: "a".repeat(64),
                validation_byte_len: 9,
                validation_sha256: "b".repeat(64),
            })
        };
        let mut native = verified(&plan);
        native.output = Some(output);
        let projected = project(&plan, "artifact-tool", "run-1", "{}", &native)
            .unwrap()
            .unwrap();
        assert_eq!(projected.outcome, CapabilityDispatchOutcome::Succeeded);
        assert_eq!(
            serde_json::from_str::<ComputerActionCompleted>(&projected.content).unwrap(),
            native
        );
        for fault in [
            "no_fact",
            "no_change",
            "wrong_leaf",
            "wrong_type",
            "invalid_digest",
        ] {
            let mut bad = native.clone();
            match fault {
                "no_fact" => bad.facts.clear(),
                "no_change" => bad.facts[0].changed = false,
                "wrong_type" => bad.output = verified(&f.plan).output,
                _ => match bad.output.as_mut().unwrap() {
                    ComputerActionOutput::FileArtifact(a) => {
                        if fault == "wrong_leaf" {
                            a.file_name = "other.txt".into();
                        } else {
                            a.digest_sha256 = "bad".into();
                        }
                    }
                    ComputerActionOutput::BatchDocumentArtifact(a) => {
                        if fault == "wrong_leaf" {
                            a.file_name = "other.pages".into();
                        } else {
                            a.validation_sha256 = "bad".into();
                        }
                    }
                    _ => unreachable!(),
                },
            }
            assert!(
                project(&plan, "artifact-tool", "run-1", "{}", &bad).is_err(),
                "{family}/{fault}"
            );
        }
    }
}

fn draft() -> LocalDraftDocument {
    LocalDraftDocument {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        recipients: vec![LocalDraftRecipient {
            role: RecipientRole::To,
            address: "alice@example.test".into(),
            display_name: None,
        }],
        subject: "draft".into(),
        body_plain_text: "body".into(),
        attachment_labels: vec![],
    }
}

#[tokio::test]
async fn outlook_projection_preserves_assistive_manual_only_handoff_and_original_request_hash() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("outlook.db")).await).await;
    let mut plan = f.plan.clone();
    let request = OutlookNewComposeHandoffRequest {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        call_id: plan.action_request_id.clone(),
        run_id: "run-1".into(),
        draft: draft(),
        surface: CommunicationSurfaceRef {
            channel: CommunicationChannel::Email,
            kind: CommunicationSurfaceKind::OutlookNewDesktop,
            scope: CommunicationSurfaceScope::DesktopApplication {
                application_id: "outlook".into(),
            },
            device_id: plan.device_id.clone(),
            os_session_id: "desktop-1".into(),
            adapter_id: "mailto".into(),
            adapter_version: "1".into(),
            profile_id: "profile-1".into(),
            account_id: "unverified".into(),
            revision: 1,
        },
    };
    request.validate().unwrap();
    let handoff = CommunicationDraftHandoff {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        handoff_id: "handoff-1".into(),
        run_id: "run-1".into(),
        surface: request.surface.clone(),
        compose_id: "compose-1".into(),
        prepared_payload_sha256: format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&request).unwrap())
        ),
        verification: CommunicationPrepareVerification::AssistiveUnverified,
        readback_payload_sha256: None,
        send_authority: CommunicationSendAuthority::ManualOnly,
        send_payload_snapshot: None,
        handed_off_at_unix_ms: 1000,
    };
    plan.adapter.kind = ComputerUseAdapterKind::OutlookNewMailto;
    plan.actions[0].target.object_kind = ObjectKind::Application;
    plan.actions[0].action = ComputerActionKind::Communication(request);
    let native = ComputerActionCompleted {
        result: ComputerActionResultClass::ChangedButUnverified,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed: true,
            verified: false,
            summary: "manual handoff".into(),
        }],
        output: Some(ComputerActionOutput::CommunicationHandoff(handoff.clone())),
        ..failed(&plan)
    };
    let projected = project(
        &plan,
        "prepare_outlook_new_draft_handoff",
        "run-1",
        "{}",
        &native,
    )
    .unwrap()
    .unwrap();
    assert_eq!(projected.outcome, CapabilityDispatchOutcome::Succeeded);
    assert_eq!(
        serde_json::from_str::<CommunicationDraftHandoff>(&projected.content).unwrap(),
        handoff
    );
    for fault in ["run", "digest", "surface", "claimed_verified", "readback"] {
        let mut bad = native.clone();
        if fault == "claimed_verified" {
            bad.result = ComputerActionResultClass::Verified;
            bad.facts[0].verified = true;
        } else if let Some(ComputerActionOutput::CommunicationHandoff(h)) = &mut bad.output {
            match fault {
                "run" => h.run_id = "other".into(),
                "digest" => h.prepared_payload_sha256 = "f".repeat(64),
                "surface" => h.surface.account_id = "other".into(),
                "readback" => h.readback_payload_sha256 = Some(h.prepared_payload_sha256.clone()),
                _ => unreachable!(),
            }
        }
        assert!(
            project(
                &plan,
                "prepare_outlook_new_draft_handoff",
                "run-1",
                "{}",
                &bad
            )
            .is_err(),
            "{fault}"
        );
    }
}

#[tokio::test]
async fn gmail_and_slack_projection_require_exact_readback_and_never_enable_send() {
    use desk_agent_protocol::browser_control::*;
    let dir = tempfile::tempdir().unwrap();
    let f = Fixture::new(file_db(&dir.path().join("web-drafts.db")).await).await;
    for gmail in [false, true] {
        let mut plan = f.plan.clone();
        let Some(ComputerActionOutput::Browser(base)) = verified(&plan).output else {
            unreachable!()
        };
        let mut page = base.page;
        page.origin.host_ascii = if gmail {
            "mail.google.com"
        } else {
            "app.slack.com"
        }
        .into();
        let field = |id: &str| BrowserElementRef {
            page_id: page.page_id.clone(),
            page_incarnation: page.page_incarnation.clone(),
            document_revision: page.document_revision,
            element_id: id.into(),
            role: BrowserElementRole::Textbox,
            accessible_name: id.into(),
            value: None,
            element_revision: 1,
        };
        let input = if gmail {
            serde_json::to_value(GmailWebDraftHandoffInput {
                schema_version: COMMUNICATION_SCHEMA_VERSION,
                page: page.clone(),
                to_field: field("to"),
                subject_field: field("subject"),
                body_field: field("body"),
                attachment: None,
                draft: draft(),
            })
            .unwrap()
        } else {
            serde_json::to_value(SlackWebDraftHandoffInput {
                schema_version: COMMUNICATION_SCHEMA_VERSION,
                page: page.clone(),
                composer: field("composer"),
                body_plain_text: "body".into(),
            })
            .unwrap()
        };
        let name = if gmail {
            "prepare_gmail_web_draft_handoff"
        } else {
            "prepare_slack_web_message_handoff"
        };
        let call = ToolCall {
            id: "model".into(),
            name: name.into(),
            arguments_json: input.to_string(),
        };
        let request = desk_diagnose_core::provider_preflight::browser_action_from_call(
            &call,
            &plan.action_request_id,
        )
        .unwrap();
        let BrowserAction::FillForm { fields, .. } = &request.action else {
            panic!("form fill required")
        };
        let readback = fields
            .iter()
            .map(|field| BrowserFormFieldReadback {
                request_element_id: field.element.element_id.clone(),
                request_role: field.element.role,
                request_accessible_name: field.element.accessible_name.clone(),
                source_element_id: field.element.element_id.clone(),
                container_element_id: Some("compose".into()),
                kind: BrowserFormReadbackKind::ControlValue,
                value: field.value.clone(),
            })
            .collect();
        plan.actions[0].action = ComputerActionKind::Browser(request);
        plan.draft_hash = format!("{:x}", Sha256::digest(call.arguments_json.as_bytes()));
        page.document_revision += 1;
        let result = BrowserActionResult {
            schema_version: 1,
            call_id: plan.action_request_id.clone(),
            outcome: BrowserActionOutcome::FormFilled,
            page: page.clone(),
            snapshot: Some(BrowserSemanticSnapshot {
                schema_version: 1,
                page,
                elements: vec![],
                truncated: false,
                captured_at_unix_ms: 1001,
            }),
            form_readback: readback,
            send_receipt: None,
            completed_at_unix_ms: 1002,
        };
        let native = ComputerActionCompleted {
            output: Some(ComputerActionOutput::Browser(result)),
            ..verified(&plan)
        };
        let projected = project(&plan, name, "run-1", &call.arguments_json, &native)
            .unwrap()
            .unwrap();
        let handoff: CommunicationDraftHandoff = serde_json::from_str(&projected.content).unwrap();
        assert_eq!(
            handoff.verification,
            CommunicationPrepareVerification::SemanticExact
        );
        assert_eq!(
            handoff.send_authority,
            CommunicationSendAuthority::ManualOnly
        );
        assert_eq!(handoff.prepared_payload_sha256, plan.draft_hash);
        for fault in ["value", "page", "revision"] {
            let mut bad = native.clone();
            let Some(ComputerActionOutput::Browser(b)) = &mut bad.output else {
                unreachable!()
            };
            match fault {
                "value" => b.form_readback[0].value = "wrong".into(),
                "page" => b.page.page_incarnation = "wrong".into(),
                "revision" => b.page.document_revision -= 1,
                _ => unreachable!(),
            };
            assert!(
                project(&plan, name, "run-1", &call.arguments_json, &bad).is_err(),
                "{gmail}/{fault}"
            );
        }
    }
}
