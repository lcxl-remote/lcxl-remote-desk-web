//! Actual OSS entry, SQLite and synthetic HTTP model; no device UI is exercised.

use super::*;
use crate::agent_run_event_store::{InputSubject, SignalAgentRunEventStore};
use desk_agent_protocol::ai_assistant::AiAssistantObjectContextOperation;
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};

#[actix_web::test]
async fn actual_owner_turn_commits_memory_without_hidden_model_calls_and_answers_after_the_tool() {
    owner_memory_turn(false).await;
}

#[actix_web::test]
async fn actual_owner_turn_updates_observed_memory_and_refreshes_context_before_answering() {
    owner_memory_turn(true).await;
}

async fn owner_memory_turn(update: bool) {
    use crate::entity::agent_memory;
    use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettingsUpdate;
    use serde_json::{Value, json};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let mut model_config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}")),
        api_key: Some("test-only-key".into()),
        max_context_bytes: Some(131_072),
        ..Default::default()
    };
    crate::model_provider::save(&db, model_config.clone())
        .await
        .unwrap();
    let settings = crate::assistant_data_config::settings(&db).await;
    crate::assistant_data_config::update_settings(
        &db,
        &AssistantDataSettingsUpdate {
            expected_revision: settings.revision,
            automatic_memory_enabled: true,
            model_history_search_enabled: settings.model_history_search_enabled,

            explanation_version: settings.explanation_version,
        },
    )
    .await
    .unwrap();
    let target = if update {
        Some(crate::assistant_data_store::tests::seed_personal_memory(&db, "update-owner").await)
    } else {
        None
    };
    let (arrived, ready) = tokio::sync::oneshot::channel();
    let (release, resume) = tokio::sync::oneshot::channel();
    let gateway = actix_web::rt::spawn(async move {
        let first = capture_one_openai_request_with_reply(&listener, |body| {
            let mut arguments = json!({"operation":"remember","content":"The owner prefers concise Chinese replies"});
            if let Some(target) = &target {
                let request:Value = serde_json::from_slice(body).unwrap();
                assert!(request["messages"].to_string().contains(target), "the update target must be observed by the real model");
                arguments["operation"] = json!("update"); arguments["memory_id"] = json!(target);
                arguments["expected_revision"] = json!(1);
            }
            memory_sse(json!({"tool_calls":[{"index":0,"id":"owner-memory-write","type":"function","function":{
                "name":"manage_memories","arguments":arguments.to_string()}}]}), "tool_calls")
        }).await;
        let final_request = capture_one_openai_request_with_reply_paused(
            &listener,
            |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                let tool = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| {
                        message["role"] == "tool" && message["tool_call_id"] == "owner-memory-write"
                    })
                    .expect("ordinary tool feedback expected, no extraction call");
                let result: Value =
                    serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
                assert_eq!(result["outcome"], "applied");
                assert_eq!(result["reason"], "committed");
                assert!(result["memory"]["memory_id"].is_string());
                assert_eq!(result["memory"].as_object().unwrap().len(), 2);
                memory_sse(json!({"content":"memory-answer"}), "stop")
            },
            Some((arrived, resume)),
        )
        .await;
        vec![first, final_request]
    });
    let ask = AiAssistantAsk {
        client_timezone: "UTC".into(),
        question: "Always use concise Chinese replies".into(),
        client_message_id: "memory-owner-input".into(),
        conversation_id: Some(
            if update {
                "memory-update-entry"
            } else {
                "memory-entry"
            }
            .into(),
        ),
        ..Default::default()
    };
    let connections = web::Data::new(SharedConnectionMap::new());
    let running_db = db.clone();
    let running_connections = connections.clone();
    let running_ask = ask.clone();
    let running = actix_web::rt::spawn(async move {
        run_turn_inner(
            running_connections,
            running_db,
            "memory-transport".into(),
            "controller".into(),
            "offline-host".into(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            "device".into(),
            running_ask,
            None,
        )
        .await;
    });
    tokio::time::timeout(MODEL_IO_TIMEOUT, ready)
        .await
        .unwrap()
        .unwrap();
    let memories = agent_memory::Entity::find().all(&db).await.unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(
        memories[0].body,
        "The owner prefers concise Chinese replies"
    );
    let commits = crate::entity::agent_memory_commit::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(memories[0].revision, if update { 2 } else { 1 });
    assert_eq!(
        crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .len(),
        2
    );
    release.send(()).unwrap();
    tokio::time::timeout(SCENARIO_TIMEOUT, running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(gateway.await.unwrap().len(), 2);
    let run_id = derive_conversation_key(
        "1",
        "device",
        Some(if update {
            "memory-update-entry"
        } else {
            "memory-entry"
        }),
        "unused",
    );
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert!(
        snapshot.terminal_error.is_none(),
        "{:?}",
        snapshot.terminal_error
    );
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("memory-answer")
    );
    let before = crate::entity::agent_memory_revision::Entity::find()
        .all(&db)
        .await
        .unwrap();
    run_turn_inner(
        connections.clone(),
        db.clone(),
        "memory-retry".into(),
        "controller".into(),
        "offline-host".into(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        "device".into(),
        ask,
        None,
    )
    .await;
    assert_eq!(
        crate::entity::agent_memory_revision::Entity::find()
            .all(&db)
            .await
            .unwrap(),
        before
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    model_config.base_url = Some(format!("http://{}", listener.local_addr().unwrap()));
    crate::model_provider::save(&db, model_config)
        .await
        .unwrap();
    let recall_gateway = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert!(request["messages"].as_array().unwrap().iter().any(|message|
                message["role"] == "user" && message["content"].as_str().is_some_and(|text|
                    text.contains("The owner prefers concise Chinese replies"))),
                "a new conversation must receive the saved memory, not just the write receipt");
            memory_sse(json!({"tool_calls":[{"index":0,"id":"recall-memory","type":"function","function":{
                "name":"search_memories","arguments":json!({"queries":["Chinese"],"limit":5}).to_string()}}]}), "tool_calls")
        }).await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let tool = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == "recall-memory"
                })
                .unwrap();
            let result: Value = serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
            assert_eq!(result["memories"].as_array().unwrap().len(), 1);
            assert_eq!(
                result["memories"][0]["content"],
                "The owner prefers concise Chinese replies"
            );
            memory_sse(json!({"content":"memory-recalled"}), "stop")
        })
        .await;
    });
    run_turn_inner(
        connections,
        db.clone(),
        "recall-transport".into(),
        "controller".into(),
        "offline-host".into(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        "device".into(),
        AiAssistantAsk {
            client_timezone: "UTC".into(),
            question: "What did I ask you to remember?".into(),
            client_message_id: "recall-owner-input".into(),
            conversation_id: Some("memory-recall-entry".into()),
            ..Default::default()
        },
        None,
    )
    .await;
    tokio::time::timeout(SCENARIO_TIMEOUT, recall_gateway)
        .await
        .unwrap()
        .unwrap();
    let recalled = sessions
        .read_snapshot(&derive_conversation_key(
            "1",
            "device",
            Some("memory-recall-entry"),
            "unused",
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest_committed_answer(&recalled).as_deref(),
        Some("memory-recalled")
    );
    db.close().await.unwrap();
}

pub(super) fn memory_sse(delta: serde_json::Value, reason: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices":[{"delta":delta,"finish_reason":null}]}),
        serde_json::json!({"choices":[{"delta":{},"finish_reason":reason}],"usage":{"prompt_tokens":5,"completion_tokens":2}})
    )
}

#[actix_web::test]
async fn production_input_entry_freezes_objects_and_rejects_changed_retry_without_model_io() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let capture = actix_web::rt::spawn(capture_one_openai_request(listener));
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}")),
        api_key: Some("test-only-key".into()),
        max_context_bytes: Some(131_072),
        runtime_max_output_tokens: 128_000,
        ..Default::default()
    };
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(&db, config).await.unwrap();
    let client_id = "input-entry";
    let run_id = derive_conversation_key("1", "device", Some(client_id), "unused");
    let update = AiAssistantObjectContextUpdate {
        conversation_id: client_id.into(),
        client_request_id: "original-object".into(),
        operation: AiAssistantObjectContextOperation::AttachTerminalOutput {
            object_ref: ObjectRef {
                token: "opaque-original-file".into(),
                snapshot_id: "worker".into(),
                object_kind: ObjectKind::TerminalOutput,
                expires_at: (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
            },
            display_summary: "not-file-type-authority".into(),
        },
    };
    apply_object_context_update(
        db.clone(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        "device".into(),
        &update,
    )
    .await
    .unwrap();
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(Some(client_id.into()), AgentSessionSurface::AiAssistant);
    let before = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    let selected = before.context_attachments[0].clone();
    let ask = AiAssistantAsk {
        client_timezone: "UTC".into(),
        temporary_conversation: None,
        question: "original requirement".into(),
        client_message_id: "original-message".into(),
        conversation_id: Some(client_id.into()),
        selected_attachment_ids: vec![selected.attachment_id.clone()],
        ..Default::default()
    };
    let connections = web::Data::new(SharedConnectionMap::new());
    tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "transport".into(),
            "controller".into(),
            "offline-host".into(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            "device".into(),
            ask.clone(),
            None,
        ),
    )
    .await
    .unwrap();
    let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert_eq!(snapshot.input_revision, 1);
    assert_eq!(snapshot.handled_input_seq, 1, "{snapshot:?}");
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("captured-ok")
    );
    let body = tokio::time::timeout(MODEL_IO_TIMEOUT, capture)
        .await
        .unwrap()
        .unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("original requirement"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["max_tokens"],
        128_000
    );
    assert!(!body.contains("opaque-original-file"));
    let events = SignalAgentRunEventStore::new(db.clone());
    let frozen = events
        .original_read_context(
            InputSubject {
                run_id: &run_id,
                actor_id: "1",
                device_id: "device",
                client_conversation_id: Some(client_id),
            },
            1,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frozen.object_attachments, [selected]);
    events
        .validate_object_read(
            InputSubject {
                run_id: &run_id,
                actor_id: "1",
                device_id: "device",
                client_conversation_id: Some(client_id),
            },
            1,
            &frozen,
            &frozen.object_attachments[0].envelope.allowed_destinations[0],
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let original_events = events.user_followups_after(&run_id, 0, 10).await.unwrap();
    let saved = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    for mutation in 0..3 {
        let mut retry = ask.clone();
        match mutation {
            0 => {} // Exact retry returns the original settled answer.
            1 => retry.question = "changed requirement".into(),
            _ => retry.selected_attachment_ids.clear(),
        }
        tokio::time::timeout(
            SCENARIO_TIMEOUT,
            run_turn_inner(
                connections.clone(),
                db.clone(),
                format!("retry-{mutation}"),
                "controller".into(),
                "offline-host".into(),
                crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                "device".into(),
                retry,
                None,
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            events.user_followups_after(&run_id, 0, 10).await.unwrap(),
            original_events
        );
        assert_eq!(
            crate::entity::agent_session::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            saved
        );
    }
    let receipts = crate::entity::model_egress_receipt::Entity::find()
        .all(&db)
        .await
        .unwrap();
    // The closed synthetic gateway also records one naming attempt. Exact
    // transport replay above must not create another main or title call.
    assert_eq!(receipts.len(), 2);
    db.close().await.unwrap();
}

mod retention;

mod schedule_input;
