use desk_agent_protocol::{communication::*, data_lineage::DestinationIdentity};
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCallRef},
    prompt::ResponseFormatSpec,
    seam::ModelRequest,
};
use serde_json::{Value, json};

pub fn request() -> ModelRequest {
    let owner = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "owner".into(),
        "Report the observed outcome".into(),
        DestinationIdentity::Model {
            connection_id: "test".into(),
            connection_revision: 1,
            model_id: "test".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let receipt = SendReceipt {
        schema_version: COMMUNICATION_SCHEMA_VERSION,
        snapshot_id: "private-snapshot-marker".into(),
        snapshot_sha256: "a".repeat(64),
        idempotency_key: "private-idempotency-marker".into(),
        outcome: SendOutcome::OutcomeUnknown,
        provider_receipt_id: None,
        evidence: SendReceiptEvidence::ReceiptNotObservedAfterActivation,
        observed_at_unix_ms: 42,
    };
    receipt.validate().unwrap();
    let mut messages = vec![owner.clone()];
    for (i, tool, value) in [
        (0, "send_gmail_message", json!(receipt)),
        (
            1,
            "search_conversations",
            json!({"hits":[{"source_id":"src_compact","revision":1,"kind":"assistant_message","locator":{"conversation_id":"private-conversation-marker","root_conversation_id":"private-conversation-marker","content_sha256":"private-source-hash-marker","message_id":"private-message-marker","fragment_index":0,"byte_start":0,"byte_end":42},"snippet":"business-marker","recorded_at_unix_ms":42,"statement_timezone":"Asia/Shanghai","source_device":{"device_ref":"dev_compact","current_name":"Laptop","recorded_name":null,"historical":true}}],"has_more":false,"scope_changed":false,"coverage":{"has_gaps":true,"partial":true,"reasons":["some history unavailable"]}}),
        ),
        (
            2,
            "search_public_web",
            json!({"query_sha256":"private-query-hash-marker","response_sha256":"private-response-hash-marker","configuration_revision":4,"untrusted_external_content":true,"results":[{"title":"Observed source","url":"https://example.test/a","snippet":"source-marker"}]}),
        ),
    ] {
        let call = format!("call-{i}");
        messages.push(ChatMessage::assistant_tool_calls(
            format!("producer-{i}"),
            "",
            vec![ToolCallRef {
                id: call.clone(),
                name: tool.into(),
                arguments_json: "{}".into(),
            }],
        ));
        let mut message = ChatMessage::tool_result(format!("result-{i}"), &call, value.to_string());
        message.tool_ok = Some(true);
        message.data_envelope =
            desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
                owner.data_envelope.as_ref(),
                &call,
                &message.text,
                tool,
            )
            .unwrap();
        messages.push(message);
    }
    let mut references = desk_diagnose_core::ui_references::UiReferenceState::default();
    references.observe(&messages).unwrap();
    let mut request = ModelRequest::text_only(messages, ResponseFormatSpec::None);
    request.ui_references =
        Some(serde_json::from_str(&serde_json::to_string(&references).unwrap()).unwrap());
    request
}

pub fn assert_wire(body: &Value) {
    let body = body.to_string();
    for marker in [
        "outcome_unknown",
        "receipt_not_observed_after_activation",
        "business-marker",
        "cv1",
        "src_compact",
        "source-marker",
        "untrusted_external_content",
        "has_gaps",
        "some history unavailable",
    ] {
        assert!(body.contains(marker), "missing {marker}");
    }
    for marker in [
        "private-snapshot-marker",
        "private-idempotency-marker",
        "private-conversation-marker",
        "private-source-hash-marker",
        "private-message-marker",
        "private-query-hash-marker",
        "private-response-hash-marker",
        "result_summary",
        "output_type",
        "contract_version",
    ] {
        assert!(!body.contains(marker), "leaked {marker}");
    }
}
