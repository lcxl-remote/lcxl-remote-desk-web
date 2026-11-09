use super::*;
use crate::{chat::ToolCallRef, prompt::ResponseFormatSpec, seam::ModelRequest};
use desk_agent_protocol::data_lineage::DestinationIdentity;

pub(crate) fn history(tool: &str, value: Value) -> Vec<ChatMessage> {
    let owner = crate::model_message_labels::model_bound_user_message(
        "owner".into(),
        "Requested operation".into(),
        DestinationIdentity::Model {
            connection_id: "test".into(),
            connection_revision: 1,
            model_id: "test".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let mut result = ChatMessage::tool_result("result", "call", value.to_string());
    result.tool_ok = Some(true);
    result.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        owner.data_envelope.as_ref(),
        "call",
        &result.text,
        tool,
    )
    .unwrap();
    if let Some(capability) =
        crate::ai_assistant::ai_assistant_provider_registry().capability_for_tool(tool)
    {
        result
            .data_envelope
            .as_mut()
            .unwrap()
            .provenance
            .source_provider_id = crate::ai_assistant::ai_assistant_provider_registry()
            .provider_for_capability(&capability.wire.capability_id)
            .unwrap()
            .wire
            .provider_id
            .clone();
    }
    vec![
        owner,
        ChatMessage::assistant_tool_calls(
            "producer",
            "",
            vec![ToolCallRef {
                id: "call".into(),
                name: tool.into(),
                arguments_json: "{}".into(),
            }],
        ),
        result,
    ]
}

fn project(tool: &str, value: Value) -> Value {
    let request = ModelRequest::text_only(history(tool, value), ResponseFormatSpec::None);
    let original = serde_json::to_string(&request.messages).unwrap();
    let projected = project_request(&request);
    eprintln!(
        "{tool}: {} -> {} model text bytes",
        request.messages.last().unwrap().text.len(),
        projected.messages.last().unwrap().text.len()
    );
    assert_eq!(serde_json::to_string(&request.messages).unwrap(), original);
    assert_eq!(
        project_request(&projected).messages.last().unwrap().text,
        projected.messages.last().unwrap().text
    );
    serde_json::from_str(&projected.messages.last().unwrap().text).unwrap()
}

#[test]
fn public_web_keeps_sources_and_trust_but_hides_network_evidence() {
    let result = project(
        "search_public_web",
        json!({"schema_version":1,"configuration_revision":9,"web_search_call_id":"long-call","query_sha256":"query-hash","response_sha256":"response-hash","response_bytes":200,"untrusted_external_content":true,"connector":{"connector_id":"test","display_name":"Search","requires_api_key":true,"experimental":false},"searched_at":42,"results":[{"title":"Business sha256","url":"https://example.test/a","snippet":"schema_version=9"}]}),
    );
    for field in [
        "schema_version",
        "configuration_revision",
        "web_search_call_id",
        "query_sha256",
        "response_sha256",
        "response_bytes",
        "result_summary",
    ] {
        assert!(result.get(field).is_none(), "{field}");
    }
    assert_eq!(result["results"][0]["source_id"], "1");
    assert_eq!(result["results"][0]["snippet"], "schema_version=9");
    assert_eq!(result["untrusted_external_content"], true);
    assert!(result["connector"].get("requires_api_key").is_none());
    let result = project(
        "fetch_public_web_page",
        json!({"sha256":"hash","body_bytes":100,"untrusted_external_content":true,"requested_url":"https://example.test/a","final_url":"https://example.test/b","excerpt":"sha256: user data","fetched_at":42}),
    );
    assert!(result.get("sha256").is_none());
    assert_eq!(result["excerpt"], "sha256: user data");
    assert_eq!(result["final_url"], "https://example.test/b");
}

#[test]
fn typed_send_receipts_preserve_all_three_outcomes_inline_and_nested() {
    use desk_agent_protocol::communication::{
        COMMUNICATION_SCHEMA_VERSION, SendOutcome, SendReceipt, SendReceiptEvidence,
    };
    for (outcome, evidence) in [
        (
            SendOutcome::Sent,
            SendReceiptEvidence::ProviderUiAcknowledgement,
        ),
        (
            SendOutcome::DefinitelyNotSent,
            SendReceiptEvidence::PreconditionRejectedBeforeActivation,
        ),
        (
            SendOutcome::OutcomeUnknown,
            SendReceiptEvidence::ReceiptNotObservedAfterActivation,
        ),
    ] {
        let receipt = SendReceipt {
            schema_version: COMMUNICATION_SCHEMA_VERSION,
            snapshot_id: "snapshot".into(),
            snapshot_sha256: "a".repeat(64),
            idempotency_key: "native-key".into(),
            outcome,
            evidence,
            provider_receipt_id: (outcome == SendOutcome::Sent)
                .then(|| "gmail-ui-derived-hash".into()),
            observed_at_unix_ms: 42,
        };
        receipt.validate().unwrap();
        let raw = json!(receipt);
        for tool in ["send_gmail_message", "send_slack_message"] {
            let view = project(tool, raw.clone());
            for field in [
                "schema_version",
                "snapshot_id",
                "snapshot_sha256",
                "idempotency_key",
                "provider_receipt_id",
                "result_summary",
            ] {
                assert!(view.get(field).is_none(), "{field}");
            }
            assert_eq!(view["outcome"], raw["outcome"]);
            assert_eq!(view["evidence"], raw["evidence"]);
            assert_eq!(view["observed_at_unix_ms"], 42);
            let nested = project(
                tool,
                json!({"output":{"kind":"browser","value":{"send_receipt":raw,"data":{"sha256":"business hash","revision":0,"verified":false}}},"result":"changed_but_unverified"}),
            );
            assert!(
                nested["output"]["value"]["send_receipt"]
                    .get("idempotency_key")
                    .is_none()
            );
            assert_eq!(nested["output"]["value"]["data"]["sha256"], "business hash");
            assert_eq!(nested["result"], "changed_but_unverified");
        }
    }
}

#[test]
fn source_and_memory_versions_have_distinct_model_purposes() {
    use desk_agent_protocol::ai_assistant::source::{
        AssistantSourceLocator, AssistantSourceResolution, SourceAvailability,
    };
    let locator = AssistantSourceLocator {
        conversation_id: "a".repeat(64),
        root_conversation_id: "a".repeat(64),
        message_id: Some("long-message".into()),
        input_event_id: None,
        action_id: None,
        result_id: None,
        fragment_index: 0,
        byte_start: 0,
        byte_end: 42,
    };
    let source = AssistantSourceResolution {
        source_id: "src_compact".into(),
        revision: 9,
        availability: SourceAvailability::Partial,
        locator: Some(locator),
        source_device: None,
        content: Some("Original body revision=9".into()),
        recorded_at_unix_ms: Some(42),
        statement_timezone: Some("Asia/Shanghai".into()),
        notice: Some("Partial source".into()),
    };
    let messages = history("read_memory_source", json!(source));
    let mut refs = crate::ui_references::UiReferenceState::default();
    refs.observe(&messages).unwrap();
    let mut request = ModelRequest::text_only(messages.clone(), ResponseFormatSpec::None);
    request.ui_references = Some(refs.clone());
    let projected = project_request(&request);
    let view: Value = serde_json::from_str(&projected.messages.last().unwrap().text).unwrap();
    assert_eq!(view["conversation_id"], "cv1");
    assert!(view.get("root_conversation_id").is_none());
    assert!(view.get("locator").is_none());
    assert!(view.get("revision").is_none());
    assert_eq!(view["availability"], "partial");
    assert_eq!(view["content"], source.content.unwrap());
    let original_request = json!({"queries":["test"], "conversation_ids":["cv1"]});
    let search: desk_agent_protocol::ai_assistant::history::ConversationSearchRequest =
        serde_json::from_value(original_request.clone()).unwrap();
    let restored: crate::ui_references::UiReferenceState =
        serde_json::from_str(&serde_json::to_string(&refs).unwrap()).unwrap();
    assert_eq!(
        restored
            .resolve_conversation_request(&search)
            .unwrap()
            .conversation_ids,
        vec!["a".repeat(64)]
    );
    assert_eq!(search.conversation_ids, vec!["cv1"]);
    let mut forged = search.clone();
    forged.conversation_ids = vec!["cv99".into()];
    assert!(restored.resolve_conversation_request(&forged).is_err());
    assert!(
        crate::ui_references::UiReferenceState::default()
            .resolve_conversation_request(&search)
            .is_err()
    );
    let view = project(
        "manage_memories",
        json!({"outcome":"applied","memory":{"memory_id":"mem_compact","revision":7,"content":"sha256 is useful","context":null},"control_receipt":{"operation_id":"native-op","outcome":"applied","reason":"memory_forget","affected_count":1,"control_generation":8,"memory_state_generation":9,"cleanup_pending":true}}),
    );
    assert_eq!(view["memory"]["revision"], 7);
    assert!(view["memory"].get("context").is_none());
    assert_eq!(view["control_receipt"]["affected_count"], 1);
    for field in [
        "operation_id",
        "control_generation",
        "memory_state_generation",
        "cleanup_pending",
    ] {
        assert!(view["control_receipt"].get(field).is_none());
    }
}

#[test]
fn live_document_protocol_data_gets_aliases_without_touching_business_data() {
    use desk_agent_protocol::computer_use::{
        ComputerUseAdapterKind, ComputerUseAdapterRef, LiveDocumentInspectOutput,
        LiveDocumentProjection, ObjectKind, ObjectRef,
    };
    let reference = ObjectRef {
        token: "long-document-token".into(),
        snapshot_id: "snapshot".into(),
        object_kind: ObjectKind::Document,
        expires_at: "2099-01-01T00:00:00Z".into(),
    };
    let inspect = LiveDocumentInspectOutput {
        snapshot_id: "snapshot".into(),
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::MacosAccessibility,
            version: "1".into(),
        },
        projection: LiveDocumentProjection::Document {
            document: reference,
            body_text: "{\"sha256\":\"body\",\"revision\":0,\"enabled\":false}".into(),
            body_sha256: "a".repeat(64),
        },
        batch_source: None,
    };
    let value = json!(desk_agent_protocol::OperationOutput::ReadContext(
        desk_agent_protocol::ReadContextOutput::DocumentLiveInspect(inspect)
    ));
    let messages = history("inspect_live_document", value);
    let mut refs = crate::ui_references::UiReferenceState::default();
    refs.observe(&messages).unwrap();
    let mut request = ModelRequest::text_only(messages, ResponseFormatSpec::None);
    request.ui_references = Some(refs);
    let projected = project_request(&request);
    let view: Value = serde_json::from_str(&projected.messages.last().unwrap().text).unwrap();
    let body = &view["ReadContext"]["DocumentLiveInspect"];
    assert_eq!(body["projection"]["data"]["document"]["id"], "doc1");
    assert!(body.get("adapter").is_none());
    assert!(body["projection"]["data"].get("body_sha256").is_none());
    assert_eq!(
        body["projection"]["data"]["body_text"],
        "{\"sha256\":\"body\",\"revision\":0,\"enabled\":false}"
    );
}

#[test]
fn capacity_status_and_preview_do_not_imply_business_completion() {
    let view = project(
        "exec_command",
        json!({"status":"attachment_capacity","result_stored":false,"original_result_sha256":"hash","notice":"Do not repeat the action"}),
    );
    assert!(view.get("original_result_sha256").is_none());
    assert_eq!(view["result_stored"], false);
    assert_eq!(view["status"], "attachment_capacity");
    let view = project(
        "preview_computer_action",
        json!({"schema_version":1,"adapter":{"kind":"macos_accessibility","version":1},"risk":"high","reversible":false,"data_egress":true,"actions":[{"before_summary":"Before","after_intent":"After"}]}),
    );
    assert!(view.get("adapter").is_none());
    assert!(view.get("schema_version").is_none());
    assert_eq!(view["risk"], "high");
    assert_eq!(view["reversible"], false);
    let view = project(
        "load_conversation_history",
        json!({"unavailable_count":0,"availability_notice":"Repeated notice","messages":[],"has_more":false}),
    );
    assert!(view.get("availability_notice").is_none());
    let view = project(
        "load_conversation_history",
        json!({"unavailable_count":1,"availability_notice":"Missing authorization","messages":[],"has_more":false}),
    );
    assert_eq!(view["availability_notice"], "Missing authorization");
}

#[test]
fn native_completion_keeps_business_hash_and_false_verification() {
    use desk_agent_protocol::computer_use::*;
    let artifact = BatchDocumentArtifact {
        file: ObjectRef {
            token: "file-token".into(),
            snapshot_id: "snapshot".into(),
            object_kind: ObjectKind::File,
            expires_at: "2099-01-01T00:00:00Z".into(),
        },
        file_name: "report.xlsx".into(),
        byte_len: 42,
        sha256: "a".repeat(64),
        validation_byte_len: 100,
        validation_sha256: "b".repeat(64),
    };
    let completed = ComputerActionCompleted {
        work_id: "native-work".into(),
        action_request_id: "native-request".into(),
        execution_generation: "native-generation".into(),
        result: ComputerActionResultClass::ChangedButUnverified,
        facts: vec![ComputerActionStepFact {
            index: 0,
            changed: true,
            verified: false,
            summary: "Saved but not verified".into(),
        }],
        message: None,
        output: Some(ComputerActionOutput::BatchDocumentArtifact(
            artifact.clone(),
        )),
    };
    for tool in [
        "patch_numbers_copy",
        "replace_pages_copy_body",
        "patch_keynote_copy",
        "patch_excel_copy",
        "replace_word_copy_body",
        "patch_powerpoint_copy",
    ] {
        let view = project(tool, json!(completed));
        for key in ["work_id", "action_request_id", "execution_generation"] {
            assert!(view.get(key).is_none());
        }
        assert_eq!(view["result"], "changed_but_unverified");
        assert_eq!(view["facts"][0]["verified"], false);
        assert_eq!(view["output"]["value"]["sha256"], artifact.sha256);
        assert!(view["output"]["value"].get("validation_sha256").is_none());
        assert!(view["output"]["value"].get("validation_byte_len").is_none());
    }
}

#[test]
fn typed_office_selection_and_live_sheet_references_roundtrip() {
    use desk_agent_protocol::{OperationOutput, ReadContextOutput, computer_use::*};
    let reference = |token: &str, object_kind: ObjectKind| ObjectRef {
        token: token.into(),
        snapshot_id: "observation".into(),
        object_kind,
        expires_at: "2099-01-01T00:00:00Z".into(),
    };
    let doc = reference("native-document", ObjectKind::Document);
    let worksheet = reference("native-sheet", ObjectKind::Worksheet);
    let range = reference("native-range", ObjectKind::Range);
    let adapter = ComputerUseAdapterRef {
        kind: ComputerUseAdapterKind::OfficeExcel,
        version: "1".into(),
    };
    let business = OfficeCellValue::Text(
        r#"{"sha256":"user data","schema_version":0,"verified":false}"#.into(),
    );
    let office = OfficeInspectOutput {
        snapshot_id: "observation".into(),
        adapter: adapter.clone(),
        selection: OfficeSelectionProjection::Excel {
            document: doc.clone(),
            worksheet: worksheet.clone(),
            range: range.clone(),
            address: "A1".into(),
            row_count: 1,
            column_count: 1,
            has_formulas: false,
            cells: vec![ExcelCellProjection {
                row_offset: 0,
                column_offset: 0,
                formula: None,
                value: business,
                number_format: None,
            }],
        },
        truncated: false,
    };
    let sheet = LiveDocumentInspectOutput {
        snapshot_id: "observation".into(),
        adapter,
        projection: LiveDocumentProjection::Spreadsheet {
            document: doc,
            worksheet,
            range,
            sheet_name: "Sheet 1".into(),
            table_name: "Table 1".into(),
            address: "A1".into(),
            value: "0".into(),
            formula: None,
            formatted_value: "0".into(),
        },
        batch_source: None,
    };
    let presentation = OfficeInspectOutput {
        snapshot_id: "observation".into(),
        adapter: ComputerUseAdapterRef {
            kind: ComputerUseAdapterKind::OfficePowerPoint,
            version: "1".into(),
        },
        selection: OfficeSelectionProjection::PowerPoint {
            document: reference("native-document", ObjectKind::Document),
            slides: vec![reference("native-slide", ObjectKind::Slide)],
            shapes: vec![reference("native-shape", ObjectKind::Shape)],
        },
        truncated: false,
    };
    for (tool, output, path) in [
        (
            "inspect_office_selection",
            ReadContextOutput::OfficeDocumentInspect(office),
            "/ReadContext/OfficeDocumentInspect/selection/data",
        ),
        (
            "inspect_live_spreadsheet",
            ReadContextOutput::SpreadsheetLiveInspect(sheet),
            "/ReadContext/SpreadsheetLiveInspect/projection/data",
        ),
        (
            "inspect_office_selection",
            ReadContextOutput::OfficeDocumentInspect(presentation),
            "/ReadContext/OfficeDocumentInspect/selection/data",
        ),
    ] {
        let original = json!(OperationOutput::ReadContext(output));
        let messages = history(tool, original.clone());
        let mut refs = crate::ui_references::UiReferenceState::default();
        refs.observe(&messages).unwrap();
        let mut request = ModelRequest::text_only(messages.clone(), ResponseFormatSpec::None);
        request.ui_references = Some(refs.clone());
        let projected = project_request(&request);
        let view: Value = serde_json::from_str(&projected.messages.last().unwrap().text).unwrap();
        let data = view.pointer(path).unwrap();
        assert_eq!(data["document"]["id"], "doc1");
        let range = data.get("range").is_some();
        if range {
            assert_eq!(data["range"]["id"], "r1");
            assert_eq!(data["worksheet"]["id"], "ws1");
        } else {
            assert_eq!(data["slides"][0]["id"], "sl1");
            assert_eq!(data["shapes"][0]["id"], "sh1");
        }
        if let Some(cells) = data.get("cells") {
            assert_eq!(Some(cells), original.pointer(path).unwrap().get("cells"));
        }
        let call = crate::chat::ToolCall {
            id: "next".into(),
            name: if range {
                "inspect_live_spreadsheet"
            } else {
                "inspect_live_document"
            }
            .into(),
            arguments_json: json!({"target_id":if range { "r1" } else { "doc1" },"max_bytes":1024})
                .to_string(),
        };
        let (expanded, expected) = refs.expand_call(&call).unwrap();
        let resolved = resolve_call_with_references(&expanded, &messages, 1, &expected).unwrap();
        crate::ui_references::UiReferenceState::validate_resolved(&expected, &resolved).unwrap();
        let native: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
        assert_eq!(
            native["target"]["token"],
            if range {
                "native-range"
            } else {
                "native-document"
            }
        );
    }
}

#[test]
fn failed_call_status_and_background_business_results_keep_their_meaning() {
    let mut messages = history(
        "exec_command",
        json!({"result":"outcome_unknown","diagnostic":{"code":"EIO","message":"No conclusive receipt"},"stdout":"user sha256 field","exit_code":null}),
    );
    messages.last_mut().unwrap().tool_ok = Some(false);
    let request = ModelRequest::text_only(messages.clone(), ResponseFormatSpec::None);
    let view = project_request(&request);
    let value: Value = serde_json::from_str(&view.messages.last().unwrap().text).unwrap();
    assert_eq!(value["tool_call_status"], "returned_error");
    assert_eq!(value["result"], "outcome_unknown");
    assert_eq!(value["diagnostic"]["code"], "EIO");
    messages.last_mut().unwrap().role = ChatRole::UntrustedOutput;
    let background = project_request(&ModelRequest::text_only(messages, ResponseFormatSpec::None));
    let value: Value = serde_json::from_str(&background.messages.last().unwrap().text).unwrap();
    assert_eq!(value["result"], "outcome_unknown");
    assert_eq!(value["stdout"], "user sha256 field");
}

#[test]
fn browser_completion_header_is_hidden_after_nested_browser_projection() {
    use desk_agent_protocol::{
        browser_control::{BROWSER_CONTROL_SCHEMA_VERSION, BrowserActionResult},
        computer_use::*,
    };
    let page = json!({"schema_version":BROWSER_CONTROL_SCHEMA_VERSION,"adapter":{"engine":"chrome_extension","device_id":"device","os_session_id":"session","browser_major_version":144,"browser_version":"144.0.0.0","adapter_id":"extension","adapter_version":"1.0","profile_incarnation":"profile","connection_revision":1},"page_id":"native-page","page_incarnation":"incarnation","origin":{"kind":"https","host_ascii":"example.com","port":443},"document_revision":1,"url_sha256":"a".repeat(64),"observed_at_unix_ms":1,"account_id":null});
    let browser:BrowserActionResult=serde_json::from_value(json!({"schema_version":BROWSER_CONTROL_SCHEMA_VERSION,"call_id":"call","outcome":"snapshot_captured","page":page,"snapshot":{"schema_version":BROWSER_CONTROL_SCHEMA_VERSION,"page":page,"elements":[],"truncated":false,"captured_at_unix_ms":1},"form_readback":[],"send_receipt":null,"completed_at_unix_ms":2})).unwrap();
    browser.validate().unwrap();
    let completed = ComputerActionCompleted {
        work_id: "private-work".into(),
        action_request_id: "private-request".into(),
        execution_generation: "private-generation".into(),
        result: ComputerActionResultClass::Verified,
        facts: vec![],
        message: None,
        output: Some(ComputerActionOutput::Browser(browser)),
    };
    for role in [ChatRole::Tool, ChatRole::UntrustedOutput] {
        let mut messages = history("browser_take_snapshot", json!(completed));
        messages.last_mut().unwrap().role = role;
        let request = ModelRequest::text_only(messages, ResponseFormatSpec::None);
        let view = project_request(&request);
        let value: Value = serde_json::from_str(&view.messages.last().unwrap().text).unwrap();
        for key in ["work_id", "action_request_id", "execution_generation"] {
            assert!(value.get(key).is_none());
        }
        assert_eq!(value["output"]["value"]["outcome"], "snapshot_captured");
        assert!(value["output"]["value"]["page"].get("adapter").is_none());
        assert_eq!(value["result"], "verified");
    }
}

#[test]
fn typed_history_inventory_and_memory_search_keep_uncertainty_coverage_and_fact_versions() {
    use desk_agent_protocol::ai_assistant::{history::*, memory::*, source::*};
    let device = AssistantSourceDevice {
        device_ref: "dev_compact".into(),
        recorded_name: None,
        current_name: Some("Laptop".into()),
        historical: true,
    };
    let coverage = AssistantHistoryCoverage {
        retained_from_unix_ms: Some(1),
        retained_until_unix_ms: Some(100),
        indexed_through_unix_ms: Some(50),
        pending: 1,
        failed: 2,
        has_gaps: true,
        partial: true,
        reasons: vec!["index incomplete".into()],
    };
    let native = NativeStatusProjection {
        native_id: "t_compact".into(),
        kind: "mail".into(),
        state: "unknown".into(),
        revision: Some(7),
        observed_at_unix_ms: 42,
        historical: true,
        source_id: "src_native".into(),
        source_device: Some(device.clone()),
    };
    let summary = NativeStatusSummary {
        source_id: "src_inventory".into(),
        source_revision: 3,
        additional_sources: vec![AssistantSourceRef {
            kind: AssistantSourceKind::NativeStatus,
            id: "src_extra".into(),
            revision: Some(4),
            role: SourceRefRole::Evidence,
        }],
        last_recorded_at_unix_ms: 42,
        items: vec![native.clone()],
        additional_count: 0,
        pending_count: 1,
        unknown_count: 1,
        no_native_task: false,
        has_more: true,
    };
    let search = ConversationSearchResult {
        hits: vec![ConversationSearchHit {
            source_id: "src_hit".into(),
            revision: 3,
            kind: AssistantSourceKind::NativeStatus,
            locator: AssistantSourceLocator {
                conversation_id: "conversation".into(),
                root_conversation_id: "root".into(),
                message_id: None,
                input_event_id: None,
                action_id: None,
                result_id: None,
                fragment_index: 0,
                byte_start: 0,
                byte_end: 3,
            },
            source_device: device.clone(),
            conversation_title: Some("Report".into()),
            snippet: "Recorded outcome is unknown".into(),
            recorded_at_unix_ms: 42,
            statement_timezone: "America/Los_Angeles".into(),
            native_action_at_unix_ms: None,
            native_action_interval: None,
            native_time_basis: None,
            native_status_summary: Some(summary),
        }],
        scope: AssistantDeviceScope::AllMyDevices,
        effective_queries: vec![vec!["report".into()]],
        retrieval_mode: "bm25_rrf".into(),
        has_more: true,
        next_cursor: Some("native-cursor".into()),
        scope_changed: true,
        coverage: coverage.clone(),
    };
    let view = project("search_conversations", json!(search));
    let summary = &view["hits"][0]["native_status_summary"];
    assert!(summary.get("source_revision").is_none());
    assert!(summary["items"][0].get("revision").is_none());
    assert!(summary["additional_sources"][0].get("revision").is_none());
    assert_eq!(summary["items"][0]["historical"], true);
    assert_eq!(summary["items"][0]["state"], "unknown");
    assert_eq!(summary["no_native_task"], false);
    assert_eq!(summary["unknown_count"], 1);
    assert_eq!(view["scope_changed"], true);
    assert_eq!(view["coverage"], json!(coverage));
    assert_eq!(view["next_cursor"], "native-cursor");
    let read = ConversationReadResult {
        conversation_id: "conversation".into(),
        messages: vec![],
        related_results: vec![native],
        previous_cursor: None,
        next_cursor: None,
        source_observed_through_unix_ms: 42,
        actual_bytes: 100,
        has_more: false,
        partial: true,
        reasons: vec!["missing source".into()],
    };
    let view = project("read_conversation", json!(read));
    assert!(view["related_results"][0].get("revision").is_none());
    assert_eq!(view["related_results"][0]["state"], "unknown");
    assert_eq!(view["partial"], true);

    let memories = MemorySearchResult {
        memories: vec![MemoryRecord {
            memory_id: "mem_compact".into(),
            revision: 8,
            scope: MemoryScope::Personal,
            source_device: Some(device),

            content: "Use Chinese".into(),
            context: Some("Owner preference".into()),
            status: MemoryStatus::PendingConflict,
            source_ids: vec!["src_hit".into()],
            observed_at_unix_ms: 1,
            updated_at_unix_ms: 42,
            valid_from_unix_ms: 1,
            valid_until_unix_ms: None,
        }],
        effective_queries: vec![vec!["language".into()]],
        has_more: false,
        next_cursor: None,
        memory_state_generation: 99,
        coverage: coverage.clone(),
    };
    let view = project("search_memories", json!(memories));
    assert!(view.get("memory_state_generation").is_none());
    assert_eq!(view["memories"][0]["revision"], 8);
    assert_eq!(view["memories"][0]["status"], "pending_conflict");
    assert_eq!(view["memories"][0]["context"], "Owner preference");
    assert_eq!(view["coverage"], json!(coverage));
}

#[test]
fn attachment_pages_share_inline_projection_and_keep_original_proof_on_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new().stack_size(bytes).spawn(|| {
            use crate::conversation_attachment::{batch::*, model_read, read::ReadPage};
            let raw=json!({"conversation_id":"a".repeat(64),"messages":[{"source_id":"src_compact","message_id":"long-message","revision":3,"role":"assistant","content":"零".repeat(2000),"recorded_at_unix_ms":42,"statement_timezone":"Asia/Shanghai","source_device":{"device_ref":"dev_compact","recorded_name":null,"current_name":"Laptop","historical":true}}],"related_results":[],"previous_cursor":null,"next_cursor":null,"source_observed_through_unix_ms":42,"actual_bytes":6000,"has_more":false,"partial":false,"reasons":[]});
            let history=history("read_conversation",raw.clone());
            let result=history.last().unwrap();
            let mut delivery=prepare_delivery(&DeliveryIdentity {conversation_id:"run",actor_id:"owner",device_id:"device",message_id:"result",tool_call_id:"call"},vec![OutputPart {name:"result".into(),content:PartContent::Json(result.text.clone()),source_truncated:false}],1).unwrap();
            let mut part=delivery.attachments.remove(0);part.metadata.source_envelope=result.data_envelope.clone();
            let original=part.clone();
            let mut refs=crate::ui_references::UiReferenceState::default();refs.observe_attachment(&part).unwrap();
            let mut input=model_read::Input {attachment_id:part.metadata.attachment_id.clone(),cursor:None,queries:None,start_line:None,end_line:None,ignore_case:false,before_context:0,after_context:0,max_bytes:Some(512)};
            let mut body=String::new();let mut pages=0;
            loop {
                let (wire,proof)=model_read::text_page_with_references(&part,&input,"read",100000,&crate::model_observability::tool::ToolObservation::default(),&mut refs).unwrap();
                assert_eq!(proof.source_sha256,original.metadata.sha256);
                assert_eq!(proof.page_sha256,crate::conversation_attachment::digest(wire.as_bytes()));
                let page:ReadPage=serde_json::from_str(&wire).unwrap();
                for line in page.lines { body.push_str(&line.text); }
                pages+=1;
                let Some(cursor)=page.cursor else { break; };
                let call=crate::chat::ToolCall {id:"next".into(),name:"read_conversation_attachment".into(),arguments_json:json!({"attachment_id":input.attachment_id,"cursor":cursor}).to_string()};
                let (expanded,_)=refs.expand_call(&call).unwrap();
                input.cursor=serde_json::from_str::<Value>(&expanded.arguments_json).unwrap()["cursor"].as_str().map(str::to_owned);
            }
            assert!(pages>1);
            let view:Value=serde_json::from_str(&body).unwrap();
            assert_eq!(view["conversation_id"],"cv1");
            assert!(view["messages"][0].get("message_id").is_none());
            assert!(view["messages"][0].get("revision").is_none());
            assert_eq!(view["messages"][0]["content"],raw["messages"][0]["content"]);
            assert_eq!(part.content,original.content);part.metadata.verify(&part.content).unwrap();
            let mut inline=result.clone();project_result_view(&mut inline,Some(&refs));
            assert_eq!(serde_json::from_str::<Value>(&inline.text).unwrap(),view);
        }).unwrap().join().unwrap();
    }
}

#[test]
fn memory_history_model_view_keeps_native_ids_versions_and_recording_times() {
    let memory = crate::native_ids::stable(crate::native_ids::Kind::Memory, &"history-memory");
    let source = crate::native_ids::stable(crate::native_ids::Kind::AssistantSource, &"old-source");
    let raw = json!({"memory_id":memory,"current_revision":2,"versions":[{
        "revision":1,"content":"Call me sss","context":null,"status":"superseded",
        "recorded_at_unix_ms":1234,"source_ids":[source]}],"has_more":false,"next_cursor":null,"partial":false,"reasons":[]});
    let request = crate::seam::ModelRequest::text_only(
        history("read_memory_history", raw),
        crate::prompt::ResponseFormatSpec::None,
    );
    let view = project_request(&request);
    let result: Value = serde_json::from_str(&view.messages.last().unwrap().text).unwrap();
    assert_eq!(result["memory_id"], memory);
    assert_eq!(result["versions"][0]["source_ids"][0], source);
    assert_eq!(result["versions"][0]["revision"], 1);
    assert_eq!(result["versions"][0]["recorded_at_unix_ms"], 1234);
    assert!(result["versions"][0].get("context").is_none());
}
