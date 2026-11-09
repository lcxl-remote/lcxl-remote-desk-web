use super::super::history_view;
use super::keyword_search::{Frame, capture, device, private_source};
use super::*;
use crate::entity::{
    agent_assistant_history as history, agent_assistant_origin as origin,
    agent_assistant_search_document as document,
};
use desk_agent_protocol::ai_assistant::source::SourceAvailability;
use desk_diagnose_core::ai_assistant::retrieval_query::QuerySourceVersion;
use sea_orm::sea_query::Expr;

async fn version(store: &TestStore, source: &str) -> QuerySourceVersion {
    let row = history::Entity::find()
        .filter(history::Column::Id.eq(desk_assistant_data_store::source_rows::history(source)))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    QuerySourceVersion {
        source_id: row.source_id(),
        revision: row.revision as u64,
    }
}

#[tokio::test]
async fn exact_fragment_locator_hash_and_full_unicode_body_survive_bounded_search_snippets() {
    let store = setup().await;
    let device = device(&store, "view-unicode").await;
    let content = "邮件📧 report \"quoted\" \\path\n".repeat(60);
    let source = capture(
        &store,
        &device,
        "original-root",
        "view-unicode",
        &content,
        &[],
    )
    .await;
    let expected = version(&store, &source).await;
    let frame = Frame::new(&store, &device).await;
    let view = history_view::read_on(&store.db, &frame.borrowed(), &expected)
        .await
        .unwrap();
    assert_eq!(view.version().unwrap(), expected);
    let hit = view.search_hit(512).unwrap();
    let message = view.message().unwrap();
    let expanded = view.search_hit(2048).unwrap();
    assert!(expanded.snippet.len() > 512 && expanded.snippet.len() <= 2048);
    assert!(content.starts_with(&expanded.snippet));
    assert_eq!(expanded.locator, hit.locator);
    let small = view.search_hit(17).unwrap();
    assert!(small.snippet.len() <= 17 && content.starts_with(&small.snippet));
    assert_eq!(hit.source_id, source);
    assert_eq!(hit.locator.conversation_id, "original-root");
    assert_eq!(hit.locator.root_conversation_id, "original-root");
    assert_eq!(
        hit.locator.message_id.as_deref(),
        Some(message.message_id.as_str())
    );
    assert_eq!(hit.locator.byte_start, 0);
    assert_eq!(hit.locator.byte_end, content.len() as u32);
    assert!(hit.snippet.len() <= 512);
    assert!(content.starts_with(&hit.snippet));
    assert!(hit.snippet.len() < content.len());
    assert_eq!(message.content, content);
    assert_eq!(message.role, "user");
    assert_eq!(message.statement_timezone, "UTC");
    assert_eq!(hit.native_action_at_unix_ms, None);
    assert_eq!(hit.native_action_interval, None);
    assert_eq!(hit.native_status_summary, None);
    assert_eq!(view.source.usage.roots[0].source_id, source);
}

#[tokio::test]
async fn owner_view_access_cannot_substitute_for_the_separate_model_source_gate() {
    let store = setup().await;
    let device = device(&store, "view-private").await;
    let source = capture(
        &store,
        &device,
        "root-A",
        "view-private",
        "private report",
        &[],
    )
    .await;
    private_source(&store, &source).await;
    let expected = version(&store, &source).await;
    let frame = Frame::new(&store, &device).await;
    let row = history::Entity::find()
        .filter(history::Column::Id.eq(desk_assistant_data_store::source_rows::history(&source)))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let owner =
        super::super::owner_source::history_source_on(&store.db, "1", &row, &frame.policy, 5000)
            .await
            .unwrap();
    assert!(matches!(
        owner.availability,
        SourceAvailability::Available | SourceAvailability::Redacted
    ));
    assert!(
        history_view::read_on(&store.db, &frame.borrowed(), &expected)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn origin_dependencies_and_index_byte_spans_are_rechecked_before_creating_any_model_hit() {
    for damage_dependency in [false, true] {
        let store = setup().await;
        let device = device(&store, "view-corrupt").await;
        let base = capture(
            &store,
            &device,
            "base-root",
            "view-base",
            "background report",
            &[],
        )
        .await;
        let frame = Frame::new(&store, &device).await;
        let base_view =
            history_view::read_on(&store.db, &frame.borrowed(), &version(&store, &base).await)
                .await
                .unwrap();
        let source = capture(
            &store,
            &device,
            "derived-root",
            "view-derived",
            "derived report",
            &base_view.source.usage.history_dependencies,
        )
        .await;
        let expected = version(&store, &source).await;
        let frame = Frame::new(&store, &device).await;
        history_view::read_on(&store.db, &frame.borrowed(), &expected)
            .await
            .unwrap();
        if damage_dependency {
            origin::Entity::update_many()
                .col_expr(origin::Column::DeletedAtMs, Expr::value(Some(4000_i64)))
                .filter(origin::Column::OriginId.eq(&base_view.source.row.origin_id))
                .exec(&store.db)
                .await
                .unwrap();
        } else {
            document::Entity::update_many()
                .col_expr(document::Column::ByteEnd, Expr::value(1_i32))
                .filter(document::Column::SourceId.eq(&source))
                .exec(&store.db)
                .await
                .unwrap();
        }
        assert!(
            history_view::read_on(&store.db, &frame.borrowed(), &expected)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn late_sources_wrong_versions_and_chat_timestamps_cannot_be_presented_as_native_receipts() {
    let store = setup().await;
    let device = device(&store, "view-time").await;
    let before = Frame::new(&store, &device).await;
    let source = capture(
        &store,
        &device,
        "root-A",
        "view-time",
        "I sent a report yesterday",
        &[],
    )
    .await;
    let mut expected = version(&store, &source).await;
    assert!(
        history_view::read_on(&store.db, &before.borrowed(), &expected)
            .await
            .is_err()
    );
    let frame = Frame::new(&store, &device).await;
    expected.revision += 1;
    assert!(
        history_view::read_on(&store.db, &frame.borrowed(), &expected)
            .await
            .is_err()
    );
    expected.revision -= 1;
    history::Entity::update_many()
        .col_expr(
            history::Column::NativeActionAtMs,
            Expr::value(Some(1000_i64)),
        )
        .col_expr(
            history::Column::TimeBasis,
            Expr::value(Some("chat_recording_time".to_string())),
        )
        .filter(history::Column::Id.eq(desk_assistant_data_store::source_rows::history(&source)))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        history_view::read_on(&store.db, &frame.borrowed(), &expected)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn long_source_tail_is_indexed_located_and_read_beyond_old_source_capacity() {
    let store = setup().await;
    let device = device(&store, "full-source").await;
    let text = format!("{} tail_delivery_confirmed", "background ".repeat(30000));
    let sources = super::keyword_search::capture_many_at(
        &store,
        &device,
        "full-source-root",
        "full-source",
        &text,
        &[],
        1000,
    )
    .await;
    assert!(sources.len() > 64);
    let tail = sources.last().unwrap();
    let frame = Frame::new(&store, &device).await;
    let request = serde_json::from_value(serde_json::json!({
        "queries":["tail_delivery_confirmed"], "limit":10
    }))
    .unwrap();
    let found = super::super::keyword_search::scan_on(
        &store.db,
        &frame.borrowed(),
        &request,
        &std::collections::BTreeSet::from([device]),
        std::time::Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert_eq!(found.ranking.documents.len(), 1);
    assert_eq!(&found.ranking.documents[0].document.source_id, tail);
    let stored = document::Entity::find()
        .filter(document::Column::SourceId.eq(tail))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(!stored.partial);
    let view = history_view::read_on(&store.db, &frame.borrowed(), &version(&store, tail).await)
        .await
        .unwrap();
    let hit = view.search_hit(512).unwrap();
    assert!(hit.locator.fragment_index > 8);
    assert_eq!(hit.locator.byte_end as usize, text.len());
    assert!(hit.locator.byte_start as usize > 256 * 1024);
    assert!(
        view.message()
            .unwrap()
            .content
            .ends_with("tail_delivery_confirmed")
    );
    assert!(hit.snippet.len() <= 512);
}
