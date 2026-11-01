use super::*;

fn request() -> ConversationSearchRequest {
    serde_json::from_value(
        serde_json::json!({"queries":["report"],"time_range":null,"cursor":null}),
    )
    .unwrap()
}

#[test]
fn default_query_limits_count_unicode_scalars_and_four_groups() {
    let limits = crate::ai_assistant::limits::AssistantRetrievalLimits::default();
    let exact = "记🙂".repeat(128);
    assert_eq!(exact.chars().count(), 256);
    assert!(validate_queries_with_limits(&vec![exact.clone(); 4], false, &limits).is_ok());
    assert!(validate_queries_with_limits(&[format!("{exact}x")], false, &limits).is_err());
    assert!(validate_queries_with_limits(&vec!["term".into(); 5], false, &limits).is_err());
    assert!(validate_queries_with_limits(&["term\nsecret".into()], false, &limits).is_err());
    let mut search = request();
    search.conversation_ids = (0..8)
        .map(|index| format!("conversation-{index}"))
        .collect();
    assert!(search.validate_with_limits(&limits).is_ok());
    search.conversation_ids.push("conversation-8".into());
    assert!(search.validate_with_limits(&limits).is_err());
}

#[test]
fn time_enumeration_requires_an_explicit_date_or_conversation_boundary() {
    let mut search = request();
    search.queries.clear();
    search.sort = HistorySort::Time;
    search.limit = 50;
    assert!(search.validate_with_limits(&Default::default()).is_err());
    search.time_range = Some(HistoryTimeRange {
        from_unix_ms: 100,
        until_unix_ms: 1000,
    });
    assert!(search.validate_with_limits(&Default::default()).is_ok());
    search.time_range = None;
    search.conversation_ids = vec!["specific-conversation".into()];
    assert!(search.validate_with_limits(&Default::default()).is_ok());
    search.sort = HistorySort::Relevance;
    search.limit = 5;
    assert!(search.validate_with_limits(&Default::default()).is_err());
    search.queries.push("report".into());
    assert!(search.validate_with_limits(&Default::default()).is_ok());
}

#[test]
fn time_order_is_explicit_on_the_wire_and_defaults_to_newest_first() {
    let mut search = request();
    assert_eq!(search.order, HistoryOrder::Desc);
    search.sort = HistorySort::Time;
    search.time_range = Some(HistoryTimeRange {
        from_unix_ms: 1,
        until_unix_ms: 1000,
    });
    search.order = HistoryOrder::Asc;
    assert!(search.validate_with_limits(&Default::default()).is_ok());
    let encoded = serde_json::to_value(&search).unwrap();
    assert_eq!(encoded["order"], "asc");
    assert_eq!(
        serde_json::from_value::<ConversationSearchRequest>(encoded).unwrap(),
        search
    );
    let mut unknown = serde_json::to_value(&search).unwrap();
    unknown["order"] = serde_json::json!("automatic");
    assert!(serde_json::from_value::<ConversationSearchRequest>(unknown).is_err());
}

#[test]
fn model_cursor_arguments_are_bounded_locators_and_cannot_supply_a_subject() {
    let mut search = request();
    search.cursor = Some("x".repeat(128));
    assert!(search.validate_with_limits(&Default::default()).is_ok());
    search.cursor = Some("x".repeat(129));
    assert!(search.validate_with_limits(&Default::default()).is_err());
    let mut value = serde_json::to_value(request()).unwrap();
    value["actor_id"] = serde_json::json!("other-owner");
    assert!(serde_json::from_value::<ConversationSearchRequest>(value).is_err());
    let mut read: ConversationReadRequest =
        serde_json::from_value(serde_json::json!({"source_id":"source","cursor":null})).unwrap();
    read.cursor = Some("x".repeat(129));
    assert!(read.validate().is_err());
    let mut memory: super::super::memory::MemorySearchRequest =
        serde_json::from_value(serde_json::json!({
            "queries":["language"],"cursor":null,
        }))
        .unwrap();
    memory.cursor = Some("x".repeat(129));
    assert!(memory.validate_with_limits(&Default::default()).is_err());
}

#[test]
fn time_window_errors_distinguish_order_and_configured_span() {
    let limits = crate::ai_assistant::limits::AssistantRetrievalLimits {
        search_window_days: 2,
        ..Default::default()
    };
    let mut range = HistoryTimeRange {
        from_unix_ms: 100,
        until_unix_ms: 100 + 2 * 86_400_000,
    };
    range.validate_with_limits(&limits).unwrap();
    range.until_unix_ms += 1;
    assert!(range.validate_with_limits(&limits).is_err());
    let mut search = request();
    search.time_range = Some(range);
    let error = search.validate_with_limits(&limits).unwrap_err();
    assert!(error.contains("$.time_range"));
    assert!(error.contains("maximum 2 days (172800000 ms)"));
    assert!(error.contains("observed 172800001 ms"));
    assert_eq!(search.validate_with_limits(&limits).unwrap_err(), error);
    for until_unix_ms in [99, 100] {
        let range = HistoryTimeRange {
            from_unix_ms: 100,
            until_unix_ms,
        };
        let error = range.validate_with_limits(&limits).unwrap_err();
        assert!(error.contains("$.time_range.until_unix_ms"));
        assert!(error.contains("greater than $.time_range.from_unix_ms"));
    }
}
