use super::*;
use crate::{
    chat::{ToolCall, ToolCallRef},
    native_ids::{self, Kind},
    ui_references::UiReferenceState,
};
use serde_json::json;

#[test]
fn native_tasks_do_not_allocate_or_resolve_aliases() {
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
    assert!(!references.observe(&messages).unwrap());
    assert_eq!(
        serde_json::to_value(&references).unwrap()["entries"],
        json!({})
    );
    for (tool, args) in [
        (tools::STATUS, json!({"task_id":id})),
        (tools::RESULT, json!({"task_id":id})),
        (tools::WAIT, json!({"task_ids":[id],"mode":"all"})),
    ] {
        let call = ToolCall {
            id: "read".into(),
            name: tool.into(),
            arguments_json: args.to_string(),
        };
        let (resolved, expected) = references.expand_call(&call).unwrap();
        assert!(expected.is_empty());
        assert_eq!(
            serde_json::from_str::<Value>(&resolved.arguments_json).unwrap(),
            args
        );
    }
}

#[test]
fn internal_metadata_is_omitted_without_touching_deliverables() {
    let id = native_ids::stable(Kind::Task, &"creation");
    let mut value = json!({
        "wait_id":"internal-wait", "tasks":[{
            "task_id":id,"state":"completed","child_session_id":"child","group_id":"group",
            "answer":"Keep child and group verbatim", "note":{"task_id":"business-id"},
            "receipt_refs":["original-proof"]
        }]
    });
    omit_internal_selectors(&mut value);
    assert!(value.get("wait_id").is_none());
    assert!(value["tasks"][0].get("child_session_id").is_none());
    assert!(value["tasks"][0].get("group_id").is_none());
    assert_eq!(value["tasks"][0]["task_id"], id);
    assert_eq!(value["tasks"][0]["note"]["task_id"], "business-id");
    assert_eq!(value["tasks"][0]["answer"], "Keep child and group verbatim");
    assert_eq!(value["tasks"][0]["receipt_refs"], json!(["original-proof"]));
}

#[test]
fn native_caller_comparison_keeps_control_inputs_exact() {
    let id = native_ids::stable(Kind::Task, &"creation");
    let original = json!({"task_id":id}).to_string();
    assert!(tools::same_arguments(
        tools::RESULT,
        &original,
        &json!({"task_id":id,"include_task":false}).to_string()
    ));
    for changed in [
        json!({"task_id":"other","include_task":false}),
        json!({"task_id":id,"include_task":true}),
        json!({"task_id":id,"include_task":false,"unknown":1}),
    ] {
        assert!(!tools::same_arguments(
            tools::RESULT,
            &original,
            &changed.to_string()
        ));
    }
}
