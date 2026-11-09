use super::*;
use crate::entity::{agent_memory, agent_memory_commit, agent_memory_revision};
use sea_orm::PaginatorTrait;
use serde_json::{Value, json};

#[test]
fn tool_error_then_memory_write_and_update_keep_retention_and_recover_missing_control_policy() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        std::thread::Builder::new()
            .name("memory-control-retention".into())
            .stack_size(bytes)
            .spawn(|| {
                actix_web::rt::System::new().block_on(async {
                    tokio::time::timeout(SCENARIO_TIMEOUT, retention_scenario())
                        .await
                        .expect("memory retention scenario timed out")
                })
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

async fn retention_scenario() {
    {
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
        let mut memory_id = None;
        for step in 0..3 {
            let call_id = format!("retention-tool-{step}");
            let mut args = if step == 0 {
                json!({"operation":"forget","memory_id":"missing-required-revision"})
            } else {
                json!({"operation":"remember","content":"Use concise Chinese replies"})
            };
            if step == 2 {
                args["operation"] = json!("update");
                // The model copies the alias its earlier write result showed.
                assert!(memory_id.is_some());
                args["memory_id"] = json!("mem1");
                args["expected_revision"] = json!(1);
                args["content"] = json!("Use detailed Chinese replies");
            }
            let running_db = db.clone();
            let running_connections = connections.clone();
            let running = actix_web::rt::spawn(async move {
                run_turn_inner(
                    running_connections,
                    running_db,
                    format!("retention-transport-{step}"),
                    "controller".into(),
                    "offline-host".into(),
                    crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                    "device".into(),
                    AiAssistantAsk {
                        client_timezone: "UTC".into(),
                        question: "Remember my Chinese reply preference".into(),
                        client_message_id: format!("retention-input-{step}"),
                        conversation_id: Some("retention-entry".into()),
                        ..Default::default()
                    },
                    None,
                )
                .await;
            });
            tokio::time::timeout(MODEL_IO_TIMEOUT, capture_one_openai_request_with_reply(&listener, |_| memory_sse(json!({"tool_calls":[{
                "index":0,"id":call_id,"type":"function","function":{"name":"manage_memories","arguments":args.to_string()}
            }]}), "tool_calls"))).await.unwrap_or_else(|error| panic!("tool request step {step}: {error}; task finished={}", running.is_finished()));
            tokio::time::timeout(
                MODEL_IO_TIMEOUT,
                capture_one_openai_request_with_reply(&listener, |body| {
                    let request: Value = serde_json::from_slice(body).unwrap();
                    let tool = request["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|message| {
                            message["tool_call_id"] == call_id && message["role"] == "tool"
                        })
                        .unwrap();
                    let content = tool["content"].as_str().unwrap();
                    if step == 0 {
                        assert!(content.contains("expected_revision"));
                    } else {
                        let result: Value = serde_json::from_str(content).unwrap();
                        assert_eq!(result["reason"], "committed", "step={step}: {result}");
                        assert_eq!(result["outcome"], "applied");
                    }
                    memory_sse(json!({"content":"memory-answer"}), "stop")
                }),
            )
            .await
            .expect("model feedback request timed out");
            if step == 0 {
                tokio::time::timeout(
                    MODEL_IO_TIMEOUT,
                    capture_one_openai_request_with_reply(&listener, |body| {
                        let request: Value = serde_json::from_slice(body).unwrap();
                        assert!(
                            request["messages"][0]["content"].as_str().is_some_and(
                                |text| text.contains("Create a short conversation title")
                            )
                        );
                        memory_sse(json!({"content":r#"{"title":"Reply preference"}"#}), "stop")
                    }),
                )
                .await
                .expect("title request timed out");
            }
            tokio::time::timeout(SCENARIO_TIMEOUT, running)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                agent_memory_commit::Entity::find()
                    .count(&db)
                    .await
                    .unwrap(),
                step as u64
            );
            assert_eq!(
                agent_memory_revision::Entity::find()
                    .count(&db)
                    .await
                    .unwrap(),
                step as u64
            );
            if step > 0 {
                let fact = agent_memory::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(fact.revision, step as i64);
                assert_eq!(
                    fact.body,
                    if step == 1 {
                        "Use concise Chinese replies"
                    } else {
                        "Use detailed Chinese replies"
                    }
                );
                memory_id = Some(fact.memory_id);
            }
        }
    }
}
