use super::*;
use crate::ai_assistant::{history::*, memory::*, user_instructions::*};

#[test]
fn defaults_preserve_content_and_work_counts_and_use_three_second_searches() {
    let limits = AssistantDataLimits::default();
    limits.validate().unwrap();
    let memory = &limits.memory;
    assert_eq!(
        (memory.draft_bytes, memory.source_refs, memory.current_facts),
        (2048, 8, 8)
    );
    assert_eq!(memory.user_instructions_bytes, 8192);
    let search = &limits.retrieval;
    assert_eq!(search.episode.query_ttl_ms, 1_800_000);
    assert_eq!(search.episode.unused_projection_ttl_ms, 1_800_000);
    assert_eq!(
        (
            search.episode.relevance_calls,
            search.episode.time_pages,
            search.episode.read_calls,
            search.episode.read_bytes
        ),
        (4, 10, 8, 131072)
    );
    assert_eq!(
        (
            search.query_groups,
            search.query_chars,
            search.relevance_hits,
            search.time_hits,
            search.search_window_days,
            search.search_conversations
        ),
        (4, 256, 10, 50, 90, 8)
    );
    assert_eq!(
        (
            search.query_candidates,
            search.relevance_roots,
            search.fragments_per_root,
            search.owner_scan_documents
        ),
        (40, 20, 3, 128)
    );
    assert_eq!(
        [
            search.history_keyword_scan_ms,
            search.memory_keyword_scan_ms,
            search.history_time_scan_ms,
            search.current_memory_selection_ms,
            search.owner_scan_ms
        ],
        [3000; 5]
    );
}

#[test]
fn query_limits_count_unicode_scalars_and_custom_counts_and_windows_apply() {
    let mut limits = AssistantRetrievalLimits::default();
    let unicode = "中🙂".repeat(128);
    assert_eq!(unicode.chars().count(), 256);
    assert!(unicode.len() > 256);
    validate_queries_with_limits(std::slice::from_ref(&unicode), false, &limits).unwrap();
    assert!(validate_queries_with_limits(&[format!("{unicode}a")], false, &limits).is_err());
    limits.query_chars = 300;
    limits.query_groups = 6;
    validate_queries_with_limits(&vec!["🙂".repeat(300); 6], false, &limits).unwrap();
    assert!(validate_queries_with_limits(&vec!["a".into(); 7], false, &limits).is_err());
    limits.query_chars = 2;
    validate_queries_with_limits(&["e\u{301}".into()], false, &limits).unwrap();
    assert!(validate_queries_with_limits(&["e\u{301}x".into()], false, &limits).is_err());
    limits.search_window_days = 120;
    limits.search_conversations = 12;
    limits.relevance_hits = 13;
    limits.time_hits = 60;
    let mut request: ConversationSearchRequest = serde_json::from_value(serde_json::json!({
        "queries": ["中🙂"], "conversation_ids": (0..12).map(|i| format!("root-{i}")).collect::<Vec<_>>(),
        "time_range": { "from_unix_ms": 1, "until_unix_ms": 120_u64 * 86_400_000 + 1 }, "limit": 13
    })).unwrap();
    request.validate_with_limits(&limits).unwrap();
    assert!(request.validate_with_limits(&Default::default()).is_err());
    request.limit = 60;
    request.sort = HistorySort::Time;
    request.validate_with_limits(&limits).unwrap();
    request.limit = 61;
    assert!(request.validate_with_limits(&limits).is_err());
}

#[test]
fn configurable_content_limits_apply_to_new_writes_without_hiding_stored_content() {
    let mut limits = AssistantMemoryLimits::default();
    let draft: MemoryDraft = serde_json::from_value(serde_json::json!({
        "content": "中".repeat(1000), "context": null,
        "source_refs": (0..9).map(|i| serde_json::json!({"kind":"input","id":format!("source-{i}"),"revision":1,"role":"evidence"})).collect::<Vec<_>>()
    })).unwrap();
    assert!(draft.validate_with_limits(&limits).is_err());
    limits.draft_bytes = 4096;
    limits.source_refs = 9;
    draft.validate_with_limits(&limits).unwrap();
    let instructions = UserInstructionsUpdate {
        expected_revision: 1,
        content: "中".repeat(3000),
    };
    assert!(instructions.validate_with_limits(&limits).is_err());
    limits.user_instructions_bytes = 16384;
    instructions.validate_with_limits(&limits).unwrap();
    instructions.validate().unwrap();
    limits.user_instructions_bytes = 1;
    assert!(instructions.validate_with_limits(&limits).is_err());
}

#[test]
fn policy_round_trips_all_limits_and_rejects_invalid_configuration_at_its_boundary() {
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.episode.relevance_calls = 0;
    policy.limits.retrieval.history_keyword_scan_ms = 7000;
    policy.limits.retrieval.episode.query_ttl_ms = 60_000;
    policy.limits.retrieval.episode.unused_projection_ttl_ms = 3_600_000;
    policy.validate().unwrap();
    let json = serde_json::to_string(&policy).unwrap();
    assert_eq!(
        serde_json::from_str::<AssistantDataPolicy>(&json).unwrap(),
        policy
    );
    let encoded = wincode::serialize(&policy).unwrap();
    assert_eq!(
        wincode::deserialize::<AssistantDataPolicy>(&encoded).unwrap(),
        policy
    );
    for invalid in 0..7 {
        let mut bad = policy.clone();
        match invalid {
            0 => bad.limits.retrieval.owner_scan_ms = 0,
            1 => bad.limits.memory.draft_bytes = 0,
            2 => bad.limits.retrieval.query_chars = 0,
            3 => bad.limits.retrieval.episode.query_ttl_ms = 0,
            4 => bad.limits.retrieval.episode.unused_projection_ttl_ms = 0,
            5 => bad.limits.retrieval.episode.query_ttl_ms = u64::MAX,
            _ => bad.limits.retrieval.episode.unused_projection_ttl_ms = u64::MAX,
        }
        assert!(bad.validate().is_err());
    }
}

#[test]
fn processing_policy_round_trips_all_nondefault_working_limits_and_rejects_invalid_input() {
    let processing = AssistantProcessingLimits {
        output_gc_interval_ms: 71,
        output_gc_actor_batch_size: 35,
        output_gc_batch_size: 129,
        native_retention_page_size: 3,
        history_capture_batch_size: 129,
        history_scan_page_size: 7,
        current_memory_page_size: 2,
        ..Default::default()
    };
    let mut policy = AssistantDataPolicy::default();
    policy.limits.processing = processing.clone();
    policy.validate().unwrap();
    let encoded = serde_json::to_value(&policy).unwrap();
    assert_eq!(
        serde_json::from_value::<AssistantDataPolicy>(encoded.clone()).unwrap(),
        policy
    );
    for key in [
        "output_gc_interval_ms",
        "output_gc_actor_batch_size",
        "output_gc_batch_size",
        "native_retention_page_size",
        "history_capture_batch_size",
        "history_scan_page_size",
        "current_memory_page_size",
    ] {
        let mut invalid = encoded.clone();
        invalid["limits"]["processing"][key] = serde_json::json!(0);
        assert!(
            serde_json::from_value::<AssistantDataPolicy>(invalid)
                .unwrap()
                .validate()
                .is_err(),
            "{key}"
        );
    }
    {
        let (key, value) = ("output_gc_interval_ms", u64::MAX);
        let mut invalid = encoded.clone();
        invalid["limits"]["processing"][key] = serde_json::json!(value);
        assert!(
            serde_json::from_value::<AssistantDataPolicy>(invalid)
                .unwrap()
                .validate()
                .is_err(),
            "{key}"
        );
    }
}

#[test]
fn configured_reply_capacity_and_window_count_replace_fixed_read_limits() {
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.read_messages = 80;
    policy.limits.retrieval.episode.response_bytes = 256 * 1024;
    policy.limits.retrieval.episode.read_bytes = 512 * 1024;
    policy.validate().unwrap();
    let restored: AssistantDataPolicy =
        serde_json::from_str(&serde_json::to_string(&policy).unwrap()).unwrap();
    assert_eq!(restored, policy);
    let mut request: ConversationReadRequest = serde_json::from_value(serde_json::json!({
        "source_id":"source", "limit":60, "max_bytes":131072
    }))
    .unwrap();
    request
        .validate_with_limits(&policy.limits.retrieval)
        .unwrap();
    assert!(request.validate_with_limits(&Default::default()).is_err());
    request.limit = 81;
    assert!(
        request
            .validate_with_limits(&policy.limits.retrieval)
            .is_err()
    );
    request.limit = 60;
    request.max_bytes = 256 * 1024 + 1;
    assert!(
        request
            .validate_with_limits(&policy.limits.retrieval)
            .is_err()
    );
    policy.limits.retrieval.read_messages = 0;
    assert!(policy.validate().is_err());
    policy.limits.retrieval.read_messages = 80;
    policy.limits.retrieval.episode.response_bytes = 3;
    assert!(policy.validate().is_err());
}

#[test]
fn remaining_product_capacities_round_trip_without_old_fixed_ceilings() {
    let mut policy = AssistantDataPolicy::default();
    let limits = &mut policy.limits;
    limits.conversation = AssistantConversationLimits {
        question_bytes: 131072,
        page_bytes: 1024 * 1024,
        citations: 80,
        citation_bytes: 131072,
        citation_page_answers: 150,
    };
    limits.retrieval.snippet_bytes = 2048;
    limits.retrieval.episode.query_devices = 20000;
    limits.retrieval.episode.query_cursors = 64;
    limits.retrieval.episode.query_state_bytes = 2 * 1024 * 1024;
    limits.retrieval.episode.query_seen_sources = 1000;
    limits.retrieval.episode.query_delivered_calls = 64;
    limits.processing.status_coverage = 80;
    limits.processing.native_status_rows = 40;
    limits.processing.native_status_items = 60;
    limits.processing.native_status_model_items = 8;
    limits.processing.native_status_source_refs = 10;
    policy.validate().unwrap();
    let value = serde_json::to_value(&policy).unwrap();
    assert_eq!(
        serde_json::from_value::<AssistantDataPolicy>(value.clone()).unwrap(),
        policy
    );
    for (group, key) in [
        ("conversation", "question_bytes"),
        ("conversation", "page_bytes"),
        ("conversation", "citations"),
        ("conversation", "citation_bytes"),
        ("conversation", "citation_page_answers"),
        ("retrieval", "snippet_bytes"),
        ("processing", "native_status_source_refs"),
    ] {
        let mut invalid = value.clone();
        invalid["limits"][group][key] = serde_json::json!(0);
        assert!(
            serde_json::from_value::<AssistantDataPolicy>(invalid)
                .unwrap()
                .validate()
                .is_err(),
            "{group}.{key}"
        );
    }
    for key in [
        "query_devices",
        "query_cursors",
        "query_state_bytes",
        "query_seen_sources",
        "query_delivered_calls",
    ] {
        let mut invalid = value.clone();
        invalid["limits"]["retrieval"]["episode"][key] = serde_json::json!(0);
        assert!(
            serde_json::from_value::<AssistantDataPolicy>(invalid)
                .unwrap()
                .validate()
                .is_err(),
            "{key}"
        );
    }
}
#[test]
fn ordinary_questions_use_configured_utf8_bytes_at_the_external_boundary() {
    let ask: crate::ai_assistant::AiAssistantAsk = serde_json::from_value(serde_json::json!({
        "question": "中".repeat(12000), "client_message_id":"input-1", "client_timezone":"Asia/Shanghai", "selected_capability_ids":[], "selected_attachment_ids":[]
    })).unwrap();
    assert!(ask.validate_with_limits(&Default::default()).is_err());
    let mut limits = AssistantConversationLimits {
        question_bytes: 36000,
        ..Default::default()
    };
    ask.validate_with_limits(&limits).unwrap();
    limits.question_bytes -= 1;
    assert!(ask.validate_with_limits(&limits).is_err());
}
