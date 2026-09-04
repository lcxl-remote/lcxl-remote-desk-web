//! Exact communication handoff and reviewed-send receipt projection shared by orchestrators.

use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    browser_control::{
        BrowserActionOutcome, BrowserActionResult, BrowserElementRef, BrowserEngineKind,
        BrowserFormFieldReadback, BrowserFormReadbackKind,
    },
    communication::{
        COMMUNICATION_SCHEMA_VERSION, CommunicationChannel, CommunicationDraftHandoff,
        CommunicationPayload, CommunicationPrepareVerification, CommunicationSendAuthority,
        CommunicationSurfaceKind, CommunicationSurfaceRef, CommunicationSurfaceScope,
        GmailWebDraftHandoffInput, GmailWebExactSendInput, ImmutableAttachmentSnapshot,
        ImmutableBodySnapshot, RecipientIdentity, RecipientKind, RecipientRole, SendOutcome,
        SendPayloadSnapshot, SendReceipt, SlackWebDraftHandoffInput, SlackWebExactSendInput,
    },
    computer_use::{ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass},
    data_lineage::ContentRef,
};
use sha2::{Digest, Sha256};

fn invalid() -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: "invalid original communication handoff result".into(),
        retryable: false,
        safe_for_model: false,
        error_code: None,
    }
}

fn exact_field_readback<'a>(
    result: &'a BrowserActionResult,
    expected: &BrowserElementRef,
    value: &str,
) -> Option<&'a BrowserFormFieldReadback> {
    let mut matching = result.form_readback.iter().filter(|readback| {
        readback.request_element_id == expected.element_id
            && readback.request_role == expected.role
            && readback.request_accessible_name == expected.accessible_name
            && readback.value == value
    });
    let matched = matching.next()?;
    matching.next().is_none().then_some(matched)
}

fn gmail_exact_form_readback(
    result: &BrowserActionResult,
    gmail: &GmailWebDraftHandoffInput,
) -> bool {
    if result.form_readback.len() != 3 {
        return false;
    }
    let Some(to) = exact_field_readback(
        result,
        &gmail.to_field,
        gmail.draft.recipients[0].address.as_str(),
    ) else {
        return false;
    };
    let Some(subject) =
        exact_field_readback(result, &gmail.subject_field, gmail.draft.subject.as_str())
    else {
        return false;
    };
    let Some(body) = exact_field_readback(
        result,
        &gmail.body_field,
        gmail.draft.body_plain_text.as_str(),
    ) else {
        return false;
    };
    if subject.kind != BrowserFormReadbackKind::ControlValue
        || body.kind != BrowserFormReadbackKind::ControlValue
    {
        return false;
    }
    let Some(container) = to.container_element_id.as_deref() else {
        return false;
    };
    subject.container_element_id.as_deref() == Some(container)
        && body.container_element_id.as_deref() == Some(container)
        && matches!(
            to.kind,
            BrowserFormReadbackKind::ControlValue | BrowserFormReadbackKind::CommittedText
        )
}

fn gmail_exact_attachment_readback(
    result: &BrowserActionResult,
    gmail: &GmailWebDraftHandoffInput,
) -> bool {
    let Some(attachment) = &gmail.attachment else {
        return result.outcome == BrowserActionOutcome::FormFilled;
    };
    result.outcome == BrowserActionOutcome::FormFilledWithFile
        && result.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.elements.iter().any(|element| {
                element.accessible_name == attachment.artifact.file_name
                    || element.value.as_deref() == Some(attachment.artifact.file_name.as_str())
            })
        })
}

fn slack_exact_form_readback(
    result: &BrowserActionResult,
    slack: &SlackWebDraftHandoffInput,
) -> bool {
    result.form_readback.len() == 1
        && exact_field_readback(result, &slack.composer, slack.body_plain_text.as_str())
            .is_some_and(|readback| readback.kind == BrowserFormReadbackKind::ControlValue)
}

fn communication_surface_kind(engine: BrowserEngineKind) -> CommunicationSurfaceKind {
    match engine {
        BrowserEngineKind::ChromeExtension => CommunicationSurfaceKind::ChromeExtension,
        BrowserEngineKind::ChromeDevtoolsMcp => CommunicationSurfaceKind::ChromeDevtoolsMcp,
    }
}

fn immutable_plain_text_body(value: &str) -> ImmutableBodySnapshot {
    let digest_sha256 = format!("{:x}", Sha256::digest(value.as_bytes()));
    ImmutableBodySnapshot {
        content: ContentRef::ImmutableBlob {
            blob_id: format!("communication-body-{digest_sha256}"),
            sha256: digest_sha256.clone(),
            size_bytes: value.len() as u64,
            media_type: "text/plain; charset=utf-8".into(),
        },
        media_type: "text/plain; charset=utf-8".into(),
        size_bytes: value.len() as u64,
        digest_sha256,
    }
}

fn exact_send_snapshot(
    snapshot_id: String,
    run_id: &str,
    payload: CommunicationPayload,
    sealed_at_unix_ms: u64,
) -> Result<SendPayloadSnapshot, AgentError> {
    crate::communication::seal_send_payload(snapshot_id, run_id.into(), payload, sealed_at_unix_ms)
        .map_err(|_| invalid())
}

/// Project a verified reviewed-site Browser completion into a manual-only
/// communication handoff. `None` means the tool is not a Web handoff.
pub fn project_web_draft_handoff(
    tool_name: &str,
    run_id: &str,
    canonical_input: &str,
    canonical_input_digest_sha256: &str,
    completion: &ComputerActionCompleted,
) -> Result<Option<CommunicationDraftHandoff>, AgentError> {
    if !matches!(
        tool_name,
        "prepare_gmail_web_draft_handoff" | "prepare_slack_web_message_handoff"
    ) {
        return Ok(None);
    }
    if run_id.trim().is_empty()
        || canonical_input_digest_sha256.len() != 64
        || !canonical_input_digest_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || completion.result != ComputerActionResultClass::Verified
        || completion.facts.len() != 1
        || !completion.facts[0].changed
        || !completion.facts[0].verified
    {
        return Err(invalid());
    }
    let Some(ComputerActionOutput::Browser(result)) = &completion.output else {
        return Err(invalid());
    };
    result.validate().map_err(|_| invalid())?;
    if result.call_id != completion.action_request_id {
        return Err(invalid());
    }
    if tool_name == "prepare_gmail_web_draft_handoff" {
        let gmail: GmailWebDraftHandoffInput =
            serde_json::from_str(canonical_input).map_err(|_| invalid())?;
        gmail.validate().map_err(|_| invalid())?;
        if !gmail_exact_attachment_readback(result, &gmail)
            || result.page.adapter != gmail.page.adapter
            || result.page.page_id != gmail.page.page_id
            || result.page.page_incarnation != gmail.page.page_incarnation
            || result.page.origin != gmail.page.origin
            || result.page.document_revision <= gmail.page.document_revision
            || !gmail_exact_form_readback(result, &gmail)
        {
            return Err(invalid());
        }
        let compose_digest = format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{}:{}:{}:{}:{}",
                    result.page.page_incarnation,
                    gmail.to_field.element_id,
                    gmail.subject_field.element_id,
                    gmail.body_field.element_id,
                    completion.action_request_id
                )
                .as_bytes(),
            )
        );
        let surface = CommunicationSurfaceRef {
            channel: CommunicationChannel::Email,
            kind: communication_surface_kind(result.page.adapter.engine),
            scope: CommunicationSurfaceScope::WebOrigin {
                origin: result.page.origin.clone(),
            },
            device_id: result.page.adapter.device_id.clone(),
            os_session_id: result.page.adapter.os_session_id.clone(),
            adapter_id: crate::device_assistant::GMAIL_WEB_ADAPTER_ID.into(),
            adapter_version: crate::device_assistant::GMAIL_WEB_ADAPTER_VERSION.into(),
            profile_id: result.page.adapter.profile_incarnation.clone(),
            account_id: crate::device_assistant::GMAIL_WEB_CURRENT_PROFILE_ACCOUNT_ID.into(),
            revision: result.page.adapter.connection_revision,
        };
        let exact_send_eligible = surface.kind == CommunicationSurfaceKind::ChromeExtension;
        let canonical_recipient =
            crate::communication::canonicalize_email_address(&gmail.draft.recipients[0].address)
                .map_err(|_| invalid())?;
        let send_payload_snapshot = exact_send_eligible
            .then(|| {
                exact_send_snapshot(
                    format!("gmail-send-{compose_digest}"),
                    run_id,
                    CommunicationPayload {
                        surface: surface.clone(),
                        recipients: vec![RecipientIdentity {
                            role: RecipientRole::To,
                            kind: RecipientKind::EmailMailbox,
                            stable_id: format!(
                                "gmail-mailbox-{:x}",
                                Sha256::digest(canonical_recipient.value.as_bytes())
                            ),
                            canonical_address: canonical_recipient.value,
                            display_name: gmail.draft.recipients[0].display_name.clone(),
                            display_warnings: canonical_recipient.display_warnings,
                            resolved_members: Vec::new(),
                            member_snapshot_sha256: None,
                        }],
                        subject: gmail.draft.subject.clone(),
                        body: immutable_plain_text_body(&gmail.draft.body_plain_text),
                        attachments: gmail
                            .attachment
                            .as_ref()
                            .map(|attachment| ImmutableAttachmentSnapshot {
                                content: attachment.artifact.content.clone(),
                                file_name: attachment.artifact.file_name.clone(),
                                media_type: attachment.artifact.media_type.clone(),
                                size_bytes: attachment.artifact.size_bytes,
                                digest_sha256: attachment.artifact.digest_sha256.clone(),
                            })
                            .into_iter()
                            .collect(),
                    },
                    result.completed_at_unix_ms,
                )
            })
            .transpose()?;
        let handoff = CommunicationDraftHandoff {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff_id: format!("gmail-handoff-{compose_digest}"),
            run_id: run_id.into(),
            surface,
            compose_id: format!("gmail-compose-{compose_digest}"),
            prepared_payload_sha256: canonical_input_digest_sha256.into(),
            verification: CommunicationPrepareVerification::SemanticExact,
            readback_payload_sha256: Some(canonical_input_digest_sha256.into()),
            send_authority: if exact_send_eligible {
                CommunicationSendAuthority::ExactGrantEligible
            } else {
                CommunicationSendAuthority::ManualOnly
            },
            send_payload_snapshot,
            handed_off_at_unix_ms: result.completed_at_unix_ms,
        };
        handoff.validate().map_err(|_| invalid())?;
        return Ok(Some(handoff));
    }

    let slack: SlackWebDraftHandoffInput =
        serde_json::from_str(canonical_input).map_err(|_| invalid())?;
    slack.validate().map_err(|_| invalid())?;
    if result.outcome != BrowserActionOutcome::FormFilled
        || result.page.adapter != slack.page.adapter
        || result.page.page_id != slack.page.page_id
        || result.page.page_incarnation != slack.page.page_incarnation
        || result.page.origin != slack.page.origin
        || result.page.document_revision <= slack.page.document_revision
        || !slack_exact_form_readback(result, &slack)
    {
        return Err(invalid());
    }
    let compose_digest = format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{}:{}:{}",
                result.page.page_incarnation,
                slack.composer.element_id,
                completion.action_request_id
            )
            .as_bytes(),
        )
    );
    let surface = CommunicationSurfaceRef {
        channel: CommunicationChannel::Chat,
        kind: communication_surface_kind(result.page.adapter.engine),
        scope: CommunicationSurfaceScope::WebOrigin {
            origin: result.page.origin.clone(),
        },
        device_id: result.page.adapter.device_id.clone(),
        os_session_id: result.page.adapter.os_session_id.clone(),
        adapter_id: crate::device_assistant::SLACK_WEB_ADAPTER_ID.into(),
        adapter_version: crate::device_assistant::SLACK_WEB_ADAPTER_VERSION.into(),
        profile_id: result.page.adapter.profile_incarnation.clone(),
        account_id: crate::device_assistant::SLACK_WEB_CURRENT_PROFILE_ACCOUNT_ID.into(),
        revision: result.page.adapter.connection_revision,
    };
    let exact_send_eligible = surface.kind == CommunicationSurfaceKind::ChromeExtension;
    let destination = slack.composer.accessible_name.trim().to_string();
    let send_payload_snapshot = exact_send_eligible
        .then(|| {
            exact_send_snapshot(
                format!("slack-send-{compose_digest}"),
                run_id,
                CommunicationPayload {
                    surface: surface.clone(),
                    recipients: vec![RecipientIdentity {
                        role: RecipientRole::ChatDestination,
                        kind: RecipientKind::ChatChannel,
                        stable_id: format!(
                            "slack-destination-{:x}",
                            Sha256::digest(
                                format!("{}:{destination}", surface.profile_id).as_bytes()
                            )
                        ),
                        canonical_address: destination,
                        display_name: None,
                        display_warnings: Vec::new(),
                        resolved_members: Vec::new(),
                        member_snapshot_sha256: None,
                    }],
                    subject: String::new(),
                    body: immutable_plain_text_body(&slack.body_plain_text),
                    attachments: Vec::new(),
                },
                result.completed_at_unix_ms,
            )
        })
        .transpose()?;
    let handoff = CommunicationDraftHandoff {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        handoff_id: format!("slack-handoff-{compose_digest}"),
        run_id: run_id.into(),
        surface,
        compose_id: format!("slack-compose-{compose_digest}"),
        prepared_payload_sha256: canonical_input_digest_sha256.into(),
        verification: CommunicationPrepareVerification::SemanticExact,
        readback_payload_sha256: Some(canonical_input_digest_sha256.into()),
        send_authority: if exact_send_eligible {
            CommunicationSendAuthority::ExactGrantEligible
        } else {
            CommunicationSendAuthority::ManualOnly
        },
        send_payload_snapshot,
        handed_off_at_unix_ms: result.completed_at_unix_ms,
    };
    handoff.validate().map_err(|_| invalid())?;
    Ok(Some(handoff))
}

/// Project the reviewed extension's bounded exact-send result into the stable
/// communication receipt contract. An unknown receipt remains unknown and is
/// never converted into a retryable failure.
pub fn project_web_send_receipt(
    tool_name: &str,
    canonical_input: &str,
    completion: &ComputerActionCompleted,
) -> Result<Option<SendReceipt>, AgentError> {
    if !matches!(tool_name, "send_gmail_web_exact" | "send_slack_web_exact") {
        return Ok(None);
    }
    let snapshot = if tool_name == "send_gmail_web_exact" {
        let input: GmailWebExactSendInput =
            serde_json::from_str(canonical_input).map_err(|_| invalid())?;
        crate::communication::verify_gmail_web_exact_send_input(&input).map_err(|_| invalid())?;
        input.handoff.send_payload_snapshot.ok_or_else(invalid)?
    } else {
        let input: SlackWebExactSendInput =
            serde_json::from_str(canonical_input).map_err(|_| invalid())?;
        crate::communication::verify_slack_web_exact_send_input(&input).map_err(|_| invalid())?;
        input.handoff.send_payload_snapshot.ok_or_else(invalid)?
    };
    let Some(ComputerActionOutput::Browser(result)) = &completion.output else {
        return Err(invalid());
    };
    result.validate().map_err(|_| invalid())?;
    let receipt = result.send_receipt.clone().ok_or_else(invalid)?;
    receipt.validate().map_err(|_| invalid())?;
    if result.outcome != BrowserActionOutcome::ExternalSend
        || result.call_id != completion.action_request_id
        || receipt.snapshot_id != snapshot.snapshot_id
        || receipt.snapshot_sha256 != snapshot.canonical_payload_sha256
        || receipt.idempotency_key
            != crate::communication::send_idempotency_key(&snapshot).map_err(|_| invalid())?
        || completion.facts.len() != 1
        || completion.facts[0].index != 0
    {
        return Err(invalid());
    }
    let fact = &completion.facts[0];
    let valid_result = match receipt.outcome {
        SendOutcome::Sent => {
            completion.result == ComputerActionResultClass::Verified
                && fact.changed
                && fact.verified
        }
        SendOutcome::DefinitelyNotSent => {
            completion.result == ComputerActionResultClass::Verified
                && !fact.changed
                && fact.verified
        }
        SendOutcome::OutcomeUnknown => {
            completion.result == ComputerActionResultClass::OutcomeUnknown
                && fact.changed
                && !fact.verified
        }
    };
    if !valid_result {
        return Err(invalid());
    }
    Ok(Some(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::communication::test_support::slack_exact_send_input;
    use desk_agent_protocol::computer_use::ComputerActionStepFact;
    use serde_json::json;

    fn fixture() -> (String, ComputerActionCompleted) {
        let input = json!({
            "schema_version": COMMUNICATION_SCHEMA_VERSION,
            "page": {
                "schema_version": 1,
                "adapter": {
                    "engine":"chrome_devtools_mcp", "device_id":"device",
                    "os_session_id":"session", "browser_major_version":145,
                    "browser_version":"145", "adapter_id":"fixture",
                    "adapter_version":"1", "profile_incarnation":"profile",
                    "connection_revision":7
                },
                "page_id":"page", "page_incarnation":"page-one",
                "origin":{"kind":"https","host_ascii":"app.slack.com","port":443},
                "document_revision":2, "url_sha256":"a".repeat(64),
                "observed_at_unix_ms":1
            },
            "composer": {
                "page_id":"page", "page_incarnation":"page-one",
                "document_revision":2, "element_id":"composer", "role":"textbox",
                "accessible_name":"Message #review", "value":null, "element_revision":1
            },
            "body_plain_text":"Draft only"
        })
        .to_string();
        let mut page: serde_json::Value = serde_json::from_str(&input).unwrap();
        page["page"]["document_revision"] = json!(3);
        let result: BrowserActionResult = serde_json::from_value(json!({
            "schema_version":1, "call_id":"request", "outcome":"form_filled",
            "page":page["page"].clone(),
            "snapshot":{
                "schema_version":1, "page":page["page"].clone(), "elements":[],
                "truncated":false, "captured_at_unix_ms":43
            },
            "form_readback":[{
                "request_element_id":"composer", "request_role":"textbox",
                "request_accessible_name":"Message #review", "source_element_id":"composer",
                "container_element_id":null, "kind":"control_value", "value":"Draft only"
            }],
            "completed_at_unix_ms":44
        }))
        .unwrap();
        let completed = ComputerActionCompleted {
            work_id: "1".into(),
            action_request_id: "request".into(),
            execution_generation: "generation".into(),
            result: ComputerActionResultClass::Verified,
            facts: vec![ComputerActionStepFact {
                index: 0,
                changed: true,
                verified: true,
                summary: "exact read-back".into(),
            }],
            message: None,
            output: Some(ComputerActionOutput::Browser(result)),
        };
        (input, completed)
    }

    #[test]
    fn slack_projection_requires_exact_verified_readback_and_stays_manual_only() {
        let (input, completed) = fixture();
        let handoff = project_web_draft_handoff(
            "prepare_slack_web_message_handoff",
            "run",
            &input,
            &"b".repeat(64),
            &completed,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            handoff.verification,
            CommunicationPrepareVerification::SemanticExact
        );
        assert_eq!(
            handoff.send_authority,
            CommunicationSendAuthority::ManualOnly
        );
        assert_eq!(handoff.readback_payload_sha256, Some("b".repeat(64)));

        for case in 0..5 {
            let mut bad = completed.clone();
            let Some(ComputerActionOutput::Browser(result)) = &mut bad.output else {
                unreachable!()
            };
            match case {
                0 => bad.result = ComputerActionResultClass::ChangedButUnverified,
                1 => bad.facts[0].verified = false,
                2 => result.form_readback[0].value = "changed".into(),
                3 => result.page.document_revision = 2,
                _ => result.page.origin.host_ascii = "example.com".into(),
            }
            assert!(
                project_web_draft_handoff(
                    "prepare_slack_web_message_handoff",
                    "run",
                    &input,
                    &"b".repeat(64),
                    &bad,
                )
                .is_err()
            );
        }
        assert!(
            project_web_draft_handoff(
                "browser_fill_form",
                "run",
                &input,
                &"b".repeat(64),
                &completed,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn chrome_extension_projection_seals_an_exact_send_snapshot() {
        let (input, mut completed) = fixture();
        let mut input_value: serde_json::Value = serde_json::from_str(&input).unwrap();
        input_value["page"]["adapter"]["engine"] = json!("chrome_extension");
        let input = serde_json::to_string(&input_value).unwrap();
        let Some(ComputerActionOutput::Browser(result)) = &mut completed.output else {
            unreachable!()
        };
        result.page.adapter.engine = BrowserEngineKind::ChromeExtension;
        if let Some(snapshot) = &mut result.snapshot {
            snapshot.page.adapter.engine = BrowserEngineKind::ChromeExtension;
        }

        let handoff = project_web_draft_handoff(
            "prepare_slack_web_message_handoff",
            "run",
            &input,
            &"b".repeat(64),
            &completed,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            handoff.send_authority,
            CommunicationSendAuthority::ExactGrantEligible
        );
        let snapshot = handoff.send_payload_snapshot.unwrap();
        assert_eq!(snapshot.run_id, "run");
        assert_eq!(
            snapshot.payload.surface.kind,
            CommunicationSurfaceKind::ChromeExtension
        );
        assert_eq!(
            snapshot.payload.recipients[0].canonical_address,
            "Message #review"
        );
        snapshot.validate_shape().unwrap();
    }

    #[test]
    fn exact_send_receipt_projection_binds_snapshot_and_three_outcomes() {
        use desk_agent_protocol::communication::SendReceiptEvidence;

        let input = slack_exact_send_input();
        let snapshot = input.handoff.send_payload_snapshot.as_ref().unwrap();
        let idempotency_key = crate::communication::send_idempotency_key(snapshot).unwrap();
        let canonical_input = serde_json::to_string(&input).unwrap();
        for (outcome, result, changed, verified, evidence, provider_receipt_id) in [
            (
                SendOutcome::Sent,
                ComputerActionResultClass::Verified,
                true,
                true,
                SendReceiptEvidence::ProviderUiAcknowledgement,
                Some("provider-receipt".to_string()),
            ),
            (
                SendOutcome::DefinitelyNotSent,
                ComputerActionResultClass::Verified,
                false,
                true,
                SendReceiptEvidence::PreconditionRejectedBeforeActivation,
                None,
            ),
            (
                SendOutcome::OutcomeUnknown,
                ComputerActionResultClass::OutcomeUnknown,
                true,
                false,
                SendReceiptEvidence::ReceiptNotObservedAfterActivation,
                None,
            ),
        ] {
            let receipt = SendReceipt {
                schema_version: COMMUNICATION_SCHEMA_VERSION,
                snapshot_id: snapshot.snapshot_id.clone(),
                snapshot_sha256: snapshot.canonical_payload_sha256.clone(),
                idempotency_key: idempotency_key.clone(),
                outcome,
                provider_receipt_id,
                evidence,
                observed_at_unix_ms: 300,
            };
            let completed = ComputerActionCompleted {
                work_id: "1".into(),
                action_request_id: "request".into(),
                execution_generation: "generation".into(),
                result,
                facts: vec![ComputerActionStepFact {
                    index: 0,
                    changed,
                    verified,
                    summary: "reviewed send result".into(),
                }],
                message: None,
                output: Some(ComputerActionOutput::Browser(BrowserActionResult {
                    schema_version: 1,
                    call_id: "request".into(),
                    outcome: BrowserActionOutcome::ExternalSend,
                    page: input.page.clone(),
                    snapshot: None,
                    form_readback: Vec::new(),
                    send_receipt: Some(receipt.clone()),
                    completed_at_unix_ms: 300,
                })),
            };
            assert_eq!(
                project_web_send_receipt("send_slack_web_exact", &canonical_input, &completed,)
                    .unwrap(),
                Some(receipt.clone())
            );

            let mut mismatched = completed;
            let Some(ComputerActionOutput::Browser(result)) = &mut mismatched.output else {
                unreachable!()
            };
            result.send_receipt.as_mut().unwrap().snapshot_sha256 = "c".repeat(64);
            assert!(
                project_web_send_receipt("send_slack_web_exact", &canonical_input, &mismatched,)
                    .is_err()
            );
        }
    }
}
