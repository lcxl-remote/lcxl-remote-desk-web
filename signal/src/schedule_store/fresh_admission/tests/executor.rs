//! Real fresh-task dispatch; publication evidence and the device remain fixtures.
use super::*;
use desk_agent_protocol::computer_use::{COMPUTER_USE_SCHEMA_VERSION, ComputerUseReadiness};
use sea_orm::EntityTrait;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[actix_web::test]
async fn concurrent_fresh_scans_settle_one_run_with_one_model_call() {
    exercise(100_000).await;
}

#[actix_web::test]
async fn insufficient_model_budget_is_reported_without_dispatch_or_audit_intent() {
    exercise(10_000).await;
}

async fn exercise(model_budget: u64) {
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(move || {
        let requests = observed.clone();
        actix_web::App::new().route("/v1/chat/completions", actix_web::web::post().to(
            move |body: actix_web::web::Json<serde_json::Value>| {
                let payload = body.to_string();
                assert!(payload.contains("ORIGINAL_FRESH_PERSONALIZATION"));
                assert!(!payload.contains("LATER_FRESH_PERSONALIZATION"));
                requests.fetch_add(1, Ordering::SeqCst);



                async move {


                    actix_web::HttpResponse::Ok().content_type("text/event-stream").body(concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"fresh task complete\"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
                        "data: [DONE]\n\n"
                    ))
                }
            }
        ))
    }).workers(1).listen(listener).unwrap().run();
    let handle = server.handle();
    actix_web::rt::spawn(server);
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    use desk_agent_protocol::ai_assistant::user_instructions::UserInstructionsUpdate;
    let first = crate::assistant_data_config::instructions(&db).await;
    let frozen = crate::assistant_data_config::update_instructions(
        &db,
        &UserInstructionsUpdate {
            expected_revision: first.revision,
            content: "ORIGINAL_FRESH_PERSONALIZATION".into(),
        },
    )
    .await
    .unwrap();
    let (store, task) =
        crate::schedule_store::tests::enabled_fresh_task_on(db.clone(), None, 1, None).await;
    crate::assistant_data_config::update_instructions(
        &db,
        &UserInstructionsUpdate {
            expected_revision: frozen.revision,
            content: "LATER_FRESH_PERSONALIZATION".into(),
        },
    )
    .await
    .unwrap();
    // The full orchestration prompt is larger than the storage fixture's request.
    let current = crate::schedule_budget_policy::read(&db).await.unwrap();
    let mut maximum = current.maximum.clone();
    maximum.max_model_tokens_per_run = model_budget;
    crate::schedule_budget_policy::update(
        &db,
        &desk_agent_protocol::schedule::policy::UpdateScheduleBudgetPolicy {
            expected_revision: current.revision,
            maximum,
        },
    )
    .await
    .unwrap();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("fresh-executor-test".into()),
            base_url: Some(format!("http://{address}/v1")),
            api_key: Some("synthetic-test-key".into()),
            max_context_bytes: Some(131_072),
            // Exercise both finite-quota cases independently of product defaults.
            runtime_max_output_tokens: 4096,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let queued = store
        .enqueue_manual(1, &task.schedule_id, "fresh-executor")
        .await
        .unwrap();
    let connection_id = uuid::Uuid::new_v4().to_string();
    let connections = actix_web::web::Data::new(SharedConnectionMap::new());
    connections.write().await.insert(
        connection_id.clone(),
        peer(&connection_id, &task.target_device_id).await,
    );
    let now = chrono::Utc::now();
    let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
    cache
        .update(
            &connection_id,
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 1,
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                server_api_version: 1,
                os: "fixture".into(),
                interactive_user_home: None,
                interactive_session_incarnation: "fresh-worker".into(),
                local_ceiling_revision: 1,
                unattended_command_execution: false,
                capabilities: vec![],
                context_references: vec![],
            },
            now,
        )
        .unwrap();
    let gate = Arc::new(AiAssistantGate::new(AiAssistantSettings {
        enabled: true,
        revision: 1,
    }));

    let input_seq_before_dispatch = crate::entity::agent_assistant_runtime::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .map_or(0, |row| row.input_seq);
    let executor =
        crate::schedule_executor::SignalScheduleExecutor::new(db.clone(), connections, gate);
    let second = executor.clone();
    let (a, b) = tokio::join!(
        actix_web::rt::spawn(async move { executor.scan_once(0).await }),
        actix_web::rt::spawn(async move { second.scan_once(0).await })
    );
    assert_eq!(a.unwrap().unwrap().settled + b.unwrap().unwrap().settled, 1);
    let finished = crate::entity::agent_schedule_run::Entity::find_by_id(queued.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let session = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(&queued.run_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let state =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&session.state_json)
            .unwrap();
    let data = state.assistant_data.as_ref().unwrap();

    assert_eq!(data.current_actor_input_seq, 0);
    assert_eq!(data.instructions.accepted_input_seq, 0);
    assert_eq!(data.instructions.content, "ORIGINAL_FRESH_PERSONALIZATION");
    assert!(data.history_access_scope.is_none() && data.memory_access_scope.is_none());
    let runtime = crate::entity::agent_assistant_runtime::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(runtime.input_seq, input_seq_before_dispatch);
    assert!(
        crate::entity::agent_run_event::Entity::find()
            .filter(
                crate::entity::agent_run_event::Column::Kind
                    .eq(desk_diagnose_core::dynamic_run::AgentRunEventKind::UserFollowup.as_str())
            )
            .all(&db)
            .await
            .unwrap()
            .is_empty()
    );
    if model_budget == 10_000 {
        assert_eq!(finished.status, "failed");
        let error = state.terminal_error.unwrap();
        assert_eq!(
            error.kind,
            desk_agent_protocol::AgentErrorKind::OutputLimitExceeded
        );
        assert_eq!(error.error_code, None);
        assert_eq!(
            desk_diagnose_core::subagent::reservation::CallRejection::from_error(&error),
            Some(
                desk_diagnose_core::subagent::reservation::CallRejection::Budget(
                    desk_diagnose_core::subagent::budget::BudgetDimension::ModelTokens
                )
            )
        );

        assert!(!error.retryable);
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert!(
            crate::entity::model_egress_receipt::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            agent_task_budget_reservation::Entity::find()
                .filter(agent_task_budget_reservation::Column::Kind.eq("model_tokens"))
                .all(&db)
                .await
                .unwrap()
                .is_empty()
        );
    } else {
        assert_eq!(
            finished.status,
            "succeeded",
            "error={:?}, terminal={:?}, model_calls={}",
            finished.error_kind,
            state.terminal_error,
            requests.load(Ordering::SeqCst)
        );
        assert_eq!(finished.attempt, 1);
        assert!(finished.result_ref.is_some());
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    cache.remove_connection(&connection_id);
    handle.stop(true).await;
}
