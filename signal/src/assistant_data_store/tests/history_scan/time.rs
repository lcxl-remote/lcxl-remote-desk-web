use super::*;

#[tokio::test]
async fn time_enumeration_keeps_all_matches_in_a_long_conversation_and_pages_without_relevance_cutoffs()
 {
    let store = setup().await;
    let device = device(&store, "time-many").await;
    let mut expected = Vec::new();
    for index in 0..60 {
        expected.push(
            capture_at(
                &store,
                &device,
                "long-conversation",
                &format!("record-{index}"),
                "report 邮件",
                &[],
                1000 + index,
            )
            .await,
        );
    }
    let mut frame = Frame::new(&store, &device).await;
    frame.policy.limits.processing.history_scan_page_size = 7;
    let owners = BTreeSet::from([device.clone()]);
    let mut query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations {
            request: temporal(),
        },
        std::slice::from_ref(&device),
        "descending",
    );
    let first = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(first.items.len(), 50);
    assert!(first.has_more);
    assert!(first.effective_queries.is_empty());
    assert_eq!(
        ids(&first.items),
        expected.iter().rev().take(50).cloned().collect::<Vec<_>>()
    );
    let cursor = first.continuation.clone().unwrap();
    accept(&mut query, &first.items, &[first.continuation]);
    let second = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        Some(&cursor),
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(!second.has_more);
    assert_eq!(second.items.len(), 10);
    let mut all = ids(&first.items);
    all.extend(ids(&second.items));
    assert_eq!(all.iter().collect::<BTreeSet<_>>().len(), 60);
    assert_eq!(all, expected.iter().rev().cloned().collect::<Vec<_>>());
    let mut request = temporal();
    request.order = HistoryOrder::Asc;
    let query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations { request },
        &[device],
        "ascending",
    );
    let first = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(
        ids(&first.items),
        expected.into_iter().take(50).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn time_pages_apply_device_conversation_root_and_optional_keyword_bounds_before_returning_sources()
 {
    let store = setup().await;
    let a = device(&store, "time-A").await;
    let b = device(&store, "time-B").await;
    let one = capture_at(&store, &a, "root-A", "one", "report report 邮件", &[], 1000).await;
    let two = capture_at(&store, &b, "root-B", "two", "report 邮件", &[], 1000).await;
    capture_at(
        &store,
        &a,
        "current-root",
        "current",
        "report 邮件",
        &[],
        1000,
    )
    .await;
    let unrelated = capture_at(
        &store,
        &a,
        "unrelated",
        "unrelated",
        "unrelated topic",
        &[],
        1000,
    )
    .await;
    let frame = Frame::new(&store, &a).await;
    let owners = BTreeSet::from([a.clone(), b.clone()]);
    let mut request = temporal();
    request.scope = AssistantDeviceScope::CurrentDevice;
    request.queries = vec!["report 邮件".into()];
    let query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations {
            request: request.clone(),
        },
        std::slice::from_ref(&a),
        "current",
    );
    let page = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page.items), std::slice::from_ref(&one));
    assert_eq!(page.effective_queries.len(), 1);
    request.scope = AssistantDeviceScope::AllMyDevices;
    request.conversation_ids = vec!["root-B".into()];
    let query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations { request },
        &[a.clone(), b.clone()],
        "specific",
    );
    let page = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page.items), [two]);
    let query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations {
            request: temporal(),
        },
        &[a, b],
        "without-keywords",
    );
    let page = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(ids(&page.items).contains(&unrelated));
    assert!(ids(&page.items).contains(&one));
    assert_eq!(page.items.len(), 3);
}

#[tokio::test]
async fn time_scans_exclude_owner_viewable_no_export_text_and_derived_sources_with_deleted_dependencies()
 {
    let store = setup().await;
    let device = device(&store, "time-private").await;
    let allowed = capture_at(&store, &device, "allowed", "allowed", "report", &[], 1000).await;
    let hidden = capture_at(&store, &device, "hidden", "hidden", "report", &[], 1000).await;
    private_source(&store, &hidden).await;
    let frame = Frame::new(&store, &device).await;
    let row = history::Entity::find()
        .filter(history::Column::Id.eq(desk_assistant_data_store::source_rows::history(&allowed)))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let parent = source_reads::readable_history_on(&store.db, &frame.borrowed(), &allowed, 1)
        .await
        .unwrap();
    let derived = capture_at(
        &store,
        &device,
        "derived",
        "derived",
        "report derived",
        &parent.usage.history_dependencies,
        1000,
    )
    .await;
    origin::Entity::update_many()
        .col_expr(origin::Column::DeletedAtMs, Expr::value(Some(2000_i64)))
        .filter(origin::Column::OriginId.eq(row.origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    let frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations {
            request: temporal(),
        },
        &[device],
        "permissions",
    );
    let page = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(page.items.is_empty());
    assert!(page.reasons.contains(&"source_unavailable".into()));
    assert!(!ids(&page.items).contains(&hidden));
    assert!(!ids(&page.items).contains(&derived));
}

#[tokio::test]
async fn native_time_enumeration_keeps_intervals_uncertain_and_never_substitutes_recording_time() {
    let store = setup().await;
    let device = device(&store, "time-native").await;
    use desk_diagnose_core::ai_assistant::native_history::{
        NativeHistoryMetadata, NativeHistoryTime, NativeTimeBasis,
    };
    use sha2::{Digest, Sha256};
    let mut timed = Vec::new();
    for (label, time) in [
        (
            "exact",
            NativeHistoryTime::Exact {
                at_unix_ms: 2000,
                basis: NativeTimeBasis::NativeRecorded,
            },
        ),
        (
            "interval",
            NativeHistoryTime::Interval {
                from_unix_ms: 1800,
                until_unix_ms: 2200,
                basis: NativeTimeBasis::ProviderAckObserved,
            },
        ),
    ] {
        let text = "sent receipt";
        let metadata = NativeHistoryMetadata {
            format_version: 1,
            native_proof_sha256: "a".repeat(64),
            projected_content_sha256: format!("{:x}", Sha256::digest(text.as_bytes())),
            projection_partial: false,
            time: Some(time),
            status_snapshot: None,
        };
        let sources = capture_many_native_at(
            &store,
            &device,
            label,
            label,
            text,
            &[],
            3000,
            Some(&metadata),
        )
        .await;
        assert_eq!(sources.len(), 1);
        timed.push(sources[0].clone());
    }
    let [exact, interval]: [String; 2] = timed.try_into().unwrap();
    let unknown = capture_at(&store, &device, "unknown", "unknown", "draft", &[], 2000).await;
    let frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let mut request = temporal();
    request.time_field = HistoryTimeField::NativeActionAt;
    request.time_range.as_mut().unwrap().from_unix_ms = 1900;
    request.time_range.as_mut().unwrap().until_unix_ms = 2100;
    let query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations { request },
        &[device],
        "native",
    );
    let page = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page.items), [exact, interval]);
    assert!(!ids(&page.items).contains(&unknown));
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.time_key_unix_ms)
            .collect::<Vec<_>>(),
        [2000, 1800]
    );
    assert!(page.reasons.contains(&"native_action_time_interval".into()));
    assert!(
        page.reasons
            .contains(&"unknown_native_times_excluded".into())
    );
}

#[tokio::test]
async fn work_time_and_revocation_limits_never_turn_an_unexpanded_time_range_into_a_complete_negative()
 {
    let store = setup().await;
    let device = device(&store, "time-bounds").await;
    for index in 0..41 {
        capture_at(
            &store,
            &device,
            "conversation",
            &format!("source-{index}"),
            "report",
            &[],
            1000 + index,
        )
        .await;
    }
    let mut frame = Frame::new(&store, &device).await;
    frame.policy.max_retrieval_document_visits = 40;
    let owners = BTreeSet::from([device.clone()]);
    let query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations {
            request: temporal(),
        },
        &[device],
        "bounds",
    );
    let page = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(page.has_more);
    assert!(page.items.len() <= 40);
    assert!(page.reasons.contains(&"document_work_limit".into()));
    let page = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert!(page.items.is_empty());
    assert!(page.has_more);
    assert_eq!(page.continuation, Some(PageAnchor::Start));
    assert!(page.reasons.contains(&"search_time_limit".into()));
    runtime::Entity::update_many()
        .col_expr(
            runtime::Column::ControlGeneration,
            Expr::value(frame.access.control_generation as i64 + 1),
        )
        .filter(runtime::Column::ActorId.eq("1"))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        scan::time::scan_on(
            &store.db,
            &frame.borrowed(),
            &query,
            None,
            &owners,
            Duration::from_secs(60)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn permission_refresh_preserves_original_observation_and_requires_a_new_query_to_see_late_receipts()
 {
    let store = setup().await;
    let device = device(&store, "time-observation").await;
    let old = capture_at(&store, &device, "conversation", "old", "report", &[], 1000).await;
    let frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let mut query = prepare(
        &frame,
        FrozenRetrievalRequest::Conversations {
            request: temporal(),
        },
        std::slice::from_ref(&device),
        "original",
    );
    let first = scan::time::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&first.items), [old]);
    accept(&mut query, &first.items, &[]);
    let late = capture_at(
        &store,
        &device,
        "conversation",
        "late",
        "late receipt",
        &[],
        1500,
    )
    .await;
    let mut current = Frame::new(&store, &device).await;
    let mut watermark = query.watermark();
    watermark.corpus_generation = current.generation;
    query
        .refresh(
            RetrievalSelection {
                devices: vec![device.clone()],
                watermark,
                candidates: Vec::new(),
                reasons: Vec::new(),
            },
            5000,
        )
        .unwrap();
    assert!(query.snapshot.corpus_generation > query.source_observation_generation);
    let fresh = prepare(
        &current,
        FrozenRetrievalRequest::Conversations {
            request: temporal(),
        },
        &[device],
        "fresh",
    );
    current.generation = query.source_observation_generation;
    let old = scan::time::scan_on(
        &store.db,
        &current.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(old.items.is_empty());
    current.generation = fresh.source_observation_generation;
    let latest = scan::time::scan_on(
        &store.db,
        &current.borrowed(),
        &fresh,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(ids(&latest.items).contains(&late));
}
