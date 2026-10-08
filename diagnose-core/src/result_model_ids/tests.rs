use super::*;
use crate::{chat::ToolCall, session::PersistedAgentSession};
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    computer_use::{FileContentReadOutput, ObjectKind, ObjectRef},
    data_lineage::{ContentRef, DestinationIdentity},
};

pub(crate) fn evidence(tool: &str, id: &str, arguments: Value, result: Value) -> Vec<ChatMessage> {
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
    let text = result.to_string();
    let mut receipt = ChatMessage::tool_result("receipt", id, &text);
    receipt.data_envelope = crate::model_message_labels::internal_tool_result_envelope(
        owner.data_envelope.as_ref(),
        id,
        &text,
        tool,
    )
    .unwrap();
    let registry = crate::ai_assistant::ai_assistant_provider_registry();
    let descriptor = registry.capability_for_tool(tool).unwrap();
    receipt
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_provider_id = registry
        .provider_for_capability(&descriptor.wire.capability_id)
        .unwrap()
        .wire
        .provider_id
        .clone();
    vec![
        owner,
        ChatMessage::assistant_tool_calls(
            "source",
            "",
            vec![ToolCallRef {
                id: id.into(),
                name: tool.into(),
                arguments_json: arguments.to_string(),
            }],
        ),
        receipt,
    ]
}

fn read_history() -> Vec<ChatMessage> {
    let read = FileContentReadOutput {
        file: ObjectRef {
            token: "file".into(),
            snapshot_id: "read-snapshot".into(),
            object_kind: ObjectKind::File,
            expires_at: "2099-01-01T00:00:00Z".into(),
        },
        display_name: "notes.txt".into(),
        content_utf8: "old".into(),
        byte_len: 3,
        sha256: format!("{:x}", Sha256::digest(b"old")),
    };
    evidence(
        "read_text_file",
        "provider-call-long",
        json!({}),
        json!(desk_agent_protocol::OperationOutput::ReadContext(
            desk_agent_protocol::ReadContextOutput::FileContentRead(read)
        )),
    )
}

#[test]
fn file_version_recovers_exact_hash_and_approval_input() {
    let history = read_history();
    let mut projected = history.last().unwrap().clone();
    crate::ui_model_ids::project_tool_message(&mut projected);
    let output: Value = serde_json::from_str(&projected.text).unwrap();
    assert!(output["file_version_id"].as_str().unwrap().len() < 32);
    assert!(!projected.text.contains("sha256"));
    let input = json!({"file_version_id":output["file_version_id"],"change":{"kind":"replace_all","content_utf8":"新内容"}});
    let call = ToolCall {
        id: "change".into(),
        name: "update_text_file".into(),
        arguments_json: input.to_string(),
    };
    let resolved = crate::ui_model_ids::resolve_call(&call, &history, 1).unwrap();
    let canonical: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
    assert_eq!(
        canonical["expected_sha256"],
        format!("{:x}", Sha256::digest(b"old"))
    );
    assert_eq!(canonical["file_result_call_id"], "provider-call-long");
    assert!(crate::ui_model_ids::same_call_input(
        &call.name,
        &call.arguments_json,
        &resolved.arguments_json
    ));
    let mut changed = canonical.clone();
    changed["change"]["content_utf8"] = json!("different");
    assert!(!crate::ui_model_ids::same_call_input(
        &call.name,
        &call.arguments_json,
        &changed.to_string()
    ));
    let permission = ToolCall {
        id: "permission".into(),
        name: "request_permissions".into(),
        arguments_json:
            json!({"items":[{"tool_name":call.name,"reason":"edit","exact_input":input}]})
                .to_string(),
    };
    let resolved = crate::ui_model_ids::resolve_call(&permission, &history, 1).unwrap();
    assert!(crate::ui_model_ids::same_call_input(
        &permission.name,
        &permission.arguments_json,
        &resolved.arguments_json
    ));
    let native: Value = serde_json::from_str(&resolved.arguments_json).unwrap();
    assert_eq!(native["items"][0]["item_id"], "item_1");
}

#[test]
fn file_references_survive_serialization_but_reject_missing_tampered_expired_and_duplicate_sources()
{
    let history = read_history();
    let args = json!({"file_version_id":alias("file","provider-call-long")});
    let restored: Vec<ChatMessage> =
        serde_json::from_str(&serde_json::to_string(&history).unwrap()).unwrap();
    let mut value = args.clone();
    resolve("delete_text_file", &mut value, &restored, 1).unwrap();
    for variant in 0..6 {
        let mut history = history.clone();
        match variant {
            0 => history.clear(),
            1 => history.last_mut().unwrap().text.push(' '),
            2 => {
                history
                    .last_mut()
                    .unwrap()
                    .data_envelope
                    .as_mut()
                    .unwrap()
                    .provenance
                    .source_provider_id = "forged".into()
            }
            3 => {
                history
                    .last_mut()
                    .unwrap()
                    .data_envelope
                    .as_mut()
                    .unwrap()
                    .retention
                    .expires_at_unix_ms = Some(1)
            }
            4 => history.push(history[1].clone()),
            _ => history.pop().map(|_| ()).unwrap(),
        }
        assert!(
            resolve("delete_text_file", &mut args.clone(), &history, 2).is_err(),
            "variant {variant}"
        );
    }
    assert!(
        resolve(
            "delete_text_file",
            &mut json!({"file_version_id":alias("draft","provider-call-long")}),
            &history,
            1
        )
        .is_err()
    );
    let mut native =
        json!({"file_result_call_id":"provider-call-long","expected_sha256":"f".repeat(64)});
    assert!(resolve("delete_text_file", &mut native, &history, 1).is_err());
}

#[test]
fn result_selectors_roundtrip_and_ambiguous_aliases_fail_closed() {
    let history = read_history();
    let input = json!({"file_result_call_id":alias("result","provider-call-long")});
    let call = ToolCall {
        id: "read-again".into(),
        name: "read_text_file".into(),
        arguments_json: input.to_string(),
    };
    let resolved = crate::ui_model_ids::resolve_call(&call, &history, 1).unwrap();
    assert!(crate::ui_model_ids::same_call_input(
        &call.name,
        &call.arguments_json,
        &resolved.arguments_json
    ));
    let source = history[1].tool_calls[0].clone();
    assert!(
        unique_result(&[(source.clone(), json!(1)), (source, json!(2))], |_, _| {
            true
        })
        .is_err()
    );
}

#[test]
fn attachments_restore_entire_artifact_and_freeze_only_matching_lineage() {
    let read = read_history();
    let result: Value = serde_json::from_str(&read.last().unwrap().text).unwrap();
    let file: ObjectRef =
        serde_json::from_value(result["ReadContext"]["FileContentRead"]["file"].clone()).unwrap();
    let digest = format!("{:x}", Sha256::digest(b"old"));
    let artifact = CreatedFileArtifactOutput {
        file,
        file_name: "notes.txt".into(),
        media_type: crate::provider_preflight::TEXT_ARTIFACT_MEDIA_TYPE.into(),
        size_bytes: 3,
        digest_sha256: digest.clone(),
        content: ContentRef::Artifact {
            artifact_id: "file".into(),
            sha256: digest,
            size_bytes: 3,
            media_type: crate::provider_preflight::TEXT_ARTIFACT_MEDIA_TYPE.into(),
        },
    };
    let completed = ComputerActionCompleted {
        work_id: "work".into(),
        action_request_id: "create".into(),
        execution_generation: "generation".into(),
        result: ComputerActionResultClass::Verified,
        facts: vec![],
        message: None,
        output: Some(ComputerActionOutput::FileArtifact(artifact.clone())),
    };
    let mut history = evidence(
        "create_text_file",
        "create",
        json!({"content_utf8":"old"}),
        json!(completed),
    );
    let mut other = history.last().unwrap().clone();
    other.text = json!({"status":"pending"}).to_string();
    other.data_envelope.as_mut().unwrap().envelope_id = "other".into();
    history.push(other);
    let mut value = json!({"draft":{"recipients":[{"address":"a@example.test"}],"subject":"a","body_plain_text":"body"},"attachment":{"artifact_result_id":artifact_id(&artifact),"element":"upload-id"}});
    resolve("prepare_gmail_draft", &mut value, &history, 1).unwrap();
    assert_eq!(value["attachment"]["artifact"], json!(artifact));
    assert_eq!(value["draft"]["attachment_labels"], json!(["notes.txt"]));
    let ids = input_envelope_ids("prepare_gmail_draft", &value, &history).unwrap();
    assert_eq!(
        ids,
        vec![
            history[2]
                .data_envelope
                .as_ref()
                .unwrap()
                .envelope_id
                .clone()
        ]
    );
    let mut another = artifact.clone();
    another.file.token = "another-file".into();
    assert_ne!(artifact_id(&artifact), artifact_id(&another));
}

#[test]
fn exact_send_restores_original_body_and_rejects_missing_or_conflicting_input() {
    for gmail in [true, false] {
        let (prepare, send, handoff, original) = if gmail {
            let input = crate::communication::test_support::gmail_exact_send_input();
            (
                "prepare_gmail_draft",
                "send_gmail_message",
                input.handoff,
                json!({"draft":input.draft}),
            )
        } else {
            let input = crate::communication::test_support::slack_exact_send_input();
            (
                "prepare_slack_message",
                "send_slack_message",
                input.handoff,
                json!({"body_plain_text":input.body_plain_text}),
            )
        };
        let history = evidence(prepare, "prepare", original.clone(), json!(handoff));
        let input =
            json!({"handoff_result_id":alias("draft",&handoff.handoff_id),"page":"new-page"});
        let mut resolved = input.clone();
        resolve(send, &mut resolved, &history, 1).unwrap();
        assert_eq!(resolved["handoff"], json!(handoff));
        let field = if gmail { "draft" } else { "body_plain_text" };
        assert_eq!(resolved[field], original[field]);
        assert!(crate::ui_model_ids::same_call_input(
            send,
            &input.to_string(),
            &resolved.to_string()
        ));
        let mut conflict = input.clone();
        conflict[field] = original[field].clone();
        assert!(resolve(send, &mut conflict, &history, 1).is_err());
        let missing = evidence(prepare, "prepare", json!({}), json!(handoff));
        assert!(resolve(send, &mut input.clone(), &missing, 1).is_err());
        let mut model = history.last().unwrap().clone();
        project_result_message(&mut model);
        assert!(!model.text.contains("sha256"));
        assert!(!model.text.contains("Reviewed"));
        assert!(model.text.contains("message_sent"));
        assert!(model.text.contains("handoff_result_id"));
    }
}

#[test]
fn nested_legacy_exact_evidence_cannot_be_discarded_by_projection() {
    let original = json!({"items":[{"tool_name":"delete_text_file","exact_input":{"file_result_call_id":"read","expected_sha256":"a".repeat(64)}}]});
    let mut changed = original.clone();
    changed["items"][0]["exact_input"]["expected_sha256"] = json!("b".repeat(64));
    assert!(!crate::ui_model_ids::same_call_input(
        "request_permissions",
        &original.to_string(),
        &changed.to_string()
    ));
}

#[test]
fn search_sources_restore_exact_title_and_url_without_retyping() {
    let source = json!({"title":"Original title","url":"https://example.test/a"});
    let history = evidence(
        "search_public_web",
        "search",
        json!({"query":"original"}),
        json!({"results":[source.clone()]}),
    );
    let mut model = history.last().unwrap().clone();
    project_result_message(&mut model);
    let output: Value = serde_json::from_str(&model.text).unwrap();
    let input = json!({"preview_id":"preview","file_name":"report.docx","title":"Report","web_search_result_id":output["search_result_id"],"web_source_ids":[output["results"][0]["source_id"]]});
    let mut canonical = input.clone();
    resolve("create_word_report", &mut canonical, &history, 1).unwrap();
    assert_eq!(canonical["web_sources"], json!([source]));
    assert!(crate::ui_model_ids::same_call_input(
        "create_word_report",
        &input.to_string(),
        &canonical.to_string()
    ));
    let mut forged = input.clone();
    forged["web_source_ids"] = json!([alias("source", "invented")]);
    assert!(resolve("create_word_report", &mut forged, &history, 1).is_err());
}

#[test]
fn short_ids_are_independent_of_process_state_and_context_projection() {
    let mut session = PersistedAgentSession::new(
        "run",
        "owner",
        "device",
        1,
        AgentScope {
            granted: vec![],
            expires_at: None,
            mode: ExecutionMode::ReadOnly,
            policy_name: None,
        },
        "2026-01-01T00:00:00Z",
    );
    session.conversation = read_history();
    let before = alias("file", "provider-call-long");
    let mut clone = session.conversation.last().unwrap().clone();
    project_result_message(&mut clone);
    let restored: PersistedAgentSession =
        serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
    let mut input = json!({"file_version_id":before});
    resolve("delete_text_file", &mut input, &restored.conversation, 1).unwrap();
    assert!(session.conversation.last().unwrap().text.contains("sha256"));
    let mut again = clone.clone();
    project_result_message(&mut again);
    assert_eq!(again.text, clone.text);
}
