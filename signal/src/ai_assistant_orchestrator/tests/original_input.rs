//! Actual OSS entry, SQLite and synthetic HTTP model; no device UI is exercised.

use super::*;
use crate::agent_run_event_store::{InputSubject, SignalAgentRunEventStore};
use desk_agent_protocol::ai_assistant::AiAssistantObjectContextOperation;
use desk_agent_protocol::computer_use::{ObjectKind, ObjectRef};

#[actix_web::test]
async fn targetless_owner_turn_uses_audited_provider_and_keeps_its_answer() {
    personal_input_roundtrip(Vec::new()).await;
}

#[actix_web::test]
async fn personal_input_with_selected_targets_runs_under_the_same_account() {
    personal_input_roundtrip(vec!["selected-mac".into(), "selected-linux".into()]).await;
}

#[actix_web::test]
async fn queued_owner_input_runs_after_reopen_without_the_submitting_connection() {
    use sea_orm::{ColumnTrait, QueryFilter};
    crate::ai_assistant_gate::enable_test_host();
    let listener = std::sync::Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("queued.db").display()
    );
    let connect = || crate::config::test_support::Database::connect(&url);
    let db = connect().await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("queue-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let ask = AiAssistantAsk {
        question: "Resume the queued work".into(),
        client_message_id: "durable-queued-message".into(),
        conversation_id: Some("durable-queued-conversation".into()),
        client_timezone: "UTC".into(),
        ..Default::default()
    };
    submit_owner_turn(
        web::Data::new(SharedConnectionMap::new_oss()),
        db.clone(),
        "submission-frame".into(),
        "closed-browser".into(),
        String::new(),
        owner,
        String::new(),
        ask.clone(),
    )
    .await;
    assert!(
        crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
    let [row]: [_; 1] = main_request::recovery_heads(&db, 0, 2)
        .await
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(row.client_message_id, ask.client_message_id);
    drop(db);

    let db = connect().await.unwrap();
    let gate = std::sync::Arc::new(crate::ai_assistant_gate::AiAssistantGate::new(
        desk_agent_protocol::ai_assistant::AiAssistantSettings {
            revision: 1,
            enabled: true,
        },
    ));
    let executor = main_requests::SignalMainRequestExecutor::new(
        db.clone(),
        web::Data::new(SharedConnectionMap::new_oss()),
        gate,
    );
    let answer_gateway = listener.clone();
    let answer = actix_web::rt::spawn(async move {
        let sse = memory_sse(serde_json::json!({"content":"Resumed by the host"}), "stop");
        capture_openai_answer_with_title(answer_gateway.as_ref(), &sse).await
    });
    tokio::time::timeout(SCENARIO_TIMEOUT, executor.scan_once(0))
        .await
        .unwrap()
        .unwrap();
    let (body, _) = tokio::time::timeout(MODEL_IO_TIMEOUT, answer)
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(body).unwrap().contains(&ask.question));
    let snapshot = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&row.conversation_id, &row.actor_id, "")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.input_revision, 1);
    assert_eq!(
        latest_committed_answer(&snapshot).as_deref(),
        Some("Resumed by the host")
    );
    assert!(
        main_request::recovery_heads(&db, 0, 2)
            .await
            .unwrap()
            .is_empty()
    );
    let native_state = || async {
        let row = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        desk_diagnose_core::session::PersistedAgentSession::decode_boxed_json(&row.state_json)
            .unwrap()
    };
    let before = native_state().await;
    for (id, question) in [
        ("queued-b", "Coordinate request B"),
        ("queued-c", "Coordinate request C"),
    ] {
        submit_owner_turn(
            web::Data::new(SharedConnectionMap::new_oss()),
            db.clone(),
            id.to_owned(),
            "closed-browser".into(),
            String::new(),
            owner,
            String::new(),
            AiAssistantAsk {
                question: question.into(),
                client_message_id: id.into(),
                ..ask.clone()
            },
        )
        .await;
    }
    let b = main_request::first_pending(&db, &row.actor_id, &row.conversation_id)
        .await
        .unwrap()
        .unwrap();
    let c = crate::entity::agent_main_request::Entity::find()
        .filter(crate::entity::agent_main_request::Column::ClientMessageId.eq("queued-c"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let reply = serde_json::json!({"decisions":[
        {"request_id":b.request_id,"route":"native"},
        {"request_id":c.request_id,"route":"native"},
    ]})
    .to_string();
    let batch = actix_web::rt::spawn({
        let listener = listener.clone();
        async move {
            capture_one_openai_request_with_reply(listener.as_ref(), |_| {
                memory_sse(serde_json::json!({"content":reply}), "stop")
            })
            .await
        }
    });
    tokio::time::timeout(SCENARIO_TIMEOUT, executor.scan_once(0))
        .await
        .unwrap()
        .unwrap();
    let captured = tokio::time::timeout(MODEL_IO_TIMEOUT, batch)
        .await
        .unwrap()
        .unwrap();
    let batch_body: serde_json::Value = serde_json::from_slice(&captured).unwrap();
    let projected = batch_body["messages"].to_string();
    assert!(projected.contains("Coordinate request B"));
    assert!(projected.contains("Coordinate request C"));
    assert!(!projected.contains(&ask.question));
    assert!(batch_body["tools"].as_array().is_none_or(Vec::is_empty));
    let decided = native_state().await;
    assert_eq!(decided.input_revision, before.input_revision);
    assert_eq!(decided.focus_epoch, before.focus_epoch);
    assert_eq!(decided.conversation, before.conversation);
    assert!(decided.main_coordination.is_none());
    for pending in [&b, &c] {
        assert!(
            main_request::load(
                &db,
                &row.actor_id,
                &row.conversation_id,
                &pending.request_id
            )
            .await
            .unwrap()
            .unwrap()
            .coordinated
        );
    }
    let gateway = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_reply(listener.as_ref(), |_| {
            memory_sse(serde_json::json!({"content":"Request B handled"}), "stop")
        })
        .await
    });
    tokio::time::timeout(SCENARIO_TIMEOUT, executor.scan_once(0))
        .await
        .unwrap()
        .unwrap();
    let native = tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
        .await
        .unwrap()
        .unwrap();
    let native = String::from_utf8(native).unwrap();
    assert!(native.contains("Coordinate request B"));
    assert!(!native.contains("Coordinate request C"));
    assert_eq!(native_state().await.input_revision, 2);
    assert_eq!(
        main_request::recovery_heads(&db, 0, 2).await.unwrap()[0].request_id,
        c.request_id
    );
    let mut changed_model = crate::model_provider::load(&db).await.unwrap();
    changed_model.model = Some("different-destination".into());
    crate::model_provider::save(&db, changed_model)
        .await
        .unwrap();
    executor.scan_once(0).await.unwrap();
    let receipt = main_request::receipts(
        &db,
        &row.actor_id,
        &row.conversation_id,
        Some(&c.request_id),
    )
    .await
    .unwrap()
    .pop()
    .unwrap();
    assert_eq!(
        receipt.state,
        desk_agent_protocol::ai_assistant::MainRequestState::Failed
    );
    assert_eq!(
        receipt.failure,
        Some(desk_agent_protocol::ai_assistant::MainRequestFailure::DestinationChanged)
    );
    assert_eq!(native_state().await.input_revision, 2);
}

async fn personal_input_roundtrip(target_refs: Vec<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    for reference in &target_refs {
        crate::device_code_store::get_or_create(&db, reference)
            .await
            .unwrap();
    }
    if !target_refs.is_empty() {
        use desk_signal_facade::model::os::OperationSystemEnum;
        for (id, name, platform) in [
            ("selected-mac", "Mac for testing", OperationSystemEnum::Mac),
            (
                "selected-linux",
                "Linux for regression",
                OperationSystemEnum::Linux,
            ),
            (
                "unselected-windows",
                "Other device",
                OperationSystemEnum::Windows,
            ),
        ] {
            register_request_device(&connections, id, name, platform).await;
        }
    }
    crate::ai_assistant_gate::enable_test_host();
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("targetless-fixture".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let gateway = actix_web::rt::spawn(async move {
        let first_answer = memory_sse(
            serde_json::json!({"content":"first targetless answer"}),
            "stop",
        );
        let (first, _) = capture_openai_answer_with_title(&listener, &first_answer).await;
        let second = capture_one_openai_request_with_reply(&listener, |_| {
            memory_sse(
                serde_json::json!({"content":"second targetless answer"}),
                "stop",
            )
        })
        .await;
        [first, second]
    });
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let ask = AiAssistantAsk {
        target_refs: target_refs.clone(),
        question: "Help me draft a test case".into(),
        client_message_id: "targetless-owner-input".into(),
        conversation_id: Some("targetless-owner-conversation".into()),
        client_timezone: "UTC".into(),
        ..Default::default()
    };
    let followup = AiAssistantAsk {
        question: "Now give me the expected result".into(),
        client_message_id: "targetless-owner-followup".into(),
        ..ask.clone()
    };
    for (index, input) in [ask.clone(), followup.clone(), followup]
        .into_iter()
        .enumerate()
    {
        tokio::time::timeout(
            SCENARIO_TIMEOUT,
            run_turn(
                connections.clone(),
                db.clone(),
                format!("targetless-owner-request-{index}"),
                "owner-browser".into(),
                String::new(),
                owner,
                String::new(),
                input,
            ),
        )
        .await
        .unwrap();
    }
    let [first, second] = tokio::time::timeout(MODEL_IO_TIMEOUT, gateway)
        .await
        .unwrap()
        .unwrap();
    for (wire, role, content) in [
        (first, "user", ask.question.as_str()),
        (second, "assistant", "first targetless answer"),
    ] {
        let request: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        assert!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == role && message["content"] == content),
            "missing {role} message {content:?} in {:?}",
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] != "system")
                .collect::<Vec<_>>(),
        );
        assert_targetless_web_tools(&request, false);
        if !target_refs.is_empty() {
            let catalogue = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_str())
                .find(|text| text.contains("\"request_devices\""))
                .expect("selected device catalogue");
            assert!(catalogue.contains("\"target_ref\":\"selected-mac\""));
            assert!(catalogue.contains("\"platform\":\"macOS\""));
            assert!(catalogue.contains("\"name\":\"Mac for testing\""));
            assert!(catalogue.contains("\"target_ref\":\"selected-linux\""));
            assert!(catalogue.contains("\"platform\":\"Linux\""));
            assert!(!catalogue.contains("unselected-windows"));
        }
    }

    let actor = owner.to_string();
    let run = derive_assistant_conversation_key(
        &actor,
        AssistantConversationScope::Personal,
        ask.conversation_id.as_deref().unwrap(),
    );
    let session = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        latest_committed_answer(&session).as_deref(),
        Some("second targetless answer")
    );
    assert_eq!(session.input_revision, 2);
    let stored = crate::entity::agent_session::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let stored =
        desk_diagnose_core::session::PersistedAgentSession::decode_json(&stored.state_json)
            .unwrap();
    assert!(stored.device_id.is_empty());
    assert_eq!(
        stored.assistant_request_devices(),
        Some(target_refs.as_slice())
    );
    let receipts = crate::entity::model_egress_receipt::Entity::find()
        .all(&db)
        .await
        .unwrap();
    assert_eq!(receipts.len(), 3);
    assert!(
        receipts
            .iter()
            .all(|receipt| receipt.state == crate::model_egress_store::STATE_SUCCEEDED)
    );
}

async fn register_request_device(
    connections: &SharedConnectionMap,
    id: &str,
    name: &str,
    platform: desk_signal_facade::model::os::OperationSystemEnum,
) -> actix_web::HttpResponse {
    use desk_signal_facade::model::{
        auth_context::AuthContext, connection::ConnectionModel, peer::PeerInfo,
        signal::RemoteDeskTypeEnum,
    };
    use std::sync::Arc;
    let request = actix_web::test::TestRequest::get()
        .insert_header(("upgrade", "websocket"))
        .insert_header(("connection", "upgrade"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let payload = <web::Payload as actix_web::FromRequest>::from_request(
        &request,
        &mut actix_web::dev::Payload::None,
    )
    .await
    .unwrap();
    let (response, socket, _) = actix_ws::handle(&request, payload).unwrap();
    let connection_id = format!("live-{id}");
    connections.write().await.insert(
        connection_id.clone(),
        ConnectionState {
            lifecycle: Default::default(),
            model: Arc::new(ConnectionModel {
                connection_id,
                ip: None,
                version_info: None,
                peer: PeerInfo {
                    remote_desk_type: RemoteDeskTypeEnum::Server,
                    operation_system: platform,
                    display_name: Some(name.into()),
                },
                exec_capabilities: desk_signal_facade::model::peer::HostExecCapabilities {
                    available_exec_shells: vec!["bash".into()],
                    ..Default::default()
                },
                ai_assistant_settings: Some(
                    desk_agent_protocol::ai_assistant::AiAssistantSettings {
                        revision: 1,
                        enabled: true,
                    },
                ),
                device_id: Some(id.into()),
                owner_node_id: None,
            }),
            client_id: Some(id.into()),
            session: Arc::new(tokio::sync::RwLock::new(socket)),
            terminal_connection_ids: Default::default(),
            request_callback_map: Default::default(),
            device_code: None,
            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        },
    );
    response
}

#[actix_web::test]
async fn personal_input_seals_command_only_for_the_selected_device() {
    run_selected_command(SelectedCommandFinish::Foreground).await;
}

#[actix_web::test]
async fn personal_input_seals_command_and_continues_after_background_result() {
    run_selected_command(SelectedCommandFinish::Background).await;
}

#[actix_web::test]
async fn personal_input_seals_command_but_retires_a_late_receipt_after_supersession() {
    run_selected_command(SelectedCommandFinish::Retired).await;
}

enum SelectedCommandFinish {
    Foreground,
    Background,
    Retired,
}

async fn run_selected_command(finish: SelectedCommandFinish) {
    let background = !matches!(finish, SelectedCommandFinish::Foreground);
    let retired = matches!(finish, SelectedCommandFinish::Retired);
    static DEVICE_FIXTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _fixture = DEVICE_FIXTURE.lock().await;
    use crate::controller::ai_assistant_session::decide_permission_on;
    use desk_agent_protocol::{Capability, computer_use::*};
    use desk_assistant_data_store::main_request::{OwnerAdmission, OwnerAdmissionResult};
    use desk_signal_facade::controller::ai_assistant_session::{
        PermissionDecisionBody, PermissionDecisionItemBody, PermissionItemDecisionBody,
    };
    use desk_signal_facade::model::os::OperationSystemEnum;
    use serde_json::{Value, json};

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
            model: Some("selected-command-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    if background {
        crate::ai_assistant_gate::enable_test_host();
        crate::agent_runtime::register_continuation_connections(connections.clone());
    }
    let now = chrono::Utc::now();
    let mut peers = Vec::new();
    for id in ["command-selected", "command-outside"] {
        crate::device_code_store::get_or_create(&db, id)
            .await
            .unwrap();
        peers.push(register_request_device(&connections, id, id, OperationSystemEnum::Linux).await);
        let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
        cache.remove_connection(&format!("live-{id}"));
        cache
            .update(
                &format!("live-{id}"),
                ComputerUseReadiness {
                    schema_version: COMPUTER_USE_SCHEMA_VERSION,
                    revision: 1,
                    observed_at: now.to_rfc3339(),
                    expires_at: (now + chrono::Duration::minutes(2)).to_rfc3339(),
                    server_api_version: 1,
                    os: "linux".into(),
                    interactive_user_home: None,
                    interactive_session_incarnation: "desktop:worker".into(),
                    local_ceiling_revision: 1,
                    capabilities: vec![ComputerUseCapabilityReadiness {
                        capability: Capability::ShellExecConfirmed,
                        adapter: ComputerUseAdapterRef {
                            kind: ComputerUseAdapterKind::Terminal,
                            version: "1".into(),
                        },
                        supported: true,
                        ready: true,
                        reason: None,
                    }],
                    context_references: vec![],
                },
                now,
            )
            .unwrap();
    }
    let model = actix_web::rt::spawn(async move {
        let reply = |target: &str| {
            memory_sse(
                json!({"tool_calls":[{"index":0,"id":format!("plan-{target}"),"type":"function","function":{
                    "name":"request_permissions","arguments":json!({
                    "target_ref":target,"items":[{"tool_name":"exec_command",
                            "exact_input":{"shell":"bash","command":"df -h","timeout_ms":1000},
                            "reason":"Check disk space on the selected machine"}]
                    }).to_string()
                }}]}),
                "tool_calls",
            )
        };
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let spec = request["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["function"]["name"] == "request_permissions")
                .unwrap();
            assert!(spec["function"]["parameters"]["properties"]["target_ref"].is_object());
            assert!(request["messages"].to_string().contains("command-selected"));
            assert!(!request["messages"].to_string().contains("command-outside"));
            reply("command-outside")
        })
        .await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool"
                        && message["tool_call_id"] == "plan-command-outside"
                        && message["content"].as_str().is_some_and(
                            |value| value.contains("Selected assistant device is unavailable")
                        )),
                "{request}"
            );
            reply("command-selected")
        })
        .await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let spec = request["tools"].as_array().unwrap().iter()
                .find(|tool| tool["function"]["name"] == "exec_command")
                .unwrap_or_else(|| panic!("approved command is missing: {request}"));
            assert!(spec["function"]["parameters"]["required"].as_array().unwrap().contains(&json!("target_ref")));
            memory_sse(json!({"tool_calls":[{"index":0,"id":"execute-selected","type":"function","function":{
                "name":"exec_command","arguments":json!({"target_ref":"command-selected",
                    "shell":"bash","command":"df -h","timeout_ms":1000}).to_string()
            }}]}), "tool_calls")
        }).await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            if background {
                assert!(
                    !request["messages"]
                        .to_string()
                        .contains("SELECTED_DISK_RECEIPT")
                );
                return memory_sse(json!({"content":"Waiting for the disk report."}), "stop");
            }
            assert!(
                request["messages"]
                    .to_string()
                    .contains("SELECTED_DISK_RECEIPT"),
                "{request}"
            );
            memory_sse(json!({"content":"Received the disk report."}), "stop")
        })
        .await;
        if background && !retired {
            let mut received_result = false;
            while !received_result {
                capture_one_openai_request_with_reply(&listener, |body| {
                    let request: Value = serde_json::from_slice(body).unwrap();
                    let messages = request["messages"].to_string();
                    if messages.contains("Create a short conversation title") {
                        return memory_sse(
                            json!({"content":"{\"title\":\"Check Disk Space\"}"}),
                            "stop",
                        );
                    }
                    assert!(
                        messages.contains("SELECTED_DISK_RECEIPT"),
                        "{}",
                        request["messages"].as_array().unwrap().last().unwrap()
                    );
                    received_result = true;
                    memory_sse(json!({"content":"Received the disk report."}), "stop")
                })
                .await;
            }
        }
    });
    let intent = "selected-command-conversation";
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let roster = crate::controller::ai_assistant_session::update_roster(
        &db,
        &crate::controller::ai_assistant_session::AiAssistantDeviceRosterBody {
            conversation: intent.into(),
            expected_revision: 0,
            target_refs: vec!["command-selected".into()],
        },
    )
    .await
    .unwrap();
    assert_eq!(roster.revision, 1);
    tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_turn(
            connections.clone(),
            db.clone(),
            "selected-command-transport".into(),
            "owner-browser".into(),
            String::new(),
            owner,
            String::new(),
            AiAssistantAsk {
                question: "Check disk space on this device".into(),
                client_message_id: "selected-command-input".into(),
                conversation_id: Some(intent.into()),
                client_timezone: "UTC".into(),
                target_refs: vec!["command-selected".into()],
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap();
    let actor = owner.to_string();
    let run =
        derive_assistant_conversation_key(&actor, AssistantConversationScope::Personal, intent);
    let snapshot = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .read_snapshot_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .unwrap();
    let [permission] = snapshot.permission_requests.as_slice() else {
        panic!(
            "expected one selected-device approval, got {:?}: {:?}",
            snapshot.permission_requests,
            snapshot.messages.iter().rev().take(3).collect::<Vec<_>>()
        );
    };
    let confirmation = permission.exact_command().unwrap();
    assert_eq!(confirmation.target_device_id, "command-selected");
    assert_eq!(
        confirmation.target_session_id,
        "live-command-selected:desktop:worker"
    );
    assert_eq!(confirmation.proposal.command, "df -h");
    assert!(
        crate::entity::agent_action_item::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .is_empty()
    );
    let item = &permission.items[0];
    let decision = decide_permission_on(
        &db,
        connections.clone(),
        web::Json(PermissionDecisionBody {
            connection: None,
            conversation: None,
            session: Some(run.clone()),
            expected_run_request_id: None,
            request_id: permission.request_id.clone(),
            carrier_id: None,
            items: vec![PermissionDecisionItemBody {
                item_id: item.item_id.clone(),
                decision: PermissionItemDecisionBody::Approve {
                    resource_scope: item.resource_scope.clone(),
                    operation_scope: item.operation_scope.clone(),
                    export_destinations: vec![],
                    ttl_seconds: 60,
                    max_uses: 1,
                },
            }],
        }),
    )
    .await
    .unwrap();
    let decision = actix_web::body::to_bytes(decision.into_body())
        .await
        .unwrap();
    let decision: Value = serde_json::from_slice(&decision).unwrap();
    assert_eq!(decision["data"]["state"], "approved", "{decision}");
    let queued = if background {
        let ask = AiAssistantAsk {
            question: "Check the next request".into(),
            client_message_id: "queued-after-command".into(),
            conversation_id: Some(intent.into()),
            client_timezone: "UTC".into(),
            ..Default::default()
        };
        let scope = desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        };
        let destination = crate::model_provider::load(&db)
            .await
            .unwrap()
            .destination_identity()
            .unwrap();
        let next = crate::agent_run_event_store::AppendUserFollowupParams {
            client_timezone: ask.client_timezone.clone(),
            temporary_conversation: None,
            event_id: ask.client_message_id.clone(),
            run_id: run.clone(),
            client_conversation_id: ask.conversation_id.clone(),
            actor_id: actor.clone(),
            device_id: String::new(),
            target_refs: vec![],
            surface: AgentSessionSurface::AiAssistant,
            policy_revision:
                desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
            read_context: Some(
                crate::agent_run_event_store::ReadContextSelection::capture(&[], &scope).unwrap(),
            ),
            current_scope: scope,
            message: model_bound_user_message(
                ask.client_message_id.clone(),
                ask.question.clone(),
                destination,
            )
            .unwrap(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let request_id = if retired {
            None
        } else {
            let OwnerAdmissionResult::Queued(receipt) = SignalAgentRunEventStore::new(db.clone())
                .admit_user_input(
                    &next,
                    None,
                    OwnerAdmission::Enqueue {
                        ask: &ask,
                        limit: 4,
                        reply_connection_id: None,
                    },
                )
                .await
                .unwrap()
            else {
                panic!("expected a queued request")
            };
            Some(receipt.request_id.clone())
        };
        Some((next, request_id))
    } else {
        None
    };
    tokio::time::timeout(SCENARIO_TIMEOUT, async {
        tokio::join!(
            async {
                respond_to_selected_command(&connections, db.clone(), peers.remove(0), background)
                    .await;
                if let Some((next, request_id)) = &queued {
                    let events = SignalAgentRunEventStore::new(db.clone());
                    if let Some(request_id) = request_id {
                        assert!(matches!(
                            events
                                .admit_user_input(
                                    next,
                                    None,
                                    OwnerAdmission::Activate { request_id }
                                )
                                .await
                                .unwrap(),
                            OwnerAdmissionResult::Waiting
                        ));
                    } else {
                        // An explicit correction may retire A before its receipt is published.
                        assert_eq!(
                            events
                                .append_user_followup(next)
                                .await
                                .unwrap()
                                .input_revision,
                            snapshot.input_revision + 1
                        );
                    }
                    crate::agent_exec_store::SignalAgentExecStore::new(db.clone())
                        .publish_once()
                        .await
                        .unwrap();
                }
            },
            async {
                model.await.unwrap();
            },
        );
    })
    .await
    .unwrap();
    let tasks = crate::entity::agent_exec_task::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let [task] = tasks.as_slice() else {
        panic!("expected one selected command")
    };
    assert_eq!(task.conversation_id, run);
    assert_eq!(task.target_connection_id, "live-command-selected");
    assert_eq!(task.status, crate::agent_exec_store::STATUS_DONE);
    if background {
        assert_eq!(
            task.delivery_state,
            crate::agent_exec_store::DELIVERY_CONSUMED
        );
        let row = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let session =
            desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                .unwrap();
        assert!(session.pending_auto_triggers.is_empty());
        if retired {
            assert_eq!(session.input_revision, snapshot.input_revision + 1);
            assert_eq!(session.automation_turns_used, 0);
            assert!(
                !session
                    .conversation
                    .iter()
                    .any(|message| message.text.contains("SELECTED_DISK_RECEIPT"))
            );
            assert!(
                task.result_text
                    .as_deref()
                    .unwrap()
                    .contains("SELECTED_DISK_RECEIPT")
            );
            return;
        }
        assert_eq!(session.input_revision, snapshot.input_revision);
        assert_eq!(session.automation_turns_used, 1);
        let (next, request_id) = queued.unwrap();
        let request_id = request_id.unwrap();
        let receipt = SignalAgentRunEventStore::new(db)
            .admit_user_input(
                &next,
                None,
                OwnerAdmission::Activate {
                    request_id: &request_id,
                },
            )
            .await
            .unwrap()
            .native()
            .unwrap();
        assert_eq!(receipt.input_revision, session.input_revision + 1);
    }
}

async fn respond_to_selected_command(
    connections: &SharedConnectionMap,
    db: crate::config::connection::DatabaseConnection,
    peer: actix_web::HttpResponse,
    background: bool,
) {
    use desk_agent_protocol::edge_exec::{
        EdgeExecDisposition, EdgeExecRequestPayload, EdgeExecResultPayload,
    };
    use desk_agent_protocol::{AgentOutcome, ExecOutput, ExecOutputStreams, OperationOutput};
    use desk_signal_facade::service::EdgeExecObserver;

    let frame = receive_device_frame(peer).await;
    assert_eq!(frame.signaling_type, SignalingType::ExecuteEdgePlan);
    assert_eq!(
        frame.to_connection_id.as_deref(),
        Some("live-command-selected")
    );
    let payload: serde_json::Value = frame.get_data().unwrap();
    let request: EdgeExecRequestPayload = serde_json::from_value(payload["inner"].clone()).unwrap();
    let EdgeExecRequestPayload::Agentic { plan, .. } = request else {
        panic!("expected exact command")
    };
    assert_eq!(plan.argv.last().map(String::as_str), Some("df -h"));
    if background {
        tokio::time::sleep(std::time::Duration::from_secs(9)).await;
    }
    let payload = EdgeExecResultPayload {
        request_id: frame.request_id.clone(),
        disposition: EdgeExecDisposition::Executed {
            outcome: AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
                started: true,
                termination_signal: None,
                failure: None,
                diagnostics: vec![],
                exit_code: Some(0),
                duration_ms: 1,
                redactions: vec![],
                streams: ExecOutputStreams::Split {
                    stdout: "SELECTED_DISK_RECEIPT".into(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                },
            })),
        },
    };
    let frame = SignalingModel::new(
        &frame.request_id,
        SignalingType::EdgeExecutionCompleted,
        None,
        None,
        Some(serde_json::to_value(payload).unwrap()),
        None,
    );
    let source = connections
        .read()
        .await
        .current_oss_server("command-selected")
        .unwrap()
        .clone();
    crate::agent_exec::SignalEdgeExecObserver::new(
        crate::agent_exec::global_agent_exec_pending(),
        db,
    )
    .on_fleet_exec_result(&source, &frame)
    .await;
}

#[actix_web::test]
async fn personal_input_reads_selected_devices_and_keeps_their_distinct_results() {
    use desk_agent_protocol::{Capability, computer_use::*};
    use desk_signal_facade::model::os::OperationSystemEnum;
    use serde_json::{Value, json};

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
            model: Some("selected-read-model".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    let now = chrono::Utc::now();
    let mut peers = Vec::new();
    for (id, platform) in [
        ("compare-mac", OperationSystemEnum::Mac),
        ("compare-linux", OperationSystemEnum::Linux),
        ("other-device", OperationSystemEnum::Windows),
    ] {
        crate::device_code_store::get_or_create(&db, id)
            .await
            .unwrap();
        let peer = register_request_device(&connections, id, id, platform).await;
        crate::computer_use_readiness::global_computer_use_readiness_cache()
            .update(
                &format!("live-{id}"),
                ComputerUseReadiness {
                    schema_version: COMPUTER_USE_SCHEMA_VERSION,
                    revision: 1,
                    observed_at: now.to_rfc3339(),
                    expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                    server_api_version: 1,
                    os: "linux".into(),
                    interactive_user_home: None,
                    interactive_session_incarnation: "selected-read-worker".into(),
                    local_ceiling_revision: 1,
                    capabilities: vec![ComputerUseCapabilityReadiness {
                        capability: Capability::SystemInfo,
                        adapter: ComputerUseAdapterRef {
                            kind: ComputerUseAdapterKind::SystemDiagnostics,
                            version: "1".into(),
                        },
                        supported: true,
                        ready: true,
                        reason: None,
                    }],
                    context_references: vec![],
                },
                now,
            )
            .unwrap();
        peers.push(peer);
    }
    let model = actix_web::rt::spawn(async move {
        let first = capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            let schema = request["tools"].as_array().unwrap().iter()
                .find(|tool| tool["function"]["name"] == "read_system_info").unwrap();
            assert_eq!(schema["function"]["parameters"]["required"], json!(["target_ref"]));
            let calls = ["compare-mac", "compare-linux", "other-device"].into_iter()
                .enumerate().map(|(index, target)| json!({
                    "index": index, "id": format!("read-{target}"), "type": "function",
                    "function": { "name": "read_system_info", "arguments": json!({"target_ref":target}).to_string() }
                })).collect::<Vec<_>>();
            memory_sse(json!({"tool_calls":calls}), "tool_calls")
        }).await;
        let answer = memory_sse(json!({"content":"Compared both selected devices"}), "stop");
        let (second, _) = capture_openai_answer_with_title(&listener, &answer).await;
        [first, second]
    });
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let turn = run_turn(
        connections.clone(),
        db.clone(),
        "selected-read-request".into(),
        "owner-browser".into(),
        String::new(),
        owner,
        String::new(),
        AiAssistantAsk {
            target_refs: vec!["compare-mac".into(), "compare-linux".into()],
            question: "Compare the operating system versions of these two devices".into(),
            client_message_id: "selected-read-message".into(),
            conversation_id: Some("selected-read-conversation".into()),
            client_timezone: "UTC".into(),
            ..Default::default()
        },
    );
    let mac = respond_to_system_info(
        &connections,
        "compare-mac",
        peers.remove(0),
        "fixture-macos-version",
    );
    let linux = respond_to_system_info(
        &connections,
        "compare-linux",
        peers.remove(0),
        "fixture-linux-version",
    );
    tokio::time::timeout(SCENARIO_TIMEOUT, async { tokio::join!(turn, mac, linux) })
        .await
        .unwrap();
    let [_, response] = tokio::time::timeout(MODEL_IO_TIMEOUT, model)
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.contains("fixture-macos-version"));
    assert!(response.contains("fixture-linux-version"));
    let work = crate::entity::agent_action_item::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let mut targets = work
        .iter()
        .map(|item| item.target_device_id.as_str())
        .collect::<Vec<_>>();
    targets.sort_unstable();
    assert_eq!(targets, ["compare-linux", "compare-mac"]);
}

async fn respond_to_system_info(
    connections: &SharedConnectionMap,
    target: &str,
    peer: actix_web::HttpResponse,
    version: &str,
) {
    use desk_agent_protocol::{
        AgentOutcome, CpuInfo, MemoryInfo, OperationOutput, ReadContextOutput, SystemInfoOutput,
        remote_tool::{RemoteToolOutput, RemoteToolRequest, RemoteToolResponse},
    };
    use desk_signal_facade::service::RemoteToolObserver;

    let frame = receive_device_frame(peer).await;
    assert_eq!(frame.signaling_type, SignalingType::InvokeRemoteTool);
    let read: RemoteToolRequest = frame.get_data().unwrap();
    assert_eq!(read.envelope.target.device_id, target);
    assert!(matches!(
        read.envelope.operation.input,
        desk_agent_protocol::ReadContextInput {
            kind: desk_agent_protocol::ContextKind::SystemInfo(_),
        }
    ));
    let result = RemoteToolOutput {
        outcome: AgentOutcome::Ok(OperationOutput::ReadContext(ReadContextOutput::SystemInfo(
            SystemInfoOutput {
                hostname: target.into(),
                os: target.into(),
                os_version: version.into(),
                arch: "arm64".into(),
                uptime_seconds: 1,
                cpu: CpuInfo {
                    usage_percent: 0.0,
                    logical_cores: 4,
                },
                memory: MemoryInfo {
                    total_bytes: 2,
                    used_bytes: 1,
                },
                disks: vec![],
            },
        ))),
        image: None,
        document_preview: None,
        document_preview_page: None,
    };
    let source = connections
        .read()
        .await
        .current_oss_server(target)
        .unwrap()
        .clone();
    let observer = crate::remote_tool_edge::SignalRemoteToolObserver::new(
        crate::remote_tool_edge::global_remote_tool_pending(),
    );
    for chunk in desk_diagnose_core::chunk::chunk_bytes(
        &read.request_id,
        &serde_json::to_vec(&result).unwrap(),
        256,
    ) {
        let response = SignalingModel::new(
            "selected-read-result",
            SignalingType::RemoteToolOutputUpdated,
            None,
            None,
            Some(serde_json::to_value(RemoteToolResponse::Chunk(chunk)).unwrap()),
            None,
        );
        observer.on_remote_tool_response(&source, &response).await;
    }
}

async fn receive_device_frame(peer: actix_web::HttpResponse) -> SignalingModel {
    use actix_http::ws::{Codec, Frame};
    use actix_web::body::MessageBody;
    use tokio_util::codec::Decoder;

    let mut body = std::pin::pin!(peer.into_body());
    let mut codec = Codec::new().client_mode();
    let mut buffer = bytes::BytesMut::new();
    loop {
        let chunk = std::future::poll_fn(|cx| body.as_mut().poll_next(cx))
            .await
            .unwrap()
            .unwrap();
        buffer.extend_from_slice(&chunk);
        if let Some(Frame::Text(text)) = codec.decode(&mut buffer).unwrap() {
            return serde_json::from_slice::<SignalingModel>(&text).unwrap();
        }
    }
}

fn assert_targetless_web_tools(request: &serde_json::Value, approved: bool) {
    let names = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        names.contains(desk_diagnose_core::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME),
        "{names:?}"
    );
    assert_eq!(
        names.contains(desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME),
        approved,
        "{names:?}"
    );
    assert!(
        names.iter().all(|name| matches!(
            *name,
            "describe_tools"
                | "request_permissions"
                | "fetch_public_web_page"
                | "read_conversation_attachment"
                | "search_memories"
                | "read_memory_history"
                | "read_memory_source"
                | "manage_memories"
        )),
        "{names:?}"
    );
}

#[actix_web::test]
async fn targetless_web_decision_resumes_the_original_model_turn_without_a_host() {
    use crate::controller::ai_assistant_session::{PermissionDecisionBody, decide_permission_on};
    use actix_web::body::to_bytes;
    use serde_json::{Value, json};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
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
            model: Some("targetless-web-fixture".into()),
            base_url: Some(format!("http://{address}")),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let gateway = actix_web::rt::spawn(async move {
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert_targetless_web_tools(&request, false);
            memory_sse(json!({"tool_calls":[{"index":0,"id":"request-web-read","type":"function","function":{
                "name":"request_permissions",
                "arguments":json!({"items":[{"tool_name":"fetch_public_web_page","exact_input":{"url":"https://docs.example.invalid/guide"},"reason":"Read the requested public guide"}]}).to_string(),
            }}]}), "tool_calls")
        }).await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert_targetless_web_tools(&request, true);
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["role"] == "user" && message["content"] == "Read the public guide"
                    })
            );
            super::permission_object::tool_reply(
                "fetch_public_web_page",
                json!({"url":"https://docs.example.invalid/guide"}),
            )
        })
        .await;
        capture_one_openai_request_with_reply(&listener, |body| {
            let request: Value = serde_json::from_slice(body).unwrap();
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["role"] == "tool"
                            && message["tool_call_id"] == "call-fetch_public_web_page"
                    })
            );
            memory_sse(
                json!({"content":"The original web request resumed."}),
                "stop",
            )
        })
        .await;
    });
    let connections = web::Data::new(SharedConnectionMap::new());
    let owner = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID;
    let actor = owner.to_string();
    let intent = "targetless-web-session";
    tokio::time::timeout(
        SCENARIO_TIMEOUT,
        run_turn(
            connections.clone(),
            db.clone(),
            "targetless-web-input".into(),
            "owner-browser".into(),
            String::new(),
            owner,
            String::new(),
            AiAssistantAsk {
                question: "Read the public guide".into(),
                client_message_id: "targetless-web-input".into(),
                conversation_id: Some(intent.into()),
                client_timezone: "UTC".into(),
                ..Default::default()
            },
        ),
    )
    .await
    .unwrap();
    let run =
        derive_assistant_conversation_key(&actor, AssistantConversationScope::Personal, intent);
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let pending = sessions
        .read_snapshot_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .unwrap();
    let [request] = pending.permission_requests.as_slice() else {
        panic!(
            "expected one web permission request, got {:?}",
            pending
                .messages
                .iter()
                .filter(|message| message.role == desk_diagnose_core::chat::ChatRole::Tool)
                .map(|message| &message.text)
                .collect::<Vec<_>>()
        )
    };
    let [item] = request.items.as_slice() else {
        panic!("expected one permission item")
    };
    let body: PermissionDecisionBody = serde_json::from_value(json!({
        "conversation":intent, "requestId":request.request_id,
        "items":[{"itemId":item.item_id,"decision":"approve","resource_scope":item.resource_scope,
            "operation_scope":item.operation_scope,"ttl_seconds":60,"max_uses":1}],
    }))
    .unwrap();
    let response = decide_permission_on(&db, connections, web::Json(body))
        .await
        .unwrap();
    let response: Value =
        serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
    assert_eq!(response["data"]["state"], "approved", "{response}");
    match tokio::time::timeout(MODEL_IO_TIMEOUT, gateway).await {
        Ok(result) => result.unwrap(),
        Err(_) => {
            let snapshot = sessions
                .read_snapshot_for_subject(&run, &actor, "")
                .await
                .unwrap()
                .unwrap();
            let dispatches = crate::entity::agent_capability_dispatch_outbox::Entity::find()
                .all(&db)
                .await
                .unwrap();
            panic!(
                "web resume did not finish model I/O, terminal error {:?}, {} dispatches",
                snapshot.terminal_error,
                dispatches.len()
            );
        }
    }
    let final_snapshot = tokio::time::timeout(SCENARIO_TIMEOUT, async {
        loop {
            let snapshot = sessions
                .read_snapshot_for_subject(&run, &actor, "")
                .await
                .unwrap()
                .unwrap();
            if latest_committed_answer(&snapshot).is_some() {
                break snapshot;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        latest_committed_answer(&final_snapshot).as_deref(),
        Some("The original web request resumed.")
    );
    assert_eq!(final_snapshot.input_revision, pending.input_revision);
    let dispatches = crate::entity::agent_capability_dispatch_outbox::Entity::find()
        .all(&db)
        .await
        .unwrap();
    let [dispatch] = dispatches.as_slice() else {
        panic!("expected one central web dispatch")
    };
    let payload: crate::capability_grant_store::CapabilityDispatchPayload =
        serde_json::from_str(&dispatch.payload_json).unwrap();
    assert_eq!(payload.tool_name, "fetch_public_web_page");
    assert_eq!(payload.input_revision, pending.input_revision);
    let grant = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&run, &actor, "")
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(payload.grant_id, grant.grant_id);
    assert!(grant.target_device_id.is_empty());
}

#[actix_web::test]
async fn actual_owner_turn_commits_memory_without_hidden_model_calls_and_answers_after_the_tool() {
    owner_memory_turn(false, "device").await;
}

#[actix_web::test]
async fn personal_input_writes_memory_only_for_the_selected_device() {
    use crate::entity::{agent_assistant_origin, agent_memory, agent_memory_source};
    use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettingsUpdate;
    use sea_orm::{ColumnTrait, QueryFilter};
    use serde_json::{Value, json};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    for id in ["selected-a", "selected-b", "outside-c"] {
        crate::device_code_store::get_or_create(&db, id)
            .await
            .unwrap();
    }
    crate::model_provider::save(
        &db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            model: Some("selected-memory-fixture".into()),
            base_url: Some(format!("http://{}", listener.local_addr().unwrap())),
            api_key: Some("test-only-key".into()),
            max_context_bytes: Some(131_072),
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
            automatic_memory_enabled: true,
            model_history_search_enabled: settings.model_history_search_enabled,
            explanation_version: settings.explanation_version,
        },
    )
    .await
    .unwrap();
    let content = "Release commands use the slim runner";
    let gateway = actix_web::rt::spawn(async move {
        for (index, reference, expected) in [
            (0, "outside-c", "source_out_of_scope"),
            (1, "selected-b", "committed"),
        ] {
            let tool_id = format!("write-selected-{index}");
            capture_one_openai_request_with_reply(&listener, |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                assert!(
                    request["tools"].as_array().is_some_and(|tools| tools
                        .iter()
                        .any(|tool| tool["function"]["name"] == "manage_memories")),
                    "selected device input {index} should expose memory writes"
                );
                memory_sse(json!({"tool_calls":[{"index":0,"id":tool_id,"type":"function","function":{
                    "name":"manage_memories","arguments":json!({
                        "operation":"remember","scope":"current_device","target_ref":reference,"content":content,
                    }).to_string()
                }}]}), "tool_calls")
            }).await;
            capture_one_openai_request_with_reply(&listener, |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                let tool = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["tool_call_id"] == tool_id)
                    .unwrap_or_else(|| {
                        panic!("{tool_id} should receive feedback: {}", request["messages"])
                    });
                let result: Value =
                    serde_json::from_str(tool["content"].as_str().unwrap()).unwrap();
                assert_eq!(result["reason"], expected);
                memory_sse(json!({"content":"finished writing"}), "stop")
            })
            .await;
            if index == 0 {
                capture_one_openai_request_with_reply(&listener, |body| {
                    let request: Value = serde_json::from_slice(body).unwrap();
                    assert!(
                        request["messages"][0]["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("Create a short conversation title"))
                    );
                    memory_sse(json!({"content":r#"{"title":"Device memories"}"#}), "stop")
                })
                .await;
            }
        }
        for expected in [true, false] {
            capture_one_openai_request_with_reply(&listener, |body| {
                let request: Value = serde_json::from_slice(body).unwrap();
                let found = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|message| message["role"] == "user")
                    .filter_map(|message| {
                        serde_json::from_str::<Value>(message["content"].as_str()?).ok()
                    })
                    .any(|message| {
                        message["memories"].as_array().is_some_and(|memories| {
                            memories.iter().any(|entry| entry["content"] == content)
                        })
                    });
                assert_eq!(found, expected);
                memory_sse(json!({"content":"finished recalling"}), "stop")
            })
            .await;
        }
    });
    let connections = web::Data::new(SharedConnectionMap::new_oss());
    for (index, targets) in [
        vec!["selected-a", "selected-b"],
        vec!["selected-a", "selected-b"],
        vec!["selected-b"],
        vec!["selected-a"],
    ]
    .into_iter()
    .enumerate()
    {
        let ask = AiAssistantAsk {
            client_timezone: "UTC".into(),
            question: if index < 2 {
                "Remember this device's release configuration"
            } else {
                "Recall this device's release configuration"
            }
            .into(),
            client_message_id: format!("memory-selected-input-{index}"),
            conversation_id: Some("selected-memory-conversation".into()),
            target_refs: targets.into_iter().map(String::from).collect(),
            ..Default::default()
        };
        tokio::time::timeout(
            SCENARIO_TIMEOUT,
            run_turn_inner(
                connections.clone(),
                db.clone(),
                format!("selected-memory-request-{index}"),
                "owner-browser".into(),
                String::new(),
                crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
                String::new(),
                ask,
                None,
            ),
        )
        .await
        .unwrap();
    }
    gateway.await.unwrap();
    let instance = crate::config::ConfigConnection::config_read(&db)
        .await
        .metadata
        .instance
        .clone();
    let target = crate::assistant_data_store::source_device_ref(&instance, "selected-b");
    let memories = agent_memory::Entity::find().all(&db).await.unwrap();
    let [memory] = memories.as_slice() else {
        panic!("expected exactly one device memory");
    };
    assert_eq!(memory.device_ref, target);
    assert_eq!(memory.scope_key, format!("device:{target}"));
    assert_eq!(memory.body, content);
    let evidence = agent_memory_source::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(evidence.source_device_json.is_none());
    let origin = agent_assistant_origin::Entity::find()
        .filter(agent_assistant_origin::Column::OriginId.eq(evidence.origin_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(origin.device_ref.is_empty());
}

#[actix_web::test]
async fn actual_owner_turn_updates_observed_memory_and_refreshes_context_before_answering() {
    owner_memory_turn(true, "device").await;
}

#[actix_web::test]
async fn targetless_owner_turn_writes_and_recalls_personal_memory() {
    owner_memory_turn(false, "").await;
}

async fn owner_memory_turn(update: bool, device_id: &'static str) {
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
    let host_connection = if device_id.is_empty() {
        ""
    } else {
        "offline-host"
    };
    let session_key = |client_intent| {
        if device_id.is_empty() {
            derive_assistant_conversation_key(
                "1",
                AssistantConversationScope::Personal,
                client_intent,
            )
        } else {
            derive_conversation_key("1", device_id, Some(client_intent), "unused")
        }
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
            host_connection.into(),
            crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
            device_id.into(),
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
    assert_eq!(memories[0].scope_key, "personal");
    assert!(memories[0].device_ref.is_empty());
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
    let run_id = session_key(if update {
        "memory-update-entry"
    } else {
        "memory-entry"
    });
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
        host_connection.into(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        device_id.into(),
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
            let content = tool["content"].as_str().unwrap();
            let result: Value = serde_json::from_str(content)
                .unwrap_or_else(|error| panic!("invalid search result: {error}: {content}"));
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
        host_connection.into(),
        crate::control_authorizer::SINGLE_ACCOUNT_USER_ID,
        device_id.into(),
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
        .read_snapshot(&session_key("memory-recall-entry"))
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
