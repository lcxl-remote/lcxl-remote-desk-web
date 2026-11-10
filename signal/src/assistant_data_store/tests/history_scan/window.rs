use super::*;

#[tokio::test]
async fn original_windows_keep_message_and_fragment_order_when_wall_clock_timestamps_go_backwards()
{
    let store = setup().await;
    let device = device(&store, "window-order").await;
    let before = capture_at(
        &store,
        &device,
        "conversation",
        "before",
        "background",
        &[],
        3000,
    )
    .await;
    let fragments = capture_many_at(
        &store,
        &device,
        "conversation",
        "long-message",
        &"report part ".repeat(1000),
        &[],
        2000,
    )
    .await;
    assert!(fragments.len() > 1);
    let after = capture_at(
        &store,
        &device,
        "conversation",
        "after",
        "completed receipt",
        &[],
        1000,
    )
    .await;
    let frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let query = window_query(
        &store,
        &frame,
        &fragments[1],
        ConversationReadView::Context,
        10,
        &[device],
        "context",
    )
    .await;
    let page = scan::window::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    let mut expected = vec![before];
    expected.extend(fragments);
    expected.push(after);
    assert_eq!(ids(&page.items), expected);
    assert!(!page.has_more);
    assert!(page.previous.is_none());
    assert!(page.next.is_none());
}

#[tokio::test]
async fn both_window_frontiers_expand_without_repeating_seen_versions_or_losing_the_opposite_side()
{
    let store = setup().await;
    let device = device(&store, "window-pages").await;
    let mut expected = Vec::new();
    for index in 0..11 {
        expected.push(
            capture_at(
                &store,
                &device,
                "conversation",
                &format!("message-{index}"),
                "report",
                &[],
                1000,
            )
            .await,
        );
    }
    let frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let mut query = window_query(
        &store,
        &frame,
        &expected[5],
        ConversationReadView::Context,
        3,
        &[device],
        "pages",
    )
    .await;
    let first = scan::window::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&first.items), expected[4..7]);
    assert!(first.previous.is_some());
    assert!(first.next.is_some());
    let mut all = ids(&first.items);
    let mut previous = first.previous;
    let mut next = first.next;
    accept(&mut query, &first.items, &[previous.clone(), next.clone()]);
    for _ in 0..10 {
        let Some(cursor) = previous.clone().or(next.clone()) else {
            break;
        };
        let page = scan::window::scan_on(
            &store.db,
            &frame.borrowed(),
            &query,
            Some(&cursor),
            &owners,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(!page.items.is_empty());
        for source in ids(&page.items) {
            assert!(!all.contains(&source));
            all.push(source);
        }
        previous = page.previous;
        next = page.next;
        accept(&mut query, &page.items, &[previous.clone(), next.clone()]);
    }
    assert!(previous.is_none());
    assert!(next.is_none());
    assert_eq!(all.len(), 11);
    assert_eq!(
        all.into_iter().collect::<BTreeSet<_>>(),
        expected.into_iter().collect::<BTreeSet<_>>()
    );
}

#[tokio::test]
async fn latest_reads_are_independent_of_search_keywords_and_old_windows_do_not_admit_late_records()
{
    let store = setup().await;
    let device = device(&store, "window-latest").await;
    let original = capture_at(
        &store,
        &device,
        "conversation",
        "requirement",
        "send report",
        &[],
        1000,
    )
    .await;
    let middle = capture_at(
        &store,
        &device,
        "conversation",
        "progress",
        "waiting for native receipt",
        &[],
        1100,
    )
    .await;
    let old_frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let mut old = window_query(
        &store,
        &old_frame,
        &original,
        ConversationReadView::Latest,
        1,
        std::slice::from_ref(&device),
        "old-latest",
    )
    .await;
    let late = capture_at(
        &store,
        &device,
        "conversation",
        "outcome",
        "delivery succeeded",
        &[],
        1200,
    )
    .await;
    let previous = scan::window::scan_on(
        &store.db,
        &old_frame.borrowed(),
        &old,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&previous.items), [middle]);
    assert!(previous.previous.is_some());
    assert!(previous.next.is_none());
    assert!(!ids(&previous.items).contains(&late));
    let frame = Frame::new(&store, &device).await;
    let mut watermark = old.watermark();
    watermark.corpus_generation = frame.generation;
    old.refresh(
        RetrievalSelection {
            devices: vec![device.clone()],
            watermark,
            candidates: Vec::new(),
            reasons: Vec::new(),
        },
        5000,
    )
    .unwrap();
    let refreshed = scan::window::scan_on(
        &store.db,
        &old_frame.borrowed(),
        &old,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&refreshed.items), ids(&previous.items));
    assert!(!ids(&refreshed.items).contains(&late));
    let query = window_query(
        &store,
        &frame,
        &original,
        ConversationReadView::Latest,
        1,
        &[device],
        "new-latest",
    )
    .await;
    let page = scan::window::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(ids(&page.items), [late]);
}

#[tokio::test]
async fn invalid_or_deleted_original_anchors_do_not_silently_fall_back_to_other_conversation_text()
{
    let store = setup().await;
    let a = device(&store, "window-A").await;
    let b = device(&store, "window-B").await;
    let one = capture_at(
        &store,
        &a,
        "root-A",
        "anchor",
        "original requirement",
        &[],
        1000,
    )
    .await;
    capture_at(&store, &a, "root-A", "later", "later result", &[], 1100).await;
    let two = capture_at(
        &store,
        &b,
        "root-B",
        "other",
        "other device result",
        &[],
        1200,
    )
    .await;
    let frame = Frame::new(&store, &a).await;
    let owners = BTreeSet::from([a.clone(), b.clone()]);
    let query = window_query(
        &store,
        &frame,
        &two,
        ConversationReadView::Context,
        10,
        std::slice::from_ref(&a),
        "wrong-device",
    )
    .await;
    assert!(
        scan::window::scan_on(
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
    let query = window_query(
        &store,
        &frame,
        &one,
        ConversationReadView::Context,
        10,
        &[a],
        "deleted-anchor",
    )
    .await;
    let row = history::Entity::find()
        .filter(history::Column::Id.eq(desk_assistant_data_store::source_rows::history(&one)))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    origin::Entity::update_many()
        .col_expr(origin::Column::DeletedAtMs, Expr::value(Some(2000_i64)))
        .filter(origin::Column::Id.eq(row.origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        scan::window::scan_on(
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
async fn window_boundaries_reject_unissued_pages_and_stale_source_or_recipient_bindings() {
    let store = setup().await;
    let device = device(&store, "window-bindings").await;
    let source = capture_at(
        &store,
        &device,
        "conversation",
        "anchor",
        "original",
        &[],
        1000,
    )
    .await;
    let mut frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let query = window_query(
        &store,
        &frame,
        &source,
        ConversationReadView::Context,
        10,
        &[device],
        "bindings",
    )
    .await;
    let unissued = PageAnchor::Window {
        admitted_generation: frame.generation,
        fragment_index: 0,
        history_id: desk_assistant_data_store::source_rows::history(&source),
        forward: true,
    };
    assert!(
        scan::window::scan_on(
            &store.db,
            &frame.borrowed(),
            &query,
            Some(&unissued),
            &owners,
            Duration::from_secs(60)
        )
        .await
        .is_err()
    );
    assert!(
        scan::window::scan_on(
            &store.db,
            &frame.borrowed(),
            &query,
            None,
            &owners,
            Duration::ZERO
        )
        .await
        .is_err()
    );
    frame.destination = destination("other-model");
    assert!(
        scan::window::scan_on(
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
async fn windows_and_continuations_accept_late_fragments_of_one_complete_source() {
    let store = setup().await;
    let device = device(&store, "full-window").await;
    let fragments = capture_many_at(
        &store,
        &device,
        "full-window-root",
        "full-window",
        &"background part ".repeat(5000),
        &[],
        1000,
    )
    .await;
    assert!(fragments.len() > 16);
    let frame = Frame::new(&store, &device).await;
    let owners = BTreeSet::from([device.clone()]);
    let mut query = window_query(
        &store,
        &frame,
        fragments.last().unwrap(),
        ConversationReadView::Context,
        3,
        &[device],
        "late-fragments",
    )
    .await;
    let first = scan::window::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        None,
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(!first.items.is_empty());
    assert!(
        first
            .items
            .iter()
            .all(|item| item.position.fragment_index > 8)
    );
    let cursor = first.previous.clone().unwrap();
    assert!(matches!(&cursor, PageAnchor::Window { fragment_index, .. } if *fragment_index > 8));
    accept(&mut query, &first.items, &[Some(cursor.clone())]);
    query.validate().unwrap();
    let next = scan::window::scan_on(
        &store.db,
        &frame.borrowed(),
        &query,
        Some(&cursor),
        &owners,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(!next.items.is_empty());
    assert!(
        next.items
            .iter()
            .all(|item| item.position.fragment_index > 8)
    );
    assert!(
        ids(&first.items)
            .iter()
            .all(|id| !ids(&next.items).contains(id))
    );
}
