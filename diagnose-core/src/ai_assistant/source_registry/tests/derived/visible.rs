use super::*;
use crate::{
    ai_assistant::source::prepare_capture,
    ai_assistant::{
        output_archive::{ArchivedAssistantOutput, VisibleAssistantMessage},
        source::HistoryCapture,
    },
    chat::{ToolCall, ToolCallRef},
};
use desk_agent_protocol::ai_assistant::{
    source::AssistantSourceKind, user_instructions::AssistantDataPolicy,
};

#[test]
fn a_visible_answer_uses_its_actual_message_id_original_completion_time_and_statement_timezone() {
    let archived = native_archive_with_turn(
        "visible",
        vec![owner_message()],
        &[],
        &ModelTurn {
            text: "answer-visible".into(),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        },
        "Asia/Shanghai",
    );
    let mut message = answer(&archived, "visible");
    message.reasoning = Some("Presentation-only reasoning must not enter history".into());
    let bound = archived.bind_visible_message(&message).unwrap();
    let capture = history_capture(&archived, &message, &bound).unwrap();
    assert_eq!(capture.native_id, message.message_id);
    assert_eq!(capture.content, "answer-visible");
    assert_eq!(capture.actor_id, archived.source_set.actor_id);
    assert_eq!(capture.device_ref, archived.source_set.source_device_ref);
    assert_eq!(capture.kind, AssistantSourceKind::AssistantMessage);
    assert_eq!(
        (capture.recorded_at_unix_ms, capture.statement_timezone),
        (110, "Asia/Shanghai")
    );
    assert_ne!(
        capture.recorded_at_unix_ms,
        archived.input_recorded_at_unix_ms
    );
    let prepared = prepare_capture(&capture, &AssistantDataPolicy::default())
        .unwrap()
        .unwrap();
    assert_eq!(prepared.retained_content, "answer-visible");
    assert_eq!(
        prepared.proof.original_envelope_id,
        archived.output.envelope_id
    );
    assert_eq!(prepared.proof.recorded_at_unix_ms, 110);
}

#[test]
fn a_valid_label_cannot_turn_rewritten_text_a_control_summary_or_another_role_into_an_archived_answer()
 {
    let archived = native_archive("visible", vec![owner_message()], &[]);
    let original = answer(&archived, "visible");
    for text in ["different answer", "Goal completed summary", ""] {
        let mut changed = original.clone();
        changed.text = text.into();
        assert!(archived.bind_visible_message(&changed).is_err());
    }
    for role in [
        ChatRole::User,
        ChatRole::Tool,
        ChatRole::SystemEvent,
        ChatRole::UntrustedOutput,
    ] {
        let mut changed = original.clone();
        changed.role = role;
        assert!(archived.bind_visible_message(&changed).is_err());
    }
    let mut changed = original.clone();
    changed
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_tool_name = "changed-producer".into();
    assert!(archived.bind_visible_message(&changed).is_err());
    let mut changed = original.clone();
    changed.message_id.clear();
    assert!(archived.bind_visible_message(&changed).is_err());
    let mut changed = original.clone();
    changed.tool_call_id = Some("another-tool".into());
    assert!(archived.bind_visible_message(&changed).is_err());
    let mut changed = original;
    changed.tool_calls.push(ToolCallRef {
        id: "call".into(),
        name: "tool".into(),
        arguments_json: "{}".into(),
    });
    assert!(archived.bind_visible_message(&changed).is_err());
}

#[test]
fn visible_tool_call_text_binds_the_entire_original_call_payload_but_retains_only_visible_text() {
    let turn = ModelTurn {
        text: "I will inspect the original source".into(),
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "read-original".into(),
            name: "read_conversation".into(),
            arguments_json: "{\"source_id\":\"original\"}".into(),
        }],
        ..Default::default()
    };
    let archived =
        native_archive_with_turn("visible-tool", vec![owner_message()], &[], &turn, "UTC");
    let mut message = ChatMessage::text(
        "actual-visible-tool-message",
        ChatRole::Assistant,
        &turn.text,
    );
    message.tool_calls = turn.tool_calls.iter().map(ToolCall::to_ref).collect();
    message.data_envelope = Some(archived.output.clone());
    let bound = archived.bind_visible_message(&message).unwrap();
    let capture = history_capture(&archived, &message, &bound).unwrap();
    assert_eq!(capture.content, turn.text);
    assert!(!capture.content.contains("source_id"));
    let mut changed = message.clone();
    changed.tool_calls[0].arguments_json.push(' ');
    assert!(archived.bind_visible_message(&changed).is_err());
    let mut changed = message;
    changed.tool_calls.clear();
    assert!(archived.bind_visible_message(&changed).is_err());
}

#[test]
fn empty_planning_text_has_no_body_and_unknown_source_policy_does_not_get_promoted() {
    let turn = ModelTurn {
        text: String::new(),
        stop_reason: StopReason::ToolUse,
        tool_calls: vec![ToolCall {
            id: "call".into(),
            name: "inspect".into(),
            arguments_json: "{}".into(),
        }],
        ..Default::default()
    };
    let archived =
        native_archive_with_turn("empty-visible", vec![owner_message()], &[], &turn, "UTC");
    let mut message = ChatMessage::text("empty-visible-tool-message", ChatRole::Assistant, "");
    message.tool_calls = turn.tool_calls.iter().map(ToolCall::to_ref).collect();
    message.data_envelope = Some(archived.output.clone());
    let bound = archived.bind_visible_message(&message).unwrap();
    assert!(history_capture(&archived, &message, &bound).is_none());
    let mut unknown = owner_message();
    unknown
        .data_envelope
        .as_mut()
        .unwrap()
        .provenance
        .source_provider_id = "unregistered-source".into();
    let archived = native_archive("unknown-visible", vec![unknown], &[]);
    let message = answer(&archived, "unknown-visible");
    let bound = archived.bind_visible_message(&message).unwrap();
    assert!(bound.history_policy().unwrap().no_copy);
    assert!(
        prepare_capture(
            &history_capture(&archived, &message, &bound).unwrap(),
            &AssistantDataPolicy::default()
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn derived_visible_answers_keep_original_parent_deadlines_and_require_complete_native_adjacency() {
    let mut owner = owner_message();
    owner
        .data_envelope
        .as_mut()
        .unwrap()
        .retention
        .expires_at_unix_ms = Some(5000);
    let first = native_archive("first-visible", vec![owner], &[]);
    let second = native_archive(
        "second-visible",
        vec![answer(&first, "first-visible")],
        &[&first],
    );
    let message = answer(&second, "second-visible");
    let bound = second.bind_visible_message(&message).unwrap();
    assert_eq!(
        bound
            .history_policy()
            .unwrap()
            .content_hard_expires_at_unix_ms,
        Some(5000)
    );
    assert_eq!(
        prepare_capture(
            &history_capture(&second, &message, &bound).unwrap(),
            &AssistantDataPolicy::default()
        )
        .unwrap()
        .unwrap()
        .proof
        .expires_at_unix_ms,
        5000
    );
    let mut missing = second.clone();
    missing.input_policy_graph.parents.clear();
    assert!(missing.bind_visible_message(&message).is_err());
}

fn history_capture<'a>(
    archive: &'a ArchivedAssistantOutput,
    message: &'a ChatMessage,
    bound: &'a VisibleAssistantMessage,
) -> Option<HistoryCapture<'a>> {
    if message.text.trim().is_empty() {
        return None;
    }
    Some(HistoryCapture {
        native: None,
        actor_id: &archive.source_set.actor_id,
        device_ref: &archive.source_set.source_device_ref,
        directory: None,
        conversation_id: &archive.source_set.consumer_conversation_id,
        root_conversation_id: &archive.source_set.consumer_conversation_id,
        native_id: &message.message_id,
        accepted_input_seq: archive.actor_input_seq,
        kind: AssistantSourceKind::AssistantMessage,
        role: "assistant",
        content: &message.text,
        envelope: &archive.output,
        source_policy: bound.history_policy()?,
        dependencies: &[],
        recorded_at_unix_ms: archive.sources.evidence.completed_at_unix_ms,
        statement_timezone: &archive.statement_timezone,
        revision: 1,
    })
}
