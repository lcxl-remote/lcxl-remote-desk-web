use super::*;
use crate::{
    action_turn_fence::AssistantTurnFence,
    seam::ToolOutputFormat,
    session::{ActionIdentity, WorkKind},
};
use desk_agent_protocol::{
    browser_control::{BrowserActionOutcome, BrowserActionResult},
    communication::{SendReceipt, SendReceiptEvidence},
    computer_use::{ComputerActionOutput, ComputerActionResultClass, ComputerActionStepFact},
    data_lineage::{RetentionBoundary, Sensitivity},
};

struct Fixture {
    input: String,
    completed: ComputerActionCompleted,
    origin: ActionResultOrigin,
    receipt: ActionResultReceipt,
    output: ToolRunOutput,
}

impl Fixture {
    fn context(&self, dispatch_at: u64, maximum_bytes: usize) -> NativeSendHistoryContext<'_> {
        NativeSendHistoryContext {
            origin: &self.origin,
            result: &self.receipt,
            output: &self.output,
            provider_device_id: "device",
            dispatched_at_unix_ms: dispatch_at,
            original_source_contract_revision:
                crate::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
            maximum_bytes,
        }
    }
    fn project(&self) -> NativeSendHistoryProjection {
        NativeSendHistoryProjection::from_web_action(
            self.context(200, 32768),
            &self.input,
            &self.completed,
        )
        .unwrap()
        .unwrap()
    }
}

fn fixture(tool: &str, outcome: SendOutcome) -> Fixture {
    let (input, page, snapshot, provider) = if tool == "send_gmail_message" {
        let input = crate::communication::test_support::gmail_exact_send_input();
        (
            serde_json::to_string(&input).unwrap(),
            input.page,
            input.handoff.send_payload_snapshot.unwrap(),
            crate::ai_assistant::GMAIL_WEB_SEND_PROVIDER_ID,
        )
    } else {
        let input = crate::communication::test_support::slack_exact_send_input();
        (
            serde_json::to_string(&input).unwrap(),
            input.page,
            input.handoff.send_payload_snapshot.unwrap(),
            crate::ai_assistant::SLACK_WEB_SEND_PROVIDER_ID,
        )
    };
    let (result_class, changed, verified, evidence, provider_receipt_id) = match outcome {
        SendOutcome::Sent => (
            ComputerActionResultClass::Verified,
            true,
            true,
            SendReceiptEvidence::ProviderUiAcknowledgement,
            Some("provider-receipt".into()),
        ),
        SendOutcome::DefinitelyNotSent => (
            ComputerActionResultClass::Verified,
            false,
            true,
            SendReceiptEvidence::PreconditionRejectedBeforeActivation,
            None,
        ),
        SendOutcome::OutcomeUnknown => (
            ComputerActionResultClass::OutcomeUnknown,
            true,
            false,
            SendReceiptEvidence::ReceiptNotObservedAfterActivation,
            None,
        ),
    };
    let send_receipt = SendReceipt {
        schema_version: desk_agent_protocol::communication::COMMUNICATION_SCHEMA_VERSION,
        snapshot_id: snapshot.snapshot_id,
        snapshot_sha256: snapshot.canonical_payload_sha256.clone(),
        idempotency_key: format!("send:v1:{}", snapshot.canonical_payload_sha256),
        outcome,
        provider_receipt_id,
        evidence,
        observed_at_unix_ms: 300,
    };
    let completed = ComputerActionCompleted {
        work_id: "1".into(),
        action_request_id: "request".into(),
        execution_generation: "generation".into(),
        result: result_class,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed,
            verified,
            summary: "Native send result".into(),
        }],
        message: None,
        output: Some(ComputerActionOutput::Browser(BrowserActionResult {
            schema_version: 1,
            call_id: "request".into(),
            outcome: BrowserActionOutcome::ExternalSend,
            page,
            snapshot: None,
            form_readback: Vec::new(),
            send_receipt: Some(send_receipt.clone()),
            completed_at_unix_ms: 300,
        })),
    };
    let output = ToolRunOutput {
        format: ToolOutputFormat::Json,
        content: crate::communication_handoff::project_send_receipt_output(&send_receipt).unwrap(),
        image_data_url: None,
        document_preview: None,
    };
    let origin = ActionResultOrigin {
        command_completion: None,
        schema_version: 1,
        turn_fence: AssistantTurnFence {
            schema_version: 1,
            conversation_id: "run".into(),
            turn_id: "turn".into(),
            actor_id: "owner".into(),
            device_id: "native-device".into(),
            input_revision: 1,
            lease_token: 1,
            control_revision: 1,
            delegation: None,
        },
        tool_call_id: "send-tool-call".into(),
        provider_id: provider.into(),
        tool_name: tool.into(),
        source_object_id: "original-object".into(),
        source_envelope_ids: vec!["model-parent".into()],
        sensitivity: Sensitivity::Sensitive,
        retention: RetentionBoundary {
            expires_at_unix_ms: Some(400),
            delete_with_run: true,
        },
        ephemeral: false,
    };
    let receipt = origin
        .receipt(
            ActionIdentity::new(1, "request", "generation", WorkKind::CapabilityProvider),
            1,
            350,
            &output,
        )
        .unwrap();
    Fixture {
        input,
        completed,
        origin,
        receipt,
        output,
    }
}

#[test]
fn original_native_outcomes_account_and_recipient_metadata_are_kept_without_message_body_or_handles()
 {
    for tool in ["send_gmail_message", "send_slack_message"] {
        for outcome in [
            SendOutcome::Sent,
            SendOutcome::DefinitelyNotSent,
            SendOutcome::OutcomeUnknown,
        ] {
            let fixture = fixture(tool, outcome);
            let projected = fixture.project();
            let fields: serde_json::Value = serde_json::from_str(projected.content()).unwrap();
            assert_eq!(fields["outcome"], serde_json::to_value(outcome).unwrap());
            assert_eq!(fields["received_at_unix_ms"], 350);
            assert_eq!(fields["provider_observed_at_unix_ms"], 300);
            assert_eq!(fields["recipients"].as_array().unwrap().len(), 1);
            assert_eq!(projected.original_envelope(), &fixture.receipt.envelope);
            assert_eq!(projected.recorded_at_unix_ms(), 350);
            assert_eq!(
                projected.metadata().columns(),
                NativeHistoryTimeColumns {
                    at_unix_ms: None,
                    from_unix_ms: Some(200),
                    until_unix_ms: Some(351),
                    basis: Some("provider_ack_observed".into())
                }
            );
            for forbidden in [
                "Reviewed Gmail body",
                "Reviewed Slack body",
                "profile_incarnation",
                "send_control",
                "idempotency_key",
                "body_plain_text",
            ] {
                assert!(!projected.content().contains(forbidden), "{forbidden}");
            }
            if tool == "send_gmail_message" {
                assert_eq!(fields["subject"], "Reviewed subject");
                assert_eq!(fields["account_id"], "gmail-web:owner@example.test");
                assert_eq!(
                    fields["recipients"][0]["canonical_address"],
                    "alice@example.com"
                );
            }
        }
    }
}

#[test]
fn bounded_field_projection_is_valid_json_and_reports_omitted_metadata_instead_of_slicing_it() {
    let fixture = fixture("send_gmail_message", SendOutcome::Sent);
    let full = fixture.project();
    let limit = full.content().len() - 1;
    let partial = NativeSendHistoryProjection::from_web_action(
        fixture.context(200, limit),
        &fixture.input,
        &fixture.completed,
    )
    .unwrap()
    .unwrap();
    assert!(partial.content().len() <= limit && partial.metadata().projection_partial);
    let fields: serde_json::Value = serde_json::from_str(partial.content()).unwrap();
    assert_eq!(fields["omitted_recipients"], 1);
    assert_eq!(fields["outcome"], "sent");
    assert!(matches!(
        NativeSendHistoryProjection::from_web_action(
            fixture.context(200, 1),
            &fixture.input,
            &fixture.completed
        ),
        Err(NativeSendHistoryError::Capacity)
    ));
}

#[test]
fn receipt_observation_never_becomes_an_exact_send_time_and_reversed_server_clocks_remain_unknown()
{
    let fixture = fixture("send_gmail_message", SendOutcome::OutcomeUnknown);
    let projected = NativeSendHistoryProjection::from_web_action(
        fixture.context(351, 32768),
        &fixture.input,
        &fixture.completed,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        projected.metadata().columns(),
        NativeHistoryTimeColumns::default()
    );
    let fields: serde_json::Value = serde_json::from_str(projected.content()).unwrap();
    assert_eq!(fields["timing_notice"], "native_clock_order_unavailable");
    assert_eq!(fields["outcome"], "outcome_unknown");
    let same_millisecond = NativeSendHistoryProjection::from_web_action(
        fixture.context(350, 32768),
        &fixture.input,
        &fixture.completed,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        same_millisecond.metadata().columns().from_unix_ms,
        Some(350)
    );
    assert_eq!(
        same_millisecond.metadata().columns().until_unix_ms,
        Some(351)
    );
}

#[test]
fn another_work_generation_payload_scope_provider_or_contradictory_output_is_never_a_verified_history_receipt()
 {
    for case in 0..7 {
        let mut fixture = fixture("send_gmail_message", SendOutcome::Sent);
        match case {
            0 => fixture.completed.execution_generation = "other-generation".into(),
            1 => fixture.completed.work_id = "2".into(),
            2 => {
                fixture.origin.provider_id = crate::ai_assistant::SLACK_WEB_SEND_PROVIDER_ID.into()
            }
            3 => {
                fixture.output.content =
                    "{\"receipt_status\":\"verified\",\"outcome\":\"sent\"}".into()
            }
            4 => {
                fixture.input = fixture
                    .input
                    .replace("Reviewed subject", "A later edited subject")
            }
            5 => fixture.origin.turn_fence.conversation_id = "other-run".into(),
            _ => fixture.receipt.origin_digest_sha256 = "f".repeat(64),
        }
        assert!(
            matches!(
                NativeSendHistoryProjection::from_web_action(
                    fixture.context(200, 32768),
                    &fixture.input,
                    &fixture.completed
                ),
                Err(NativeSendHistoryError::InvalidOriginal)
            ),
            "case {case}"
        );
    }
}

#[test]
fn original_device_and_compiled_contract_are_required_and_a_draft_never_becomes_a_send_receipt() {
    let mut fixture = fixture("send_gmail_message", SendOutcome::Sent);
    let mut context = fixture.context(200, 32768);
    context.provider_device_id = "other-device";
    assert!(matches!(
        NativeSendHistoryProjection::from_web_action(context, &fixture.input, &fixture.completed),
        Err(NativeSendHistoryError::InvalidOriginal)
    ));
    let mut context = fixture.context(200, 32768);
    context.original_source_contract_revision += 1;
    assert!(matches!(
        NativeSendHistoryProjection::from_web_action(context, &fixture.input, &fixture.completed),
        Err(NativeSendHistoryError::InvalidOriginal)
    ));
    fixture.origin.tool_name = "prepare_gmail_draft".into();
    assert!(
        NativeSendHistoryProjection::from_web_action(
            fixture.context(200, 32768),
            &fixture.input,
            &fixture.completed
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn provider_clock_skew_does_not_change_the_original_server_observation_interval_or_replay_clock() {
    let mut fixture = fixture("send_gmail_message", SendOutcome::Sent);
    let Some(ComputerActionOutput::Browser(browser)) = &mut fixture.completed.output else {
        unreachable!()
    };
    browser.completed_at_unix_ms = 5000;
    browser.send_receipt.as_mut().unwrap().observed_at_unix_ms = 5000;
    fixture.output.content = crate::communication_handoff::project_send_receipt_output(
        browser.send_receipt.as_ref().unwrap(),
    )
    .unwrap();
    fixture.receipt = fixture
        .origin
        .receipt(
            fixture.receipt.action.clone(),
            fixture.receipt.attempt,
            350,
            &fixture.output,
        )
        .unwrap();
    let first = fixture.project();
    let replay = fixture.project();
    assert_eq!(first.metadata(), replay.metadata());
    assert_eq!(first.content(), replay.content());
    assert_eq!(first.metadata().columns().until_unix_ms, Some(351));
    let fields: serde_json::Value = serde_json::from_str(first.content()).unwrap();
    assert_eq!(fields["provider_observed_at_unix_ms"], 5000);
    assert_eq!(fields["received_at_unix_ms"], 350);
}

#[test]
fn signal_computer_action_and_manager_provider_work_keep_their_original_native_kind() {
    let mut fixture = fixture("send_gmail_message", SendOutcome::Sent);
    fixture.receipt = fixture
        .origin
        .receipt(
            ActionIdentity::new(1, "request", "generation", WorkKind::ComputerAction),
            1,
            350,
            &fixture.output,
        )
        .unwrap();
    let projected = fixture.project();
    assert_eq!(projected.original_envelope(), &fixture.receipt.envelope);
    assert_eq!(fixture.receipt.action.kind, WorkKind::ComputerAction);
}
