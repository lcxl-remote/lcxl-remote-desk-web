use super::keyword_search::{capture_at, device};
use super::*;
use desk_agent_protocol::ai_assistant::owner_history::AssistantHistoryConversationsRequest;

#[tokio::test]
async fn owner_conversation_list_deduplicates_roots_and_pages_without_execution_or_connections() {
    let store = setup().await;
    let device = device(&store, "owner-list").await;
    for index in 0..23 {
        for part in 0..2 {
            capture_at(
                &store,
                &device,
                &format!("list-root-{index}"),
                &format!("list-{index}-{part}"),
                &format!("Conversation {index} fragment {part}"),
                &[],
                1000 + index,
            )
            .await;
        }
    }
    let policy = AssistantDataPolicy::default();
    let mut request = AssistantHistoryConversationsRequest {
        limit: 7,
        cursor: None,
    };
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let page =
            super::super::owner_conversations::list_on(&store.db, "1", &request, &policy, 5000)
                .await
                .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= 32 * 1024);
        for item in page.conversations {
            assert!(seen.insert(item.conversation_id));
            assert!(!item.execution_available);
            assert!(!item.active);
            assert!(item.source.content.is_some());
        }
        request.cursor = page.next_cursor;
        if request.cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 23);
}
#[tokio::test]
async fn owner_conversation_list_hides_expired_sources_and_rejects_foreign_or_changed_cursor_scope()
{
    let store = setup().await;
    let device = device(&store, "owner-list-scope").await;
    for index in 0..3 {
        capture_at(
            &store,
            &device,
            &format!("root-{index}"),
            &format!("list-source-{index}"),
            "viewable",
            &[],
            1000,
        )
        .await;
    }
    let mut policy = AssistantDataPolicy::default();
    let mut request = AssistantHistoryConversationsRequest {
        limit: 1,
        cursor: None,
    };
    let page = super::super::owner_conversations::list_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    request.cursor = page.next_cursor;
    policy.revision += 1;
    assert!(
        super::super::owner_conversations::list_on(&store.db, "1", &request, &policy, 5000)
            .await
            .is_err()
    );
    policy.revision -= 1;
    assert!(
        super::super::owner_conversations::list_on(&store.db, "2", &request, &policy, 5000)
            .await
            .is_err()
    );
    request.cursor = None;
    let expired = super::super::owner_conversations::list_on(
        &store.db,
        "1",
        &request,
        &policy,
        u64::from(policy.history_retention_days) * 86_400_000 + 5000,
    )
    .await
    .unwrap();
    assert!(expired.conversations.is_empty());
}

#[actix_web::test]
async fn owner_conversation_http_signal_deletes_an_offline_retained_root_without_recreating_execution()
 {
    // The production entry point uses one process-wide database. Isolate that
    // original bootstrap so parallel functional tests keep their own contexts.
    const CHILD: &str = "ASSISTANT_HISTORY_HTTP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "assistant_data_store::tests::owner_conversations::owner_conversation_http_signal_deletes_an_offline_retained_root_without_recreating_execution", "--nocapture"])
            .env(CHILD, "1").output().await.unwrap();
        assert!(
            output.status.success(),
            "isolated HTTP case: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use crate::{
        controller::{
            ai_assistant_session::delete_ai_assistant_session,
            assistant_data::{get_assistant_source, list_retained_assistant_conversations},
        },
        entity::{
            agent_assistant_device_binding as bindings, agent_file_recovery_cleanup as removal,
            agent_session,
        },
    };
    use actix_web::{App, test, web};
    use desk_signal_facade::model::connection::SharedConnectionMap;
    use sea_orm::PaginatorTrait;
    let directory = tempfile::tempdir().unwrap();
    let config = crate::config::test_support::context().await;
    let db = crate::db::init_db(directory.path().to_str().unwrap(), config)
        .await
        .unwrap()
        .clone();
    let store = TestStore { db };
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    let device = device(&store, "signal-http-retained").await;
    let binding = bindings::Entity::find()
        .filter(bindings::Column::BindingId.eq(&device))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    let root = "signal-http-retained-root";
    let source = capture_at(
        &store,
        &device,
        root,
        "signal-http-source",
        "A retained report",
        &[],
        now as u64,
    )
    .await;
    for (id, actor) in [(root, "1"), ("foreign-http-root", "2")] {
        removal::ActiveModel {
            conversation_id: Set(id.into()),
            actor_id: Set(actor.into()),
            device_id: Set(binding.audience.clone()),
            created_at_unix_ms: Set(now),
            history_deleted_at_unix_ms: Set(None),
            next_attempt_at_unix_ms: Set(now),
            attempts: Set(0),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
    }
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(SharedConnectionMap::default()))
            .service(
                web::scope("/api")
                    .service(delete_ai_assistant_session)
                    .service(list_retained_assistant_conversations)
                    .service(get_assistant_source),
            ),
    )
    .await;
    let page: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri("/api/my/ai-assistant-history/conversations?limit=20")
            .to_request(),
    )
    .await;
    assert_eq!(page["success"], true);
    assert!(
        page["data"]["conversations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["conversation_id"] == root && item["execution_available"] == false)
    );
    let foreign: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::post()
            .uri("/api/my/ai-assistant-session/delete")
            .set_json(serde_json::json!({"connection":"", "session":"foreign-http-root"}))
            .to_request(),
    )
    .await;
    assert_ne!(foreign["success"], true);
    for _ in 0..2 {
        let response: serde_json::Value = test::call_and_read_body_json(
            &app,
            test::TestRequest::post()
                .uri("/api/my/ai-assistant-session/delete")
                .set_json(serde_json::json!({"connection":"", "session":root}))
                .to_request(),
        )
        .await;
        assert_eq!(
            response["success"], true,
            "original offline delete and retry: {response}"
        );
    }
    assert_eq!(
        agent_session::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert!(
        removal::Entity::find_by_id(root)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .history_deleted_at_unix_ms
            .is_some()
    );
    assert!(
        removal::Entity::find_by_id("foreign-http-root")
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .history_deleted_at_unix_ms
            .is_none()
    );
    let source: serde_json::Value = test::call_and_read_body_json(
        &app,
        test::TestRequest::get()
            .uri(&format!("/api/my/ai-assistant-source?source_id={source}"))
            .to_request(),
    )
    .await;
    assert!(source["data"]["content"].is_null());
    assert_ne!(source["data"]["availability"], "available");
}

#[tokio::test]
async fn owner_list_scans_past_twenty_unreadable_fragments_with_the_configured_budget() {
    use crate::entity::agent_assistant_history as history;
    let store = setup().await;
    let device = device(&store, "list-scan-budget").await;
    let good = capture_at(
        &store,
        &device,
        "readable-root",
        "good-input",
        "readable",
        &[],
        1000,
    )
    .await;
    for index in 0..40 {
        capture_at(
            &store,
            &device,
            "readable-root",
            &format!("unreadable-{index}"),
            "unreadable",
            &[],
            1001 + index,
        )
        .await;
    }
    history::Entity::update_many()
        .col_expr(
            // A body that no longer matches its indexed span is unreadable.
            history::Column::Content,
            sea_orm::sea_query::Expr::value("x"),
        )
        .filter(history::Column::Id.ne(desk_assistant_data_store::source_rows::history(&good)))
        .exec(&store.db)
        .await
        .unwrap();
    let mut policy = AssistantDataPolicy::default();
    policy.limits.processing.history_scan_page_size = 7;
    let request = AssistantHistoryConversationsRequest {
        limit: 20,
        cursor: None,
    };
    policy.limits.retrieval.owner_scan_documents = 8;
    let limited =
        super::super::owner_conversations::list_on(&store.db, "1", &request, &policy, 5000)
            .await
            .unwrap();
    assert!(limited.conversations.is_empty());
    assert!(limited.partial);
    policy.limits.retrieval.owner_scan_documents = 128;
    let found = super::super::owner_conversations::list_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(found.conversations.len(), 1);
    assert_eq!(found.conversations[0].source.source_id, good);
}

#[tokio::test]
async fn owner_conversation_list_uses_the_same_page_target_as_search_and_messages() {
    let store = setup().await;
    let device = device(&store, "list-shared-page").await;
    for index in 0..20 {
        capture_at(
            &store,
            &device,
            &format!("large-list-{index}"),
            &format!("large-input-{index}"),
            &"原文".repeat(600),
            &[],
            1000 + index,
        )
        .await;
    }
    let request = AssistantHistoryConversationsRequest {
        limit: 20,
        cursor: None,
    };
    let mut policy = AssistantDataPolicy::default();
    let large = super::super::owner_conversations::list_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(large.conversations.len(), 20);
    assert!(serde_json::to_vec(&large).unwrap().len() > 30 * 1024);
    policy.limits.conversation.page_bytes = 1024;
    let first = super::super::owner_conversations::list_on(&store.db, "1", &request, &policy, 5000)
        .await
        .unwrap();
    assert_eq!(first.conversations.len(), 1);
    let next = super::super::owner_conversations::list_on(
        &store.db,
        "1",
        &AssistantHistoryConversationsRequest {
            cursor: first.next_cursor,
            ..request
        },
        &policy,
        5000,
    )
    .await
    .unwrap();
    assert_eq!(next.conversations.len(), 1);
    assert_ne!(
        first.conversations[0].conversation_id,
        next.conversations[0].conversation_id
    );
}
