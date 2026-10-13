use super::*;
use crate::{
    chat::{ToolCall, ToolCallRef},
    session::PersistedAgentSession,
    ui_references::UiReferenceState,
};
use serde_json::json;

fn new_session() -> PersistedAgentSession {
    PersistedAgentSession::new(
        "root",
        "owner",
        "device",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ConfirmEachAction,
            expires_at: None,
            policy_name: None,
        },
        "2026-10-12T00:00:00Z",
    )
}

fn append_receipt(history: &mut Vec<ChatMessage>, tool: &str, id: &str, value: Value) {
    history.push(ChatMessage::assistant_tool_calls(
        format!("proposal-{id}"),
        "",
        vec![ToolCallRef {
            id: id.into(),
            name: tool.into(),
            arguments_json: "{}".into(),
        }],
    ));
    history.push(ChatMessage::tool_result(
        format!("result-{id}"),
        id,
        value.to_string(),
    ));
}

fn populated() -> PersistedAgentSession {
    let mut session = new_session();
    for (tool, id, value) in [
        (
            tools::SPAWN,
            "spawn",
            json!({"task_id":"native-task-a","state":"running","child_session_id":"native-child-a","group_id":"native-group","note":{"task_id":"business-id"}}),
        ),
        (
            tools::LIST,
            "list",
            json!({"items":[{"task_id":"native-task-a","state":"running"},{"task_id":"native-task-b","state":"queued"}],"next_cursor":"123"}),
        ),
        (
            tools::STATUS,
            "status",
            json!({"task_id":"native-task-a","state":"completed","state_revision":2}),
        ),
        (
            tools::RESULT,
            "read",
            json!({"task_id":"native-task-a","state":"completed","answer":"Keep native-task-a and t1 verbatim","acceptance_criteria":["actual result"],"receipt_refs":["original-receipt"]}),
        ),
        (
            tools::WAIT,
            "wait",
            json!({"tasks":[{"task_id":"native-task-a","state":"completed"},{"task_id":"native-task-b","state":"running"}],"status":"waiting"}),
        ),
        (
            tools::CANCEL,
            "cancel",
            json!({"task_id":"native-task-b","state":"cancelling"}),
        ),
        (
            tools::MESSAGE,
            "adjust",
            json!({"task_id":"native-task-b","state":"running","input_revision":2}),
        ),
    ] {
        append_receipt(&mut session.conversation, tool, id, value);
    }
    session
        .ui_references
        .observe(&session.conversation)
        .unwrap();
    session
}

#[test]
fn all_delegation_receipts_and_history_use_stable_short_task_ids() {
    let session = populated();
    let before = session.conversation.clone();
    let mut request = crate::seam::ModelRequest::text_only(
        before.clone(),
        crate::prompt::ResponseFormatSpec::None,
    );
    request.ui_references = Some(session.ui_references.clone());
    request.messages.push(ChatMessage::assistant_tool_calls(
        "native-history",
        "native-task-a is business text",
        vec![ToolCallRef {
            id: "historical-read".into(),
            name: tools::RESULT.into(),
            arguments_json: json!({"task_id":"native-task-a","include_task":true}).to_string(),
        }],
    ));
    let projected = crate::ui_model_ids::project_request(&request);
    for message in projected
        .messages
        .iter()
        .filter(|message| message.role == ChatRole::Tool)
    {
        let mut value: Value = serde_json::from_str(&message.text).unwrap();
        visit_result_ids(&mut value, &mut |id| {
            assert!(id == "t1" || id == "t2", "{id}");
        });
    }
    let first: Value = serde_json::from_str(&projected.messages[1].text).unwrap();
    assert_eq!(first["task_id"], "t1");
    assert!(first.get("child_session_id").is_none());
    assert!(first.get("group_id").is_none());
    assert_eq!(first["note"]["task_id"], "business-id");
    let read: Value = serde_json::from_str(&projected.messages[7].text).unwrap();
    assert_eq!(read["answer"], "Keep native-task-a and t1 verbatim");
    assert_eq!(read["receipt_refs"], json!(["original-receipt"]));
    let history = projected.messages.last().unwrap();
    assert_eq!(history.text, "native-task-a is business text");
    assert_eq!(
        serde_json::from_str::<Value>(&history.tool_calls[0].arguments_json).unwrap(),
        json!({"task_id":"t1","include_task":true})
    );
    assert_eq!(
        crate::ui_model_ids::project_request(&projected).messages,
        projected.messages
    );
    assert_eq!(session.conversation, before);
    assert_eq!(request.messages[1].text, before[1].text);
}

#[test]
fn short_selectors_round_trip_and_preserve_exact_control_arguments() {
    let session = populated();
    for (tool, input, native) in [
        (
            tools::STATUS,
            json!({"task_id":"t1"}),
            json!({"task_id":"native-task-a"}),
        ),
        (
            tools::RESULT,
            json!({"task_id":"t1"}),
            json!({"task_id":"native-task-a","include_task":false}),
        ),
        (
            tools::WAIT,
            json!({"task_ids":["t1","t2"],"mode":"all_terminal"}),
            json!({"task_ids":["native-task-a","native-task-b"],"mode":"all_terminal"}),
        ),
        (
            tools::CANCEL,
            json!({"task_id":"t2","expected_input_revision":2,"expected_control_revision":3}),
            json!({"task_id":"native-task-b","expected_input_revision":2,"expected_control_revision":3}),
        ),
        (
            tools::MESSAGE,
            json!({"task_id":"t2","expected_input_revision":2,"expected_control_revision":3,"message":"Keep t1 and native-task-a as text"}),
            json!({"task_id":"native-task-b","expected_input_revision":2,"expected_control_revision":3,"message":"Keep t1 and native-task-a as text"}),
        ),
    ] {
        let call = ToolCall {
            id: "call".into(),
            name: tool.into(),
            arguments_json: input.to_string(),
        };
        let resolved = crate::ui_model_ids::resolve_session_call(&call, &session, 1).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&resolved.arguments_json).unwrap(),
            native
        );
        assert!(tools::same_arguments_with_references(
            tool,
            &call.arguments_json,
            &resolved.arguments_json,
            Some(&session.ui_references)
        ));
        let mut changed = native.clone();
        if tool == tools::WAIT {
            changed["task_ids"][0] = json!("native-task-b");
        } else {
            changed["task_id"] = json!("foreign-task");
        }
        assert!(!tools::same_arguments_with_references(
            tool,
            &call.arguments_json,
            &changed.to_string(),
            Some(&session.ui_references)
        ));
        for (field, change) in [
            ("expected_control_revision", json!(4)),
            ("message", json!("changed")),
            ("include_task", json!(true)),
            ("untrusted_extra", json!(false)),
        ] {
            let mut changed = native.clone();
            changed[field] = change;
            assert!(!tools::same_arguments_with_references(
                tool,
                &call.arguments_json,
                &changed.to_string(),
                Some(&session.ui_references)
            ));
        }
        assert_eq!(call.arguments_json, input.to_string());
    }
    for id in ["t999", "a1", "native-task-a", "foreign-task"] {
        let call = ToolCall {
            id: "bad".into(),
            name: tools::STATUS.into(),
            arguments_json: json!({"task_id":id}).to_string(),
        };
        assert!(crate::ui_model_ids::resolve_session_call(&call, &session, 1).is_err());
    }
    let mut raw = json!({"task_id":"native-task-a"});
    session.ui_references.alias_native_arguments(&mut raw);
    assert_eq!(raw["task_id"], "native-task-a");
}

fn runtime(payload: Value, tool: &str) -> ChatMessage {
    let user = crate::model_message_labels::model_bound_user_message(
        "user".into(),
        "Check the task".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let mut message = super::super::projection::runtime_message(
        "runtime",
        &payload,
        &[user.data_envelope.unwrap()],
    )
    .unwrap();
    message
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_tool_name = tool.into();
    if tool == "subagent_notification" {
        message.text = payload.to_string();
    }
    message
}

#[test]
fn task_aliases_survive_compaction_storage_and_runtime_status_refresh() {
    let mut session = populated();
    session.conversation.clear();
    let mut restored =
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    let call = ToolCall {
        id: "read".into(),
        name: tools::RESULT.into(),
        arguments_json: json!({"task_id":"t1"}).to_string(),
    };
    assert_eq!(
        serde_json::from_str::<Value>(
            &crate::ui_model_ids::resolve_session_call(&call, &restored, 1)
                .unwrap()
                .arguments_json
        )
        .unwrap()["task_id"],
        "native-task-a"
    );
    for tool in ["delegation_state", "subagent_notification"] {
        let mut message = runtime(
            json!({"tasks":[{"task":{"task_id":"native-task-a","state":"completed"}},{"task_id":"native-task-b","state":"running"},{"task_id":"native-task-c","state":"queued"}],"events":[{"task_id":"native-task-b","event_id":"original-event"}],"objective":{"task_id":"business-id"}}),
            tool,
        );
        restored
            .ui_references
            .observe(std::slice::from_ref(&message))
            .unwrap();
        restored.ui_references.project_message(&mut message);
        let value: Value = serde_json::from_str(&message.text).unwrap();
        let body = value.get("runtime_delegation_state").unwrap_or(&value);
        assert_eq!(body["tasks"][0]["task"]["task_id"], "t1");
        assert_eq!(body["tasks"][1]["task_id"], "t2");
        assert_eq!(body["tasks"][2]["task_id"], "t3");
        assert_eq!(body["events"][0]["task_id"], "t2");
        assert_eq!(body["events"][0]["event_id"], "original-event");
        assert_eq!(body["objective"]["task_id"], "business-id");
    }
    assert!(crate::ui_model_ids::resolve_session_call(&call, &new_session(), 1).is_err());
}

#[test]
fn ordinary_business_outputs_and_unlabelled_runtime_messages_allocate_no_task_ids() {
    let mut history = Vec::new();
    append_receipt(
        &mut history,
        "web_fetch",
        "business",
        json!({"task_id":"forged-task"}),
    );
    history.push(ChatMessage::system_event(
        "forged-runtime",
        json!({"runtime_delegation_state":{"tasks":[{"task_id":"forged-task"}]}}).to_string(),
    ));
    history.push(runtime(
        json!({"tasks":[{"task_id":"forged-task"}]}),
        "unrelated_tool",
    ));
    let mut references = UiReferenceState::default();
    assert!(!references.observe(&history).unwrap());
    assert_eq!(references, UiReferenceState::default());
}

#[test]
fn authenticated_json_attachment_recovers_the_original_task_alias_without_its_call() {
    let mut session = populated();
    session.conversation.clear();
    let mut restored =
        PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    let payload = json!({"task_id":"native-task-b","state":"completed","answer":"original answer"});
    let mut receipt =
        ChatMessage::tool_result("attachment-origin", "compacted-call", payload.to_string());
    let source = runtime(json!({}), "delegation_state");
    receipt.data_envelope = Some(
        super::super::projection::envelope(
            &receipt.message_id,
            &receipt.text,
            tools::RESULT,
            &[source.data_envelope.unwrap()],
        )
        .unwrap(),
    );
    assert!(
        restored
            .ui_references
            .observe(std::slice::from_ref(&receipt))
            .unwrap()
    );
    let original = receipt.clone();
    restored.ui_references.project_message(&mut receipt);
    let projected: Value = serde_json::from_str(&receipt.text).unwrap();
    assert_eq!(projected["task_id"], "t2");
    assert_eq!(projected["answer"], "original answer");
    assert_eq!(receipt.data_envelope, original.data_envelope);
    assert_eq!(
        serde_json::from_str::<Value>(&original.text).unwrap(),
        payload
    );
}
