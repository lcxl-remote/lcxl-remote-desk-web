//! Exercise task admission through the real audited seam and a loopback gateway.
use super::*;
use crate::assistant_model::{
    FreshTaskModelContext, MeteredModel, ModelExportSource, model_export_id,
};
use desk_diagnose_core::{
    model_message_labels::model_bound_user_message,
    prompt::ResponseFormatSpec,
    seam::{ModelRequest, ModelSeam, NullTurnSink},
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[actix_web::test]
async fn fresh_task_gateway_accounts_usage_and_rejects_invalid_authority() {
    for with_usage in [true, false] {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let count = Arc::new(AtomicU64::new(0));
        let captured = count.clone();
        let server = actix_web::HttpServer::new(move || {
            let count = captured.clone();
            actix_web::App::new().route("/v1/chat/completions", actix_web::web::post().to(
                move |_: actix_web::web::Json<serde_json::Value>| {
                    count.fetch_add(1, Ordering::Relaxed);
                    async move {
                        let delta = serde_json::json!({"choices":[{"delta":{"content":"task answer"},"finish_reason":"stop"}]});
                        let usage = if with_usage { "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n" } else { "" };
                        actix_web::HttpResponse::Ok().content_type("text/event-stream")
                            .body(format!("data: {delta}\n\n{usage}data: [DONE]\n\n"))
                    }
                }))
        }).workers(1).listen(listener).unwrap().run();
        let handle = server.handle();
        actix_web::rt::spawn(server);
        let db = Database::connect("sqlite::memory:").await.unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let (store, task, _, publication) = fixture_on(db.clone()).await;
        let config = crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("task-test".into()),
            base_url: Some(format!("http://{address}/v1")),
            api_key: Some("local-fixture-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        };
        crate::model_provider::save(&db, config).await.unwrap();
        let config = crate::model_provider::load(&db).await.unwrap();
        store
            .publish_task(1, &publication, &Verifier(true))
            .await
            .unwrap();
        let run = store
            .enqueue_manual(1, &task.schedule_id, "task-model-gateway")
            .await
            .unwrap();
        let connections = actix_web::web::Data::new(SharedConnectionMap::new());
        connections.write().await.insert(
            "target".into(),
            peer("target", &task.target_device_id).await,
        );
        let settings = DeviceAssistantSettings {
            revision: 1,
            enabled: true,
        };
        let gate = Arc::new(DeviceAssistantGate::new(settings));
        let claimed = store
            .claim_fresh_task(
                &connections,
                &gate,
                FreshTaskClaim {
                    owner: 1,
                    run_id: &run.run_id,
                    node_id: "task-node",
                    lease_seconds: 90,
                    policy_revision: 1,
                    scope: AgentScope {
                        granted: vec![],
                        mode: ExecutionMode::SuggestOnly,
                        expires_at: None,
                        policy_name: None,
                    },
                },
            )
            .await
            .unwrap();
        let held = &claimed.session;
        let destination = config.destination_identity().unwrap();
        let mut model = MeteredModel {
            inner: crate::model_dial::SignalModelSeam::from_config(&config)
                .unwrap()
                .with_context_db(db.clone()),
            db: db.clone(),
            model_name: "task-test".into(),
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: model_export_id(
                "1",
                &task.target_device_id,
                &run.run_id,
                ModelExportSource::Turn(held.current_turn_id.as_deref().unwrap()),
            ),
            permission_resume: false,
            completed_compression_receipt: std::cell::RefCell::new(None),
            model_call_ordinal: AtomicU64::new(0),
            fresh_task: Some(FreshTaskModelContext {
                run_id: run.run_id.clone(),
                device_id: task.target_device_id.clone(),
                node_id: "task-node".into(),
                run_epoch: 1,
                session_token: held.lease_token,
                target_connection_id: claimed.target_connection_id,
                connections: connections.clone(),
                gate: gate.clone(),
            }),
        };
        let request = |text: &str| {
            let message =
                model_bound_user_message("task-input".into(), text.into(), destination.clone())
                    .unwrap()
                    .with_turn_id(held.current_turn_id.clone().unwrap());
            ModelRequest::text_only(vec![message], ResponseFormatSpec::None)
        };
        let answer = model
            .call(request(&task.prompt), &mut NullTurnSink)
            .await
            .unwrap();
        assert_eq!(answer.text, "task answer");
        assert_eq!(count.load(Ordering::Relaxed), 1);
        let budget = agent_task_budget_reservation::Entity::find()
            .all(&db)
            .await
            .unwrap();
        let charge = budget
            .iter()
            .find(|row| row.kind == "model_tokens")
            .unwrap();
        if with_usage {
            assert_eq!(charge.state, "settled");
            assert_eq!(charge.charged_units, 7);
        } else {
            assert_eq!(charge.state, "reserved");
            assert_eq!(charge.charged_units, charge.reserved_units);
        }
        model.model_call_ordinal.store(0, Ordering::Relaxed);
        assert!(
            model
                .call(request(&task.prompt), &mut NullTurnSink)
                .await
                .is_err()
        );
        let large = request(&"x".repeat(20_000));
        assert!(
            model
                .model_egress_policy()
                .unwrap()
                .unwrap()
                .authorize_request(large.clone())
                .is_ok()
        );
        assert!(model.call(large, &mut NullTurnSink).await.is_err());
        model.fresh_task.as_mut().unwrap().session_token += 1;
        assert!(
            model
                .call(request(&task.prompt), &mut NullTurnSink)
                .await
                .is_err()
        );
        model.fresh_task.as_mut().unwrap().session_token = held.lease_token;
        gate.replace(DeviceAssistantSettings {
            revision: 2,
            enabled: false,
        });
        assert!(
            model
                .call(request(&task.prompt), &mut NullTurnSink)
                .await
                .is_err()
        );
        gate.replace(settings);
        connections.write().await.clear();
        assert!(
            model
                .call(request(&task.prompt), &mut NullTurnSink)
                .await
                .is_err()
        );
        connections.write().await.insert(
            "target".into(),
            peer("target", &task.target_device_id).await,
        );
        let authorization = crate::entity::agent_task_authorization::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        store
            .revoke_task_authorization(1, &authorization.authorization_id, "owner revoked")
            .await
            .unwrap();
        assert!(
            model
                .call(request(&task.prompt), &mut NullTurnSink)
                .await
                .is_err()
        );
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(
            agent_task_budget_reservation::Entity::find()
                .all(&db)
                .await
                .unwrap(),
            budget
        );
        assert_eq!(
            crate::entity::model_egress_receipt::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .len(),
            1
        );
        handle.stop(true).await;
    }
}
