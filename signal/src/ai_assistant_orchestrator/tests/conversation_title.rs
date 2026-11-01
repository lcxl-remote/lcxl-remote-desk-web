//! Exercise naming through the actual entry, model transport and native ledgers.
use super::*;
use sea_orm::{ColumnTrait, QueryFilter};
use serde_json::{Value, json};

#[actix_web::test]
async fn original_owner_answer_names_the_conversation_through_metered_egress() {
    naming(false, None).await;
}

#[actix_web::test]
async fn malformed_title_preserves_the_original_answer_and_actual_usage() {
    naming(true, None).await;
}

async fn naming(malformed: bool, stack_bytes: Option<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("title.sqlite").display()
    );
    let mut options = sea_orm::ConnectOptions::new(&url);
    options.max_connections(1);
    let db = crate::config::test_support::Database::connect(options)
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("title-test-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            runtime_max_output_tokens: 4096,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let gateway = actix_web::rt::spawn(async move {
        let main = capture_one_openai_request_with_reply(&listener, |_| {
            original_input::memory_sse(json!({"content":"The original answer"}), "stop")
        })
        .await;
        let title = tokio::time::timeout(MODEL_IO_TIMEOUT, capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert_eq!(request["model"], "title-test-model");
            assert!(request["messages"][0]["content"].as_str().is_some_and(|text| text.contains("Create a short conversation title")));
            assert!(request.get("response_format").is_none());
            assert_eq!(request["messages"].as_array().unwrap().len(), 2);
            assert_eq!(request["messages"][1]["content"], "Find my report");
            assert!(request["tools"].as_array().is_none_or(|tools| tools.is_empty()));
            original_input::memory_sse(json!({"content":if malformed { "invalid JSON" } else { r#"{"title":"Find my report"}"# }}), "stop")
        })).await.expect("the native title call must reach the gateway");
        (main, title)
    });
    let ask = AiAssistantAsk {
        client_timezone: "UTC".into(),
        question: "Find my report".into(),
        client_message_id: "title-owner-input".into(),
        conversation_id: Some("title-entry".into()),
        ..Default::default()
    };
    let run_db = db.clone();
    let run = move || async move {
        run_turn_inner(
            web::Data::new(SharedConnectionMap::new()),
            run_db,
            "title-transport".into(),
            "controller".into(),
            "offline-host".into(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            "device".into(),
            ask,
            None,
        )
        .await;
    };
    if let Some(bytes) = stack_bytes {
        let (sent, done) = tokio::sync::oneshot::channel();
        let child = std::thread::Builder::new()
            .stack_size(bytes)
            .spawn(move || {
                actix_web::rt::System::new().block_on(run());
                sent.send(()).unwrap();
            })
            .unwrap();
        tokio::time::timeout(SCENARIO_TIMEOUT, done)
            .await
            .unwrap()
            .unwrap();
        child.join().unwrap();
    } else {
        tokio::time::timeout(SCENARIO_TIMEOUT, run()).await.unwrap();
    }
    gateway.await.unwrap();
    // Runtime shutdown can cancel SQLx's asynchronous connection return. Retire
    // the last connection explicitly to verify that its replacement retains the fixture.
    db.get_sqlite_connection_pool()
        .acquire()
        .await
        .unwrap()
        .close()
        .await
        .unwrap();
    let row = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.title.as_deref(),
        (!malformed).then_some("Find my report")
    );
    let session =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert_eq!(
        session.turn_state,
        desk_diagnose_core::session::TurnState::Idle
    );
    assert!(
        session
            .conversation
            .iter()
            .any(|message| message.text == "The original answer")
    );
    assert!(
        !session
            .conversation
            .iter()
            .any(|message| message.text.contains(r#"{"title""#))
    );
    let calls = crate::entity::model_egress_receipt::Entity::find()
        .filter(
            crate::entity::model_egress_receipt::Column::ContextConversationId
                .eq(&row.conversation_id),
        )
        .all(&db)
        .await
        .unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|call| call.state == "succeeded" && call.usage_json.is_some())
    );
    let expected = row;
    db.close().await.unwrap();
    let reopened = crate::config::test_support::Database::connect(&url)
        .await
        .unwrap();
    assert_eq!(
        crate::entity::agent_session::Entity::find()
            .one(&reopened)
            .await
            .unwrap(),
        Some(expected)
    );
    reopened.close().await.unwrap();
}

#[actix_web::test]
async fn original_answer_naming_and_native_storage_fit_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        naming(false, Some(bytes)).await;
    }
}
