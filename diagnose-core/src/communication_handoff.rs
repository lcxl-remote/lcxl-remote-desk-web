//! Exact, manual-only communication handoff projection shared by orchestrators.

use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    browser_control::{
        BrowserActionOutcome, BrowserActionResult, BrowserElementRef, BrowserEngineKind,
        BrowserFormFieldReadback, BrowserFormReadbackKind,
    },
    communication::{
        COMMUNICATION_SCHEMA_VERSION, CommunicationChannel, CommunicationDraftHandoff,
        CommunicationPrepareVerification, CommunicationSendAuthority, CommunicationSurfaceKind,
        CommunicationSurfaceRef, CommunicationSurfaceScope, GmailWebDraftHandoffInput,
        SlackWebDraftHandoffInput,
    },
    computer_use::{ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass},
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
        let handoff = CommunicationDraftHandoff {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            handoff_id: format!("gmail-handoff-{compose_digest}"),
            run_id: run_id.into(),
            surface: CommunicationSurfaceRef {
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
            },
            compose_id: format!("gmail-compose-{compose_digest}"),
            prepared_payload_sha256: canonical_input_digest_sha256.into(),
            verification: CommunicationPrepareVerification::SemanticExact,
            readback_payload_sha256: Some(canonical_input_digest_sha256.into()),
            send_authority: CommunicationSendAuthority::ManualOnly,
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
    let handoff = CommunicationDraftHandoff {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        handoff_id: format!("slack-handoff-{compose_digest}"),
        run_id: run_id.into(),
        surface: CommunicationSurfaceRef {
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
        },
        compose_id: format!("slack-compose-{compose_digest}"),
        prepared_payload_sha256: canonical_input_digest_sha256.into(),
        verification: CommunicationPrepareVerification::SemanticExact,
        readback_payload_sha256: Some(canonical_input_digest_sha256.into()),
        send_authority: CommunicationSendAuthority::ManualOnly,
        handed_off_at_unix_ms: result.completed_at_unix_ms,
    };
    handoff.validate().map_err(|_| invalid())?;
    Ok(Some(handoff))
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
