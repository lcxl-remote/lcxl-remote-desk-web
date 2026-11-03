//! A finished command continues the owner's requirement through the ordinary turn.
use super::*;
use actix_web::FromRequest;
use desk_agent_protocol::computer_use::*;
use desk_diagnose_core::session::PersistedAgentSession;
use desk_signal_facade::model::peer::{
    HostExecCapabilities, PeerInfo, ServerRegistration, SignalingPeer,
};
use desk_signal_facade::model::{
    auth_context::AuthContext, connection::ConnectionModel, signal::RemoteDeskTypeEnum,
    version::VersionInfo,
};
use sea_orm::{ColumnTrait, QueryFilter};
use std::sync::Arc;

const CONVERSATION: &str = "work-continuation";

async fn stored(db: &DatabaseConnection, conversation_id: &str) -> PersistedAgentSession {
    let row = crate::entity::agent_session::Entity::find()
        .filter(crate::entity::agent_session::Column::ConversationId.eq(conversation_id))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[actix_web::test]
async fn finished_command_continues_with_history_and_tools_then_drains_its_trigger() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("continuation.db").display()
    );
    let db = crate::config::test_support::Database::connect(database_url)
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("continuation-model".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only".into()),
            max_context_bytes: Some(131_072),
            runtime_max_output_tokens: 4096,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new());
    let (request, mut payload) = actix_web::test::TestRequest::get()
        .insert_header(("connection", "upgrade"))
        .insert_header(("upgrade", "websocket"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_parts();
    let (_response, socket, _stream) = actix_ws::handle(
        &request,
        web::Payload::from_request(&request, &mut payload)
            .await
            .unwrap(),
    )
    .unwrap();
    let host = format!("continuation-host-{}", uuid::Uuid::new_v4());
    connections.write().await.insert(host.clone(), {
        let (client_id, model) = (SignalingPeer {
            version_info: Some(VersionInfo {
                api_version: 1,
                build_number: 1,
                commit_hash: "fixture".into(),
                debug_build: cfg!(debug_assertions),
            }),
            peer: PeerInfo {
                remote_desk_type: RemoteDeskTypeEnum::Server,
                operation_system: Default::default(),
                display_name: None,
            },
            server: (Some("device".into())).map(|client_id| ServerRegistration {
                client_id,
                exec_capabilities: HostExecCapabilities {
                    available_exec_shells: Vec::new(),
                    exec_pty: false,
                    exec_pty_elevation: false,
                    max_ai_command_runtime_ms: None,
                },
                ai_assistant_settings: desk_agent_protocol::ai_assistant::AiAssistantSettings {
                    revision: 0,
                    enabled: false,
                },
            }),
        })
        .into_connection_model(host.clone(), None);
        ConnectionState {
            client_id,
            model: (ConnectionModel {
                device_id: None,
                owner_node_id: None,
                ..model
            })
            .into(),
            session: Arc::new(tokio::sync::RwLock::new(socket)),
            terminal_connection_ids: Default::default(),
            request_callback_map: Default::default(),
            device_code: None,
            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        }
    });
    let now = chrono::Utc::now();
    crate::computer_use_readiness::global_computer_use_readiness_cache()
        .update(
            &host,
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 1,
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                server_api_version: 1,
                os: "macos".into(),
                interactive_user_home: Some("/Users/fixture".into()),
                interactive_session_incarnation: "continuation-worker".into(),
                local_ceiling_revision: 1,
                capabilities: vec![],
                context_references: vec![],
            },
            now,
        )
        .unwrap();
    let sse = |text: &str| {
        format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"delta":{"content":text}}]})
        )
    };
    let (first_reply, second_reply) = (sse("The wait is running."), sse("All work is finished."));
    let capture = actix_web::rt::spawn(async move {
        let (first, _title) = capture_openai_answer_with_title(&listener, &first_reply).await;
        let second = capture_one_openai_request_with_sse(&listener, &second_reply).await;
        (first, second)
    });
    tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "transport-first".into(),
            "controller".into(),
            host.clone(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            "device".into(),
            AiAssistantAsk {
                client_timezone: "UTC".into(),
                question: "Wait for the command, then report".into(),
                client_message_id: "input-first".into(),
                conversation_id: Some(CONVERSATION.into()),
                ..Default::default()
            },
            None,
        ),
    )
    .await
    .unwrap();
    let conversation_id = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap()
        .conversation_id;
    let session = stored(&db, &conversation_id).await;
    let user =
        desk_diagnose_core::permission_resume::latest_user_requirement(&session.conversation)
            .unwrap();
    let envelope = desk_diagnose_core::model_message_labels::internal_tool_result_envelope(
        user.data_envelope.as_ref(),
        "signal-exec:task:done",
        "command-finished-marker",
        "exec_command",
    )
    .unwrap();
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(
            session.client_conversation_id.clone(),
            AgentSessionSurface::AiAssistant,
        )
        .deliver_work_completion_with_envelope(
            &conversation_id,
            1,
            desk_diagnose_core::session::WorkKind::AgentExec,
            "signal-exec:task:done",
            "generation",
            "call-wait",
            "task",
            "command-finished-marker",
            envelope,
            desk_diagnose_core::seam::ToolOutputFormat::Text,
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
    let session = stored(&db, &conversation_id).await;
    assert_eq!(session.pending_auto_triggers.len(), 1);
    let chain = session.chain_id.clone();

    let outcome = tokio::time::timeout(
        SCENARIO_TIMEOUT,
        resume_after_work_completion(connections.clone(), db.clone(), host, session),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        outcome,
        LoopOutcome::Answered("All work is finished.".into())
    );
    let (_first, second) = tokio::time::timeout(SCENARIO_TIMEOUT, capture)
        .await
        .unwrap()
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&second).unwrap();
    assert!(
        body["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty()),
        "the continuation must advertise the ordinary tools"
    );
    let messages = body["messages"].to_string();
    assert!(messages.contains("The wait is running."), "{messages}");
    assert!(messages.contains("command-finished-marker"), "{messages}");
    assert!(messages.contains("grants no new permission"), "{messages}");
    assert!(!messages.contains("No tools are available"), "{messages}");

    let after = stored(&db, &conversation_id).await;
    assert_eq!(
        after.trigger_origin,
        desk_diagnose_core::session::TriggerOrigin::WorkContinuation
    );
    assert!(after.pending_auto_triggers.is_empty());
    assert_eq!(after.automation_turns_used, 1);
    assert_eq!(after.chain_id, chain);
    assert_eq!(
        desk_diagnose_core::permission_resume::latest_user_requirement(&after.conversation)
            .unwrap()
            .message_id,
        "input-first"
    );
}
