use super::*;
use crate::native_ids::{self, Kind};
use desk_agent_protocol::ai_assistant::{
    history::{
        AssistantDeviceScope, AssistantHistoryCoverage, ConversationSearchHit, HistoryMessage,
    },
    source::{AssistantSourceDevice, AssistantSourceKind, AssistantSourceLocator},
};

fn token(label: &str) -> String {
    QueryCursor::new(
        &native_ids::stable(Kind::AssistantRetrievalQuery, &"query"),
        &native_ids::stable(Kind::AssistantPageCursor, &label),
    )
    .unwrap()
    .encode()
}
fn device() -> AssistantSourceDevice {
    AssistantSourceDevice {
        device_ref: "device".into(),
        recorded_name: Some("原电脑".into()),
        current_name: Some("电脑A".into()),
        historical: false,
    }
}
fn coverage() -> AssistantHistoryCoverage {
    AssistantHistoryCoverage {
        retained_from_unix_ms: Some(1),
        retained_until_unix_ms: Some(1000),
        indexed_through_unix_ms: Some(1000),
        pending: 0,
        failed: 0,
        has_gaps: false,
        partial: false,
        reasons: Vec::new(),
    }
}
fn search(count: usize) -> ConversationSearchResult {
    ConversationSearchResult {
        hits: (0..count)
            .map(|index| ConversationSearchHit {
                source_id: format!("source-{index}"),
                revision: 1,
                kind: AssistantSourceKind::Input,
                locator: AssistantSourceLocator {
                    conversation_id: format!("conversation-{index}"),
                    root_conversation_id: format!("conversation-{index}"),
                    message_id: Some(format!("message-{index}")),
                    input_event_id: None,
                    action_id: None,
                    result_id: None,
                    fragment_index: 0,
                    byte_start: 0,
                    byte_end: 1024,
                },
                source_device: Some(device()),
                conversation_title: Some("报告调研".into()),
                snippet: "汉".repeat(300),
                recorded_at_unix_ms: 1000,
                statement_timezone: "Asia/Shanghai".into(),
                native_action_at_unix_ms: None,
                native_action_interval: None,
                native_time_basis: None,
                native_status_summary: None,
            })
            .collect(),
        scope: AssistantDeviceScope::CurrentDevice,
        effective_queries: vec![vec!["报告".into(), "report".into()]],
        retrieval_mode: "relevance".into(),
        has_more: false,
        next_cursor: None,
        scope_changed: false,
        coverage: coverage(),
    }
}
fn window(count: usize, content: &str) -> ConversationReadResult {
    ConversationReadResult {
        conversation_id: "conversation".into(),
        messages: (0..count)
            .map(|index| HistoryMessage {
                source_id: format!("source-{index}"),
                message_id: format!("message-{index}"),
                revision: 1,
                role: "assistant".into(),
                content: content.into(),
                recorded_at_unix_ms: 1000 + index as u64,
                statement_timezone: "Asia/Shanghai".into(),
                source_device: Some(device()),
            })
            .collect(),
        related_results: Vec::new(),
        previous_cursor: None,
        next_cursor: None,
        source_observed_through_unix_ms: 1000,
        actual_bytes: 123,
        has_more: false,
        partial: false,
        reasons: Vec::new(),
    }
}
fn measured(call_id: &str, content: &str) -> u32 {
    serde_json::to_vec(&serde_json::json!({"role":"tool","tool_call_id":call_id,"content":content}))
        .unwrap()
        .len() as u32
}

#[test]
fn framing_counts_escaped_json_utf8_and_tool_metadata_instead_of_only_the_result_text() {
    let value = serde_json::json!({"text":"汉\n\"C:\\report.txt\"","source_id":"source-1"});
    let raw = serde_json::to_string(&value).unwrap();
    let bytes = measured("call\"1", &raw);
    assert!(bytes as usize > raw.len());
    assert!(
        ToolReplyBudget::new("call\"1", raw.len() as u32)
            .unwrap()
            .try_encode(&value)
            .unwrap()
            .is_none()
    );
    let exact = ToolReplyBudget::new("call\"1", bytes)
        .unwrap()
        .encode(&value)
        .unwrap();
    assert_eq!(exact.content, raw);
    assert_eq!(exact.serialized_bytes, bytes);
    assert!(
        ToolReplyBudget::new("call\"1", bytes - 1)
            .unwrap()
            .try_encode(&value)
            .unwrap()
            .is_none()
    );
}

#[test]
fn search_keeps_a_ranked_prefix_and_utf8_snippets_while_reserving_its_real_continuation_and_partial_metadata()
 {
    let result = fit_search(
        search(8),
        &ToolReplyBudget::new("call", 6000).unwrap(),
        &token("next"),
    )
    .unwrap();
    assert!(result.selected_count > 0 && result.selected_count < 8);
    assert_eq!(result.result.hits.len(), result.selected_count);
    assert!(result.result.has_more);
    assert!(result.result.coverage.partial);
    assert!(
        result
            .result
            .coverage
            .reasons
            .contains(&RESPONSE_BYTE_LIMIT_REASON.into())
    );
    assert_eq!(result.result.next_cursor, Some(token("next")));
    assert_eq!(
        result.framed.serialized_bytes,
        measured("call", &result.framed.content)
    );
    assert!(result.framed.serialized_bytes <= 6000);
    for (index, hit) in result.result.hits.iter().enumerate() {
        assert_eq!(hit.source_id, format!("source-{index}"));
        assert_eq!(hit.snippet, search(8).hits[index].snippet);
        assert!(hit.snippet.is_char_boundary(hit.snippet.len()));
    }
}

#[test]
fn one_byte_less_than_a_complete_search_never_returns_an_oversized_success_or_claims_complete_coverage()
 {
    let full = fit_search(
        search(2),
        &ToolReplyBudget::new("call", 32768).unwrap(),
        &token("next"),
    )
    .unwrap();
    let exact = fit_search(
        search(2),
        &ToolReplyBudget::new("call", full.framed.serialized_bytes).unwrap(),
        &token("next"),
    )
    .unwrap();
    assert_eq!(exact.selected_count, 2);
    assert!(!exact.result.has_more);
    let shorter = fit_search(
        search(2),
        &ToolReplyBudget::new("call", full.framed.serialized_bytes - 1).unwrap(),
        &token("next"),
    )
    .unwrap();
    assert_eq!(shorter.selected_count, 1);
    assert!(shorter.result.has_more);
    assert!(shorter.result.coverage.partial);
}

#[test]
fn whole_fragment_selection_follows_direction_and_keeps_the_context_anchor() {
    let content = "x".repeat(2400);
    let original = window(6, &content);
    for (layout, expected) in [
        (WindowLayout::Forward, 0..1),
        (WindowLayout::Backward, 5..6),
        (WindowLayout::Latest, 5..6),
        (WindowLayout::Context { focus_index: 3 }, 3..4),
    ] {
        let selected = fit_window(
            original.clone(),
            &ToolReplyBudget::new("call", 4000).unwrap(),
            layout,
            WindowPageTokens {
                previous: &token("previous"),
                next: &token("next"),
            },
        )
        .unwrap();
        assert_eq!(selected.selected, expected);
        assert_eq!(selected.result.messages[0].content, content);
        assert_eq!(
            selected.result.messages[0].source_id,
            original.messages[expected.start].source_id
        );
        assert_eq!(
            selected.result.previous_cursor.is_some(),
            expected.start > 0
        );
        assert_eq!(
            selected.result.next_cursor.is_some(),
            expected.end < original.messages.len()
        );
        assert!(selected.result.partial);
        assert!(selected.result.has_more);
        assert_eq!(
            selected.result.actual_bytes,
            measured("call", &selected.framed.content)
        );
        assert!(selected.result.actual_bytes <= 4000);
    }
}

fn memories(count: usize) -> desk_agent_protocol::ai_assistant::memory::MemorySearchResult {
    use desk_agent_protocol::ai_assistant::memory::{
        MemoryRecord, MemoryScope, MemorySearchResult, MemoryStatus,
    };
    MemorySearchResult {
        memories: (0..count)
            .map(|index| MemoryRecord {
                memory_id: format!("memory-{index}"),
                revision: 2,
                scope: MemoryScope::Personal,
                source_device: None,

                content: "汉\n\"C:\\目录\"".repeat(35),
                context: Some("来自本人明确陈述".repeat(20)),
                status: if index == 0 {
                    MemoryStatus::PendingConflict
                } else {
                    MemoryStatus::Active
                },
                source_ids: vec![format!("evidence-{index}"), format!("background-{index}")],
                observed_at_unix_ms: 10,
                updated_at_unix_ms: 20,
                valid_from_unix_ms: 20,
                valid_until_unix_ms: (index == 0).then_some(20),
            })
            .collect(),
        effective_queries: vec![vec!["目录".into()]],
        has_more: false,
        next_cursor: None,
        memory_state_generation: 3,
        coverage: coverage(),
    }
}

#[test]
fn memory_byte_pressure_keeps_whole_facts_context_status_and_source_sets_with_a_real_continuation()
{
    let original = memories(6);
    let reply = fit_memories(
        original.clone(),
        &ToolReplyBudget::new("call", 6000).unwrap(),
        &token("memory-next"),
    )
    .unwrap();
    assert!(reply.selected_count > 0 && reply.selected_count < original.memories.len());
    assert_eq!(
        reply.result.memories,
        original.memories[..reply.selected_count]
    );
    assert!(reply.result.has_more);
    assert!(reply.result.coverage.partial);
    assert!(
        reply
            .result
            .coverage
            .reasons
            .contains(&RESPONSE_BYTE_LIMIT_REASON.into())
    );
    assert_eq!(reply.result.next_cursor, Some(token("memory-next")));
    assert_eq!(
        reply.framed.serialized_bytes,
        measured("call", &reply.framed.content)
    );
    assert!(reply.framed.serialized_bytes <= 6000);
}

#[test]
fn memory_exact_framed_byte_limit_and_empty_results_never_silently_drop_a_fact() {
    let full = fit_memories(
        memories(2),
        &ToolReplyBudget::new("call", 16384).unwrap(),
        &token("memory-next"),
    )
    .unwrap();
    let exact = fit_memories(
        memories(2),
        &ToolReplyBudget::new("call", full.framed.serialized_bytes).unwrap(),
        &token("memory-next"),
    )
    .unwrap();
    assert_eq!(exact.selected_count, 2);
    assert!(!exact.result.has_more);
    let shorter = fit_memories(
        memories(2),
        &ToolReplyBudget::new("call", full.framed.serialized_bytes - 1).unwrap(),
        &token("memory-next"),
    )
    .unwrap();
    assert_eq!(shorter.selected_count, 1);
    assert!(shorter.result.has_more);
    assert!(
        fit_memories(
            memories(1),
            &ToolReplyBudget::new("call", 128).unwrap(),
            &token("memory-next")
        )
        .is_err()
    );
    let empty = fit_memories(
        memories(0),
        &ToolReplyBudget::new("call", 4096).unwrap(),
        &token("memory-next"),
    )
    .unwrap();
    assert_eq!(empty.selected_count, 0);
    assert!(!empty.result.has_more);
    assert_eq!(empty.result.next_cursor, None);
}

#[test]
fn actual_window_bytes_include_the_decimal_count_field_itself_and_roundtrip_exactly() {
    for length in [0, 1, 900, 999, 1000, 1500] {
        let mut value = window(2, &"汉\n\"\\".repeat(length / 6));
        let budget = ToolReplyBudget::new("call", 32768).unwrap();
        let framed = budget.try_encode_window(&mut value).unwrap().unwrap();
        assert_eq!(value.actual_bytes, measured("call", &framed.content));
        assert_eq!(value.actual_bytes, framed.serialized_bytes);
        assert_eq!(
            serde_json::from_str::<ConversationReadResult>(&framed.content).unwrap(),
            value
        );
    }
}

#[test]
fn metadata_only_empty_results_are_valid_but_cannot_hide_nonempty_unread_sources() {
    let empty = fit_search(
        search(0),
        &ToolReplyBudget::new("call", 32768).unwrap(),
        &token("next"),
    )
    .unwrap();
    assert_eq!(empty.selected_count, 0);
    assert!(!empty.result.has_more);
    assert!(
        fit_search(
            search(1),
            &ToolReplyBudget::new("call", 700).unwrap(),
            &token("next")
        )
        .is_err()
    );
    assert!(
        fit_window(
            window(1, &"x".repeat(4096)),
            &ToolReplyBudget::new("call", 1000).unwrap(),
            WindowLayout::Forward,
            WindowPageTokens {
                previous: &token("previous"),
                next: &token("next")
            }
        )
        .is_err()
    );
    assert!(
        fit_window(
            window(0, ""),
            &ToolReplyBudget::new("call", 32).unwrap(),
            WindowLayout::Forward,
            WindowPageTokens {
                previous: &token("previous"),
                next: &token("next")
            }
        )
        .is_err()
    );
}

#[test]
fn malformed_limits_focus_tokens_or_reason_text_are_not_silently_serialized() {
    assert!(ToolReplyBudget::new("call", 0).is_err());
    assert!(ToolReplyBudget::new("call", 32769).is_ok());
    assert!(
        fit_search(
            search(1),
            &ToolReplyBudget::new("call", 32768).unwrap(),
            "forged-token"
        )
        .is_err()
    );
    assert!(
        fit_window(
            window(1, "text"),
            &ToolReplyBudget::new("call", 32768).unwrap(),
            WindowLayout::Context { focus_index: 2 },
            WindowPageTokens {
                previous: &token("previous"),
                next: &token("next")
            }
        )
        .is_err()
    );
    let mut malformed = search(0);
    malformed.coverage.reasons = vec!["raw backend error body".into()];
    assert!(
        fit_search(
            malformed,
            &ToolReplyBudget::new("call", 32768).unwrap(),
            &token("next")
        )
        .is_err()
    );
}

#[test]
fn configured_large_replies_fit_complete_windows_and_searches_without_legacy_caps() {
    let budget = ToolReplyBudget::new("read", 256 * 1024).unwrap();
    let previous = token("previous");
    let next = token("next");
    let reply = fit_window(
        window(40, &"中".repeat(1024)),
        &budget,
        WindowLayout::Forward,
        WindowPageTokens {
            previous: &previous,
            next: &next,
        },
    )
    .unwrap();
    assert_eq!(reply.result.messages.len(), 40);
    assert!(reply.framed.serialized_bytes > 32 * 1024);
    assert_eq!(
        reply.framed.serialized_bytes,
        measured("read", &reply.framed.content)
    );
    let reply = fit_search(search(50), &budget, &next).unwrap();
    assert_eq!(reply.selected_count, 50);
    assert!(reply.framed.serialized_bytes > 16 * 1024);
}

#[test]
fn history_reply_keeps_large_whole_versions_and_rejects_nonadvancing_empty_pages() {
    use desk_agent_protocol::ai_assistant::memory::{
        MemoryHistoryEntry, MemoryHistoryResult, MemoryStatus,
    };
    let result = MemoryHistoryResult {
        memory_id: "memory".into(),
        current_revision: 2,
        versions: [2, 1]
            .into_iter()
            .map(|revision| MemoryHistoryEntry {
                revision,
                content: "x".repeat(40 * 1024),
                context: Some("original context".into()),
                status: if revision == 2 {
                    MemoryStatus::Active
                } else {
                    MemoryStatus::Superseded
                },
                recorded_at_unix_ms: revision,
                source_ids: vec![format!("source-{revision}")],
            })
            .collect(),
        has_more: false,
        next_cursor: None,
        partial: false,
        reasons: Vec::new(),
    };
    let prepared = fit_memory_history(
        result.clone(),
        &ToolReplyBudget::new("call", 64 * 1024).unwrap(),
        &token("history"),
    )
    .unwrap();
    assert_eq!(prepared.selected_count, 1);
    assert_eq!(prepared.result.versions[0], result.versions[0]);
    assert!(prepared.result.has_more && prepared.result.partial);
    assert!(
        prepared
            .result
            .reasons
            .iter()
            .any(|reason| reason == RESPONSE_BYTE_LIMIT_REASON)
    );
    assert!(
        fit_memory_history(
            result,
            &ToolReplyBudget::new("call", 32 * 1024).unwrap(),
            &token("history")
        )
        .is_err()
    );
}
