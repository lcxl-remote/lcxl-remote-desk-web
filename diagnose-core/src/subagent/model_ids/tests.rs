use super::*;
use crate::{
    chat::{ToolCall, ToolCallRef},
    native_ids::{self, Kind},
    ui_references::UiReferenceState,
};
use serde_json::json;

#[test]
fn native_tasks_are_presented_and_accepted_as_aliases() {
    let id = native_ids::stable(Kind::Task, &("root", "group", "owner", "device", "call"));
    let mut references = UiReferenceState::default();
    let messages = vec![
        ChatMessage::assistant_tool_calls(
            "proposal",
            "",
            vec![ToolCallRef {
                id: "call".into(),
                name: tools::SPAWN.into(),
                arguments_json: "{}".into(),
            }],
        ),
        ChatMessage::tool_result(
            "result",
            "call",
            json!({"task_id":id,"state":"running"}).to_string(),
        ),
    ];
    assert!(references.observe(&messages).unwrap());
    let mut projected = messages[1].clone();
    references.project_message(&mut projected);
    assert_eq!(
        serde_json::from_str::<Value>(&projected.text).unwrap()["task_id"],
        "task1"
    );
    for (tool, aliased, native) in [
        (
            tools::STATUS,
            json!({"task_id":"task1"}),
            json!({"task_id":id}),
        ),
        (
            tools::REPORTS,
            json!({"request_id":"task1"}),
            json!({"request_id":id}),
        ),
        (
            tools::WAIT_REPORTS,
            json!({"requests":[{"request_id":"task1","after_seq":0}],"kinds":["completed"]}),
            json!({"requests":[{"request_id":id,"after_seq":0}],"kinds":["completed"]}),
        ),
    ] {
        for arguments in [&aliased, &native] {
            let call = ToolCall {
                id: "read".into(),
                name: tool.into(),
                arguments_json: arguments.to_string(),
            };
            let (resolved, expected) = references.expand_call(&call).unwrap();
            assert!(expected.is_empty());
            assert_eq!(
                serde_json::from_str::<Value>(&resolved.arguments_json).unwrap(),
                native
            );
        }
        let mut recorded = native.clone();
        references.alias_selector_ids(&mut recorded);
        assert_eq!(recorded, aliased);
    }
    let call = ToolCall {
        id: "read".into(),
        name: tools::STATUS.into(),
        arguments_json: json!({"task_id":"task9"}).to_string(),
    };
    assert!(references.expand_call(&call).is_err());
}

#[test]
fn internal_metadata_is_omitted_without_touching_deliverables() {
    let id = native_ids::stable(Kind::Task, &"creation");
    let worker_id = native_ids::stable(Kind::Worker, &"creation");
    let mut value = json!({
        "wait_id":"internal-wait", "tasks":[{
            "task_id":id,"worker_id":worker_id,"state":"completed","child_session_id":"child","group_id":"group",
            "answer":"Keep child and group verbatim", "note":{"task_id":"business-id"},
            "receipt_refs":["original-proof"]
        }]
    });
    omit_internal_selectors(&mut value);
    assert!(value.get("wait_id").is_none());
    assert!(value["tasks"][0].get("child_session_id").is_none());
    assert!(value["tasks"][0].get("group_id").is_none());
    assert_eq!(value["tasks"][0]["task_id"], id);
    assert_eq!(value["tasks"][0]["worker_id"], worker_id);
    assert_eq!(value["tasks"][0]["note"]["task_id"], "business-id");
    assert_eq!(value["tasks"][0]["answer"], "Keep child and group verbatim");
    assert_eq!(value["tasks"][0]["receipt_refs"], json!(["original-proof"]));
}

#[test]
fn native_caller_comparison_keeps_control_inputs_exact() {
    let id = native_ids::stable(Kind::Task, &"creation");
    let original = json!({"request_id":id}).to_string();
    assert!(tools::same_arguments(
        tools::REPORTS,
        &original,
        &json!({"request_id":id,"after_seq":0}).to_string()
    ));
    for changed in [
        json!({"request_id":"other","after_seq":0}),
        json!({"request_id":id,"after_seq":1}),
        json!({"request_id":id,"after_seq":0,"unknown":1}),
    ] {
        assert!(!tools::same_arguments(
            tools::REPORTS,
            &original,
            &changed.to_string()
        ));
    }
}

#[test]
fn runtime_state_reuses_aliases_and_recorded_calls_keep_them() {
    let owner = crate::model_message_labels::model_bound_user_message(
        "owner".into(),
        "Delegate the check".into(),
        desk_agent_protocol::data_lineage::DestinationIdentity::Model {
            connection_id: "test".into(),
            connection_revision: 1,
            model_id: "test".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let issued = native_ids::stable(Kind::Task, &"issued");
    let runtime_only = native_ids::stable(Kind::Task, &"runtime-only");
    let state =
        json!({"runtime_delegation_state":{"tasks":[{"task_id":runtime_only}]}}).to_string();
    let mut runtime = ChatMessage::system_event("runtime", state.clone());
    runtime.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        owner.data_envelope.as_ref(),
        "runtime",
        &state,
        "delegation_state",
    )
    .unwrap();
    assert!(is_runtime(&runtime));
    let conversation = vec![
        owner,
        ChatMessage::assistant_tool_calls(
            "proposal",
            "",
            vec![ToolCallRef {
                id: "call".into(),
                name: tools::SPAWN.into(),
                arguments_json: "{}".into(),
            }],
        ),
        ChatMessage::tool_result("result", "call", json!({"task_id":issued}).to_string()),
        ChatMessage::assistant_tool_calls(
            "status",
            "",
            vec![ToolCallRef {
                id: "status-call".into(),
                name: tools::STATUS.into(),
                arguments_json: json!({"task_id":"task1"}).to_string(),
            }],
        ),
    ];
    let mut projected = conversation.clone();
    projected.push(runtime);
    let mut references = UiReferenceState::default();
    assert!(references.observe(&projected).unwrap());
    let sources = conversation
        .iter()
        .map(|message| message.message_id.clone())
        .collect();
    let protected = references.retained_call_aliases(&conversation, &[]);
    assert!(protected.contains("task1"));
    assert!(!references.retain_sources(&sources, &protected));
    assert!(!references.observe(&projected).unwrap());
    assert!(!references.retain_sources(&sources, &protected));
    assert_eq!(
        serde_json::to_value(&references).unwrap()["entries"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    // A recorded call keeps its alias after the issuing result leaves the conversation.
    assert!(references.retain_sources(&Default::default(), &protected));
    let call = ToolCall {
        id: "status-call".into(),
        name: tools::STATUS.into(),
        arguments_json: json!({"task_id":"task1"}).to_string(),
    };
    assert!(references.expand_call(&call).is_ok());
}
