use super::*;
use crate::entity::{agent_schedule, agent_schedule_run};
use desk_diagnose_core::session::PersistedAgentSession;
use sea_orm::PaginatorTrait;
use serde_json::{Value, json};

async fn scenario() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
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
            model: Some("fake-model".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new());
    for (step, (name, args)) in cases().into_iter().enumerate() {
        let original = serde_json::to_string_pretty(&args).unwrap();
        let call_id = format!("schedule-call-{step}");
        let run_db = db.clone();
        let run_connections = connections.clone();
        let running = actix_web::rt::spawn(async move {
            run_turn_inner(
                run_connections,
                run_db,
                format!("schedule-transport-{step}"),
                "controller".into(),
                "offline-host".into(),
                crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                "device".into(),
                AiAssistantAsk {
                    client_timezone: "UTC".into(),
                    question: "Prepare a daily read-only report at 08:30 in Asia/Shanghai".into(),
                    client_message_id: format!("schedule-input-{step}"),
                    conversation_id: Some("schedule-input-entry".into()),
                    ..Default::default()
                },
                None,
            )
            .await;
        });
        tokio::time::timeout(MODEL_IO_TIMEOUT,capture_one_openai_request_with_reply(&listener, |_| memory_sse(json!({"tool_calls":[{
            "index":0,"id":call_id,"type":"function","function":{"name":name,"arguments":original}
        }]}),"tool_calls"))).await.expect("tool request timed out");
        tokio::time::timeout(
            MODEL_IO_TIMEOUT,
            capture_one_openai_request_with_reply(&listener, |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                let tool = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|m| m["role"] == "tool" && m["tool_call_id"] == call_id)
                    .unwrap();
                assert_feedback(step, tool["content"].as_str().unwrap());
                memory_sse(json!({"content":"schedule-answer"}), "stop")
            }),
        )
        .await
        .expect("feedback request timed out");
        if step == 0 {
            tokio::time::timeout(
                MODEL_IO_TIMEOUT,
                capture_one_openai_request_with_reply(&listener, |body| {
                    let request: Value = serde_json::from_slice(body).unwrap();
                    assert_eq!(
                        request["response_format"]["json_schema"]["name"],
                        "conversation_title"
                    );
                    memory_sse(json!({"content":r#"{"title":"Daily report"}"#}), "stop")
                }),
            )
            .await
            .expect("title request timed out");
        }
        running.await.unwrap();
        let row = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        let call = session
            .conversation
            .iter()
            .flat_map(|m| &m.tool_calls)
            .find(|c| c.id == call_id)
            .unwrap();
        assert_eq!(call.arguments_json, original);
        assert_eq!(
            agent_schedule::Entity::find().count(&db).await.unwrap(),
            u64::from(step >= 2)
        );
        assert_eq!(
            agent_schedule_run::Entity::find().count(&db).await.unwrap(),
            0
        );
    }
    let draft = agent_schedule::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(draft.status, "draft");
    assert_eq!(draft.kind, "fresh_task");
    assert!(draft.active_run_id.is_none());
    let mut session = PersistedAgentSession::decode_json(
        &crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .state_json,
    )
    .unwrap();
    session.turn_state = desk_diagnose_core::session::TurnState::Running;
    let invalid = desk_diagnose_core::chat::ToolCall {
        id: "invalid-direct".into(),
        name: cases()[0].0.into(),
        arguments_json: cases()[0].1.to_string(),
    };
    let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let error = desk_diagnose_core::seam::SessionSeam::manage_schedule_tool(
        &store,
        &mut session,
        &invalid,
        Default::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.kind,
        desk_agent_protocol::AgentErrorKind::InvalidInput
    );
    assert!(error.message.contains("fresh_task"));
    assert_eq!(agent_schedule::Entity::find().count(&db).await.unwrap(), 1);
}

fn cases() -> Vec<(&'static str, Value)> {
    let time = json!({"timezone":"Asia/Shanghai","reference_date":"2026-11-01","local_time":"08:30:00","rule":{"kind":"daily"}});
    vec![
        (
            "request_scheduled_task",
            json!({"kind":"conversation_resume","title":"每日检查","prompt":"Prepare a read-only status report","time_input":time}),
        ),
        ("list_scheduled_tasks", json!({"limit":20})),
        (
            "request_scheduled_task",
            json!({"kind":"fresh_task","title":"每日检查","prompt":"Prepare a read-only status report","time_input":time}),
        ),
        ("list_scheduled_tasks", json!({"limit":20})),
    ]
}

fn assert_feedback(step: usize, content: &str) {
    if step == 0 {
        assert!(content.contains("once"), "{content}");
        assert!(!content.contains("current conversation authority"));
    } else {
        let value: Value = serde_json::from_str(content).unwrap();
        if step == 2 {
            assert_eq!(value["kind"], "fresh_task");
            assert_eq!(value["state"], "draft");
        } else {
            assert_eq!(value["ok"], true);
            assert_eq!(
                value["tasks"].as_array().unwrap().len(),
                usize::from(step == 3)
            );
        }
    }
}

#[test]
fn schedule_input_errors_and_pretty_json_calls_reach_the_real_store_on_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .name("schedule-call-integrity".into())
            .stack_size(bytes)
            .spawn(|| {
                actix_web::rt::System::new().block_on(async {
                    tokio::time::timeout(SCENARIO_TIMEOUT, scenario())
                        .await
                        .expect("schedule scenario timed out")
                })
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
