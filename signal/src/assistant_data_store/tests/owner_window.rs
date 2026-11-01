use super::super::owner_window;
use super::keyword_search::{capture_at, device};
use super::*;
use crate::entity::agent_assistant_history as history;
use desk_agent_protocol::ai_assistant::history_window::{
    AssistantHistoryWindowRequest, AssistantHistoryWindowView,
};
use sea_orm::sea_query::Expr;

fn request(source: &str, view: AssistantHistoryWindowView) -> AssistantHistoryWindowRequest {
    AssistantHistoryWindowRequest {
        source_id: source.into(),
        revision: 1,
        view,
        cursor: None,
    }
}

#[tokio::test]
async fn retained_window_survives_missing_execution_state_and_freezes_tail_across_pages() {
    let store = setup().await;
    let device = device(&store, "owner-window").await;
    let mut sources = Vec::new();
    for index in 0..45 {
        sources.push(
            capture_at(
                &store,
                &device,
                "retained-root",
                &format!("window-{index}"),
                &format!("Original message {index} 📧"),
                &[],
                if index % 2 == 0 { 1000 } else { 900 },
            )
            .await,
        );
    }
    // No execution session was ever created. Source admission order, rather
    // than equal/backward message timestamps, defines the owner window.
    let policy = AssistantDataPolicy::default();
    let mut input = request(&sources[22], AssistantHistoryWindowView::Context);
    let context = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(context.fragments.len(), 20);
    assert!(
        context
            .fragments
            .iter()
            .any(|item| item.source.source_id == sources[22])
    );
    assert_eq!(context.anchor_source_id, sources[22]);
    assert!(context.previous_cursor.is_some());
    assert!(context.next_cursor.is_some());
    input.view = AssistantHistoryWindowView::Latest;
    let latest = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(
        latest.fragments.last().unwrap().source.source_id,
        sources[44]
    );
    assert!(latest.next_cursor.is_none());
    let late = capture_at(
        &store,
        &device,
        "retained-root",
        "late-window",
        "Later admitted source",
        &[],
        950,
    )
    .await;
    input.cursor = latest.previous_cursor.clone();
    let older = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(
        older.observed_through_generation,
        latest.observed_through_generation
    );
    assert_eq!(
        older.fragments.last().unwrap().source.source_id,
        sources[24]
    );
    assert!(
        !older
            .fragments
            .iter()
            .any(|item| item.source.source_id == late)
    );
    assert_ne!(older.previous_cursor, input.cursor);
    input.cursor = None;
    let fresh = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(fresh.fragments.last().unwrap().source.source_id, late);
}

#[tokio::test]
async fn retained_pages_reject_wrong_owner_version_cross_conversation_and_revoked_anchor() {
    let store = setup().await;
    let device = device(&store, "owner-window-fences").await;
    let first = capture_at(&store, &device, "root-A", "window-A", "A source", &[], 1000).await;
    let second = capture_at(
        &store,
        &device,
        "root-A",
        "window-A2",
        "A later source",
        &[],
        1000,
    )
    .await;
    let other = capture_at(&store, &device, "root-B", "window-B", "B source", &[], 1000).await;
    for index in 0..22 {
        capture_at(
            &store,
            &device,
            "root-A",
            &format!("fence-tail-{index}"),
            "Later source",
            &[],
            1000,
        )
        .await;
    }
    let policy = AssistantDataPolicy::default();
    let mut input = request(&second, AssistantHistoryWindowView::Latest);
    assert!(
        owner_window::read_on(&store.db, "2", &input, &policy, 5000)
            .await
            .is_err()
    );
    input.revision = 2;
    assert!(
        owner_window::read_on(&store.db, "1", &input, &policy, 5000)
            .await
            .is_err()
    );
    input.revision = 1;
    let page = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    let context = owner_window::read_on(
        &store.db,
        "1",
        &request(&first, AssistantHistoryWindowView::Context),
        &policy,
        5000,
    )
    .await
    .unwrap();
    let mut forged = request(&other, AssistantHistoryWindowView::Context);
    forged.cursor = context.next_cursor;
    assert!(forged.cursor.is_some());
    assert!(
        owner_window::read_on(&store.db, "1", &forged, &policy, 5000)
            .await
            .is_err()
    );
    history::Entity::update_many()
        .col_expr(history::Column::DeletedAtMs, Expr::value(5000_i64))
        .filter(history::Column::SourceId.eq(&second))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        owner_window::read_on(&store.db, "1", &input, &policy, 5001)
            .await
            .is_err()
    );
    assert_eq!(page.fragments.len(), 20);
}

async fn large_window_fixture() -> (TestStore, Vec<String>, String) {
    let store = setup().await;
    let device = device(&store, "owner-window-bytes").await;
    let text = "报价📧\\\"\n".repeat(260);
    let mut sources = Vec::new();
    for index in 0..25 {
        sources.push(
            capture_at(
                &store,
                &device,
                "budget-root",
                &format!("bytes-{index}"),
                &text,
                &[],
                1000,
            )
            .await,
        );
    }
    (store, sources, text)
}

async fn exercise_large_window(store: TestStore, sources: Vec<String>, text: String) {
    let policy = AssistantDataPolicy::default();
    let mut input = request(&sources[5], AssistantHistoryWindowView::Latest);
    let mut seen = std::collections::BTreeSet::new();
    for index in 0..3 {
        let future = owner_window::read_on(&store.db, "1", &input, &policy, 5000);
        let bytes = std::mem::size_of_val(&future);
        eprintln!("raw retained-window future: {bytes} bytes");
        assert!(bytes <= 16 * 1024);
        let page = future.await.unwrap();
        assert!(!page.partial);
        assert_eq!(page.fragments.len(), if index == 0 { 20 } else { 5 });
        if index == 0 {
            assert!(serde_json::to_vec(&page).unwrap().len() > 32 * 1024);
        }
        for fragment in page.fragments {
            assert_eq!(fragment.source.content.as_deref(), Some(text.as_str()));
            assert!(seen.insert(fragment.source.source_id));
        }
        let Some(cursor) = page.previous_cursor else {
            break;
        };
        input.cursor = Some(cursor);
    }
    assert_eq!(seen.len(), sources.len());
}

#[test]
fn retained_window_keeps_large_pages_whole_and_does_not_skip_rows_on_small_stacks() {
    const CHILD: &str = "LRD_TEST_RETAINED_WINDOW_STACK_BYTES";
    if let Ok(value) = std::env::var(CHILD) {
        let bytes = value.parse::<usize>().unwrap();
        assert!([1_048_576, 2_097_152].contains(&bytes));
        actix_web::rt::System::new().block_on(async {
            let (store, sources, text) = large_window_fixture().await;
            let (done, ready) = tokio::sync::oneshot::channel();
            let thread = std::thread::Builder::new()
                .name("retained-window-stack".into())
                .stack_size(bytes)
                .spawn(move || {
                    actix_web::rt::System::new().block_on(async move {
                        eprintln!("retained-window real reads on {bytes} byte thread");
                        Box::pin(exercise_large_window(store, sources, text)).await;
                        let _ = done.send(());
                    });
                })
                .unwrap();
            ready.await.unwrap();
            thread.join().unwrap();
        });
        return;
    }
    let test = concat!(
        module_path!(),
        "::retained_window_keeps_large_pages_whole_and_does_not_skip_rows_on_small_stacks"
    );
    let test = test.split_once("::").unwrap().1;
    for bytes in [1_048_576, 2_097_152] {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture"])
            .env_remove("RUST_MIN_STACK")
            .env(CHILD, bytes.to_string())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "retained window failed on {bytes} bytes: {status}"
        );
    }
}

#[tokio::test]
async fn tail_discovery_scans_past_thirty_two_rows_and_keeps_a_valid_anchor_on_budget_exhaustion() {
    let store = setup().await;
    let device = device(&store, "tail-scan-budget").await;
    let good = capture_at(
        &store,
        &device,
        "tail-root",
        "good-input",
        "readable original",
        &[],
        1000,
    )
    .await;
    for index in 0..40 {
        capture_at(
            &store,
            &device,
            "tail-root",
            &format!("unreadable-{index}"),
            "unreadable",
            &[],
            1001 + index,
        )
        .await;
    }
    history::Entity::update_many()
        .col_expr(history::Column::ContentSha256, Expr::value("0".repeat(64)))
        .filter(history::Column::SourceId.ne(&good))
        .exec(&store.db)
        .await
        .unwrap();
    let mut policy = AssistantDataPolicy::default();
    policy.limits.processing.history_scan_page_size = 7;
    let input = request(&good, AssistantHistoryWindowView::Latest);
    let found = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(found.fragments.len(), 1);
    assert_eq!(found.fragments[0].source.source_id, good);
    policy.limits.retrieval.owner_scan_documents = 8;
    let partial = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert!(partial.partial);
    assert_eq!(partial.fragments.len(), 1);
    assert_eq!(partial.fragments[0].source.source_id, good);
}

#[tokio::test]
async fn small_window_page_target_preserves_the_anchor_and_forward_progress() {
    let store = setup().await;
    let device = device(&store, "window-page-target").await;
    let mut sources = Vec::new();
    for index in 0..6 {
        sources.push(
            capture_at(
                &store,
                &device,
                "window-target-root",
                &format!("window-target-{index}"),
                &"原文".repeat(600),
                &[],
                1000 + index,
            )
            .await,
        );
    }
    let mut policy = AssistantDataPolicy::default();
    policy.limits.conversation.page_bytes = 1024;
    let mut input = request(&sources[2], AssistantHistoryWindowView::Context);
    let page = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(page.fragments.len(), 1);
    assert_eq!(page.fragments[0].source.source_id, sources[2]);
    assert_eq!(
        page.fragments[0].source.content.as_deref(),
        Some("原文".repeat(600).as_str())
    );
    input.cursor = page.next_cursor;
    let next = owner_window::read_on(&store.db, "1", &input, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(next.fragments.len(), 1);
    assert_eq!(next.fragments[0].source.source_id, sources[3]);
}
