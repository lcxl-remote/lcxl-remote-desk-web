//! Production entry with a local model gateway and a reopened SQLite database.
use super::*;

#[actix_web::test]
async fn expired_history_continues_without_exporting_old_content() {
    use sea_orm::{ActiveModelTrait, IntoActiveModel, Set};
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap();
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let first_listener = listener.clone();
    let first = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_sse(first_listener.as_ref(), &sse("expired-answer-marker"))
            .await
    });
    ask(
        &db,
        "expired-history",
        "first",
        "expired-question-marker".into(),
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), first)
        .await
        .unwrap()
        .unwrap();
    let row = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut session =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json).unwrap();
    let original_len = session.conversation.len();
    for message in &mut session.conversation {
        if message.role == ChatRole::Assistant {
            message
                .data_envelope
                .as_mut()
                .unwrap()
                .retention
                .expires_at_unix_ms = Some(1);
        }
    }
    let mut active = row.into_active_model();
    active.state_json = Set(serde_json::to_string(&session).unwrap());
    active.update(&db).await.unwrap();
    let second = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_sse(listener.as_ref(), &sse("continued-ok")).await
    });
    ask(&db, "expired-history", "second", "continue".into()).await;
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(!body.contains("expired-question-marker"));
    assert!(!body.contains("expired-answer-marker"));
    let run = derive_conversation_key("7", "device", Some("expired-history"), "unused");
    let snapshot = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot(&run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("continued-ok")
    );
    assert!(snapshot.terminal_error.is_none());
    assert!(snapshot.messages.len() > original_len);
    assert!(
        snapshot
            .messages
            .iter()
            .any(|message| message.text == "expired-answer-marker")
    );
    assert!(
        snapshot
            .context_notices
            .iter()
            .any(|notice| notice.checkpoint_generation.is_none())
    );
}

#[actix_web::test]
async fn production_compaction_answers_and_restores_checkpoint_from_sqlite() {
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("session.db").display()
    );
    let db = Database::connect(&url).await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}")),
        api_key: Some("test-only-key".into()),
        max_context_bytes: Some(131_072),
        ..Default::default()
    };
    crate::model_provider::save(&db, config).await.unwrap();
    let client = "compaction-entry";
    let run = derive_conversation_key("7", "device", Some(client), "unused");
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(Some(client.into()), AgentSessionSurface::DeviceAssistant);
    for index in 0..3 {
        let first_listener = listener.clone();
        let first = actix_web::rt::spawn(async move {
            capture_one_openai_request_with_sse(first_listener.as_ref(), &sse(&"z".repeat(16_000)))
                .await
        });
        ask(&db, client, &format!("first-{index}"), "a".repeat(16_000)).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), first)
            .await
            .unwrap()
            .unwrap();
        let snapshot = sessions.read_snapshot(&run).await.unwrap().unwrap();
        assert_eq!(latest_committed_answer(&snapshot).unwrap().len(), 16_000);
        assert_eq!(
            snapshot
                .messages
                .iter()
                .filter(
                    |message| message.role == ChatRole::Assistant && message.text.len() == 16_000
                )
                .count(),
            index + 1
        );
        assert!(
            snapshot.context_notices.is_empty(),
            "seed turns must not compress"
        );
    }
    let before = sessions.read_snapshot(&run).await.unwrap().unwrap();
    assert_eq!(latest_committed_answer(&before).unwrap().len(), 16_000);
    let source = before
        .messages
        .iter()
        .find(|message| message.role == ChatRole::User)
        .unwrap()
        .message_id
        .clone();
    let second_listener = listener.clone();
    let second = actix_web::rt::spawn(async move {
        let summary =
            serde_json::json!({"goals":[{"text":"Earlier goal","source_message_ids":[source]}]})
                .to_string();
        let compression =
            capture_one_openai_request_with_sse(second_listener.as_ref(), &sse(&summary)).await;
        let answer =
            capture_one_openai_request_with_sse(second_listener.as_ref(), &sse("second-answer"))
                .await;
        (compression, answer)
    });
    ask(&db, client, "second", "b".repeat(16_000)).await;
    let after = sessions.read_snapshot(&run).await.unwrap().unwrap();
    assert_eq!(
        latest_committed_answer(&after).as_deref(),
        Some("second-answer"),
        "{:?}",
        after.task_status_projection
    );
    let (compression, answer) = tokio::time::timeout(std::time::Duration::from_secs(5), second)
        .await
        .unwrap()
        .unwrap();
    let compression: serde_json::Value = serde_json::from_slice(&compression).unwrap();
    assert!(
        compression
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
    );
    assert!(String::from_utf8(answer).unwrap().contains("Earlier goal"));
    assert!(
        after
            .context_notices
            .iter()
            .any(|notice| notice.checkpoint_generation == Some(1))
    );
    let saved_context = context(&db).await;
    drop(sessions);
    db.close().await.unwrap();
    let reopened = Database::connect(&url).await.unwrap();
    crate::db::initialize_schema(&reopened).await.unwrap();
    let restored = crate::agent_session_store::SignalAgentSessionStore::new(reopened.clone())
        .read_snapshot(&run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context(&reopened).await, saved_context);
    assert_eq!(
        latest_committed_answer(&restored).as_deref(),
        Some("second-answer")
    );
    reopened.close().await.unwrap();
}

fn sse(answer: &str) -> String {
    let frame = serde_json::json!({"choices":[{"delta":{"content":answer},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2}});
    format!("data: {frame}\n\ndata: [DONE]\n\n")
}

async fn context(db: &sea_orm::DatabaseConnection) -> serde_json::Value {
    let row = crate::entity::agent_session::Entity::find()
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let session =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json).unwrap();
    serde_json::to_value(session.model_context_state).unwrap()
}

async fn ask(db: &sea_orm::DatabaseConnection, client: &str, message: &str, question: String) {
    let result = tokio::time::timeout(
        // Cold macOS trust-store initialization can itself exceed 15 seconds.
        // This watchdog bounds the harness, not the production model timeout.
        std::time::Duration::from_secs(60),
        run_turn_inner(
            web::Data::new(SharedConnectionMap::new()),
            db.clone(),
            format!("transport-{message}"),
            "controller".into(),
            "offline-host".into(),
            7,
            "device".into(),
            DeviceAssistantAsk {
                question,
                client_message_id: message.into(),
                conversation_id: Some(client.into()),
                ..Default::default()
            },
            None,
        ),
    )
    .await;
    if result.is_err() {
        let row = crate::entity::agent_session::Entity::find()
            .one(db)
            .await
            .unwrap()
            .unwrap();
        let session =
            desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                .unwrap();
        let receipts = crate::entity::model_egress_receipt::Entity::find()
            .all(db)
            .await
            .unwrap();
        panic!(
            "{message}: timeout; state={:?}, receipts={:?}, tail={:?}",
            session.turn_state,
            receipts
                .iter()
                .map(|receipt| &receipt.state)
                .collect::<Vec<_>>(),
            session
                .conversation
                .iter()
                .rev()
                .take(2)
                .map(|message| (
                    message.role,
                    message.text.chars().take(150).collect::<String>()
                ))
                .collect::<Vec<_>>()
        );
    }
}
