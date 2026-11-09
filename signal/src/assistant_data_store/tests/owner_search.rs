use super::keyword_search::{capture_at, device};
use super::*;
use desk_agent_protocol::ai_assistant::{
    history::HistorySort, owner_history::AssistantHistorySearchRequest,
};

fn query() -> AssistantHistorySearchRequest {
    AssistantHistorySearchRequest {
        queries: vec!["report email".into(), "邮件 报告".into()],
        device_ref: None,
        conversation_id: None,
        time_range: None,
        source_kinds: Vec::new(),
        sort: HistorySort::Relevance,
        limit: 2,
        cursor: None,
    }
}
#[tokio::test]
async fn owner_search_uses_real_index_and_frozen_pages_without_execution_or_model_scope() {
    let store = setup().await;
    let device = device(&store, "owner-search").await;
    let mut sources = Vec::new();
    for index in 0..7 {
        sources.push(
            capture_at(
                &store,
                &device,
                &format!("root-{index}"),
                &format!("search-{index}"),
                "发送邮件 email report 报告",
                &[],
                1000 + index,
            )
            .await,
        );
    }
    let mut request = query();
    let policy = AssistantDataPolicy::default();
    let first = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(first.fragments.len(), 2);
    assert!(
        first
            .reasons
            .iter()
            .any(|reason| reason == "candidate_statistics")
    );
    let late = capture_at(
        &store,
        &device,
        "root-late",
        "search-late",
        "email report",
        &[],
        2000,
    )
    .await;
    let mut ids = first
        .fragments
        .iter()
        .map(|fragment| fragment.source.source_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    request.cursor = first.next_cursor;
    while request.cursor.is_some() {
        let page = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
            .await
            .unwrap();
        assert_eq!(
            page.observed_through_generation,
            first.observed_through_generation
        );
        for fragment in page.fragments {
            assert!(ids.insert(fragment.source.source_id));
        }
        request.cursor = page.next_cursor;
    }
    assert_eq!(ids, sources.into_iter().collect());
    assert!(!ids.contains(&late));
    let unauthorized =
        super::super::owner_search::search_on(&store.db, "2", &query(), &policy, 5000).await;
    assert!(unauthorized.is_err() || unauthorized.unwrap().fragments.is_empty());
}
#[tokio::test]
async fn owner_search_rechecks_revision_scope_retention_and_current_device_on_each_page() {
    let store = setup().await;
    let device = device(&store, "owner-search-revoke").await;
    for index in 0..4 {
        capture_at(
            &store,
            &device,
            &format!("revoke-{index}"),
            &format!("source-{index}"),
            "report email",
            &[],
            1000,
        )
        .await;
    }
    let mut request = query();
    request.limit = 1;
    let mut policy = AssistantDataPolicy::default();
    let first = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    request.cursor = first.next_cursor.clone();
    request.queries = vec!["different".into()];
    assert!(
        super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
            .await
            .is_err()
    );
    request.queries = query().queries;
    policy.revision += 1;
    assert!(
        super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
            .await
            .is_err()
    );
    policy.revision -= 1;
    let expired = super::super::owner_search::search_on(
        &store.db,
        "1",
        &request,
        &policy,
        u64::from(policy.history_retention_days) * 86_400_000 + 5000,
    )
    .await
    .unwrap();
    assert!(expired.fragments.is_empty());
    assert!(expired.partial);
    assert!(expired.next_cursor.is_none());
}
#[tokio::test]
async fn owner_search_preserves_whole_fragments_inside_the_final_response_budget() {
    let store = setup().await;
    let device = device(&store, "owner-search-bytes").await;
    for index in 0..30 {
        capture_at(
            &store,
            &device,
            "large-root",
            &format!("large-{index}"),
            &"a".repeat(3800),
            &[],
            1000 + index,
        )
        .await;
    }
    let mut request = query();
    request.queries.clear();
    request.sort = HistorySort::Time;
    request.limit = 20;
    let mut policy = AssistantDataPolicy::default();
    policy.limits.conversation.page_bytes = 32 * 1024;
    let mut count = 0;
    loop {
        let page = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
            .await
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 32 * 1024);
        for fragment in &page.fragments {
            assert_eq!(fragment.source.content.as_ref().unwrap().len(), 3800);
        }
        count += page.fragments.len();
        request.cursor = page.next_cursor;
        if request.cursor.is_none() {
            break;
        }
    }
    assert_eq!(count, 30);
}

#[tokio::test]
async fn expanded_owner_candidates_and_large_cursors_page_every_result() {
    let store = setup().await;
    let device = device(&store, "many-owner-candidates").await;
    let mut expected = std::collections::BTreeSet::new();
    for index in 0..260 {
        expected.insert(
            capture_at(
                &store,
                &device,
                &format!("many-root-{index}"),
                &format!("many-input-{index}"),
                "email report",
                &[],
                1000 + index,
            )
            .await,
        );
    }
    let mut policy = AssistantDataPolicy::default();
    policy.limits.retrieval.query_candidates = 300;
    policy.limits.retrieval.relevance_roots = 300;
    policy.limits.retrieval.owner_scan_documents = 400;
    policy.limits.retrieval.owner_scan_ms = 30_000;
    let mut request = query();
    request.queries = vec!["email report".into()];
    request.limit = 20;
    let mut seen = std::collections::BTreeSet::new();
    let first = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert!(first.next_cursor.as_ref().unwrap().len() > 4096);
    {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let raw = URL_SAFE_NO_PAD
            .decode(
                first
                    .next_cursor
                    .as_ref()
                    .unwrap()
                    .strip_prefix("oh1_")
                    .unwrap(),
            )
            .unwrap();
        let mut state: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let remaining = state["remaining"].as_array_mut().unwrap();
        for index in 0..10 {
            remaining.insert(0, serde_json::json!([format!("missing-source-{index}"), 1]));
        }
        request.cursor = Some(format!(
            "oh1_{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&state).unwrap())
        ));
        let mut restricted = policy.clone();
        restricted.limits.retrieval.owner_scan_documents = 3;
        assert!(
            super::super::owner_search::search_on(&store.db, "1", &request, &restricted, 5000)
                .await
                .is_err()
        );
    }
    request.cursor = first.next_cursor;
    for fragment in first.fragments {
        assert!(seen.insert(fragment.source.source_id));
    }
    while request.cursor.is_some() {
        let page = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
            .await
            .unwrap();
        for fragment in page.fragments {
            assert!(seen.insert(fragment.source.source_id));
        }
        request.cursor = page.next_cursor;
    }
    assert_eq!(seen, expected);
    request.cursor = None;
    policy.limits.retrieval.episode.query_state_bytes = 128;
    assert!(
        super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn owner_search_page_target_keeps_one_whole_fragment_and_can_exceed_old_32k() {
    let store = setup().await;
    let device = device(&store, "owner-shared-page").await;
    for index in 0..20 {
        capture_at(
            &store,
            &device,
            "shared-page",
            &format!("shared-page-{index}"),
            &"原文".repeat(600),
            &[],
            1000 + index,
        )
        .await;
    }
    let mut request = query();
    request.queries.clear();
    request.sort = HistorySort::Time;
    request.limit = 20;
    let mut policy = AssistantDataPolicy::default();
    let large = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(large.fragments.len(), 20);
    assert!(serde_json::to_vec(&large).unwrap().len() > 32 * 1024);
    policy.limits.conversation.page_bytes = 1024;
    let first = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(first.fragments.len(), 1);
    assert_eq!(
        first.fragments[0].source.content.as_deref(),
        Some("原文".repeat(600).as_str())
    );
    request.cursor = first.next_cursor;
    let next = super::super::owner_search::search_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(next.fragments.len(), 1);
    assert_ne!(
        first.fragments[0].source.source_id,
        next.fragments[0].source.source_id
    );
}

#[tokio::test]
async fn default_chinese_query_groups_keep_all_matching_terms_in_owner_ranking() {
    let store = setup().await;
    let device = device(&store, "owner-cjk-postings").await;
    // Four default-sized CJK groups produce more than 1,025 matching terms.
    let groups = (0..4)
        .map(|group| {
            (0..256)
                .map(|index| char::from_u32(0x4e00 + group * 256 + index).unwrap())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let complete = capture_at(
        &store,
        &device,
        "complete-cjk-root",
        "complete-cjk-input",
        &groups.join(" "),
        &[],
        1000,
    )
    .await;
    capture_at(
        &store,
        &device,
        "prefix-cjk-root",
        "prefix-cjk-input",
        &groups[..3].join(" "),
        &[],
        1000,
    )
    .await;
    let mut input = query();
    input.queries = groups;
    input.limit = 1;
    let result = super::super::owner_search::search_on(
        &store.db,
        "1",
        &input,
        &AssistantDataPolicy::default(),
        5000,
    )
    .await
    .unwrap();
    assert_eq!(result.fragments.len(), 1);
    assert_eq!(result.fragments[0].source.source_id, complete);
}
