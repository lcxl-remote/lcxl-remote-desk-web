//! A real ready worker must not prevent visible answers from being searchable.
use super::*;
use actix_web::FromRequest;
use desk_agent_protocol::{
    ai_assistant::{
        owner_history::AssistantHistorySearchRequest,
        user_instructions::AssistantDataSettingsUpdate,
    },
    computer_use::*,
};
use desk_signal_facade::model::peer::{
    HostExecCapabilities, PeerInfo, ServerRegistration, SignalingPeer,
};
use desk_signal_facade::model::{
    auth_context::AuthContext, connection::ConnectionModel, signal::RemoteDeskTypeEnum,
    version::VersionInfo,
};
use sea_orm::{ColumnTrait, QueryFilter};
use std::sync::Arc;

mod provider {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../signal-facade/tests/fixtures/assistant_reply_gateway.rs"
    ));
}

#[actix_web::test]
async fn worker_home_answer_is_indexed_and_found_by_owner_and_other_conversation_tools() {
    scenario(None).await;
}

#[actix_web::test]
async fn worker_home_reply_and_history_tools_fit_small_stacks() {
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        scenario(Some(bytes)).await;
    }
}

async fn scenario(stack_bytes: Option<usize>) {
    // File backing preserves state when a small-stack runtime retires a pool connection.
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("reply.db").display()
    );
    let db = crate::config::test_support::Database::connect(database_url)
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let gateway = provider::start().await;
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("reply-model".into()),
            base_url: Some(gateway.url.clone()),
            api_key: Some("test-only".into()),
            max_context_bytes: Some(131_072),
            runtime_max_output_tokens: 4096,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let settings = crate::assistant_data_config::settings(&db).await;
    crate::assistant_data_config::update_settings(
        &db,
        &AssistantDataSettingsUpdate {
            expected_revision: settings.revision,
            automatic_memory_enabled: false,
            model_history_search_enabled: true,
            explanation_version: settings.explanation_version,
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
    let host = format!("reply-host-{}", uuid::Uuid::new_v4());
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
                interactive_session_incarnation: "reply-worker".into(),
                local_ceiling_revision: 1,
                capabilities: vec![],
                context_references: vec![],
            },
            now,
        )
        .unwrap();
    for (conversation, input, question) in [
        ("answer-source", "first", "Explain the result"),
        (
            "answer-search",
            "search",
            "Find the answer from my previous conversation",
        ),
        ("answer-source", "followup", "Explain the next step"),
    ] {
        let (run_connections, run_db, run_host) = (connections.clone(), db.clone(), host.clone());
        provider::run_on_stack(stack_bytes, move || async move {
            tokio::time::timeout(
                SCENARIO_TIMEOUT,
                run_turn_inner(
                    run_connections,
                    run_db,
                    format!("transport-{input}"),
                    "controller".into(),
                    run_host,
                    crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                    "device".into(),
                    AiAssistantAsk {
                        client_timezone: "UTC".into(),
                        question: question.into(),
                        client_message_id: format!("input-{input}"),
                        conversation_id: Some(conversation.into()),
                        ..Default::default()
                    },
                    None,
                ),
            )
            .await
            .unwrap();
        })
        .await;
        if input == "first" {
            let query: AssistantHistorySearchRequest =
                serde_json::from_value(serde_json::json!({"queries":["Cobaltarchive"]})).unwrap();
            let found = crate::assistant_data_store::owner_history_search(
                &db,
                &query,
                chrono::Utc::now().timestamp_millis() as u64,
            )
            .await
            .unwrap();
            let archives = crate::entity::agent_assistant_model_output::Entity::find()
                .all(&db)
                .await
                .unwrap();
            let mut metadata = Vec::with_capacity(archives.len());
            for row in archives {
                let proof = crate::assistant_data_store::output_archive::decode_row_on(&db, row)
                    .await
                    .unwrap()
                    .proof;
                let inputs = proof
                    .sources
                    .input_snapshot
                    .policy_sources()
                    .into_iter()
                    .zip(&proof.input_policies.leaves)
                    .map(|(source, leaf)| {
                        (
                            source.provenance.source_provider_id.clone(),
                            source.provenance.source_tool_name.clone(),
                            leaf.as_ref().map(|value| value.contract),
                        )
                    })
                    .collect::<Vec<_>>();
                metadata.push((proof.retained_policy.is_some(), inputs));
            }
            assert_eq!(
                found.fragments.len(),
                1,
                "the actual answer must enter the owner search index: {metadata:?}"
            );
            assert_eq!(
                found.fragments[0].source.content.as_deref(),
                Some(provider::ANSWER)
            );
            *gateway.source_id.lock().unwrap() = found.fragments[0].source.source_id.clone();
        }
    }
    let rows = crate::entity::agent_assistant_history::Entity::find()
        .filter(crate::entity::agent_assistant_history::Column::Kind.eq("assistant_message"))
        .all(&db)
        .await
        .unwrap();
    assert!(rows.iter().any(|row| row.content == provider::ANSWER));
    let followup: AssistantHistorySearchRequest =
        serde_json::from_value(serde_json::json!({"queries":["Jadefollowup"]})).unwrap();
    let found = crate::assistant_data_store::owner_history_search(
        &db,
        &followup,
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap();
    assert_eq!(found.fragments.len(), 1);
    assert_eq!(
        found.fragments[0].source.content.as_deref(),
        Some(provider::FOLLOWUP)
    );
    let policy = crate::config::ConfigConnection::config_read(&db)
        .await
        .assistant_data_policy
        .clone();
    assert!(rows.iter().all(|row| row.expires_at_ms - row.recorded_at_ms
        == i64::from(policy.history_retention_days) * 86_400_000));
    let calls = gateway.requests.lock().unwrap().len();
    assert_eq!(
        calls, 7,
        "three answers, two title calls and the search/read tool roundtrips"
    );
    let session =
        crate::entity::agent_session::Entity::find()
            .filter(crate::entity::agent_session::Column::ConversationId.eq(
                derive_conversation_key("1", "device", Some("answer-search"), "unused"),
            ))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
    let saved =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&session.state_json)
            .unwrap();
    assert!(
        saved
            .conversation
            .iter()
            .any(|message| message.text == "Found the previous reply.")
    );
    gateway.stop().await;
}
