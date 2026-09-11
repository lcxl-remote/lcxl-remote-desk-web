//! Empty-context desktop permission decisions over the real model/edge transports.
use super::permission_object::tool_reply;
use super::*;
use crate::remote_tool_edge::SignalRemoteToolObserver;
use chrono::Utc;
use desk_agent_protocol::computer_use::*;
use desk_agent_protocol::remote_tool::{RemoteToolOutput, RemoteToolRequest, RemoteToolResponse};
use desk_agent_protocol::{AgentOutcome, ContextKind, ReadContextInput};
use desk_diagnose_core::{
    chunk::chunk_bytes,
    dynamic_run::{PermissionDecisionItem, PermissionItemDecision},
};
use desk_signal_facade::{
    model::{
        auth_context::AuthContext, connection::ConnectionModel, signal::RemoteDeskTypeEnum,
        version::VersionInfo,
    },
    service::RemoteToolObserver,
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};

#[actix_web::test]
async fn unselected_desktop_read_requires_owner_approval_and_resumes_over_transport() {
    run_desktop_case(true, "inspect_desktop_session").await;
}

#[actix_web::test]
async fn rejected_desktop_permission_resumes_without_reading() {
    run_desktop_case(false, "inspect_desktop_session").await;
}

#[actix_web::test]
async fn unselected_semantic_ui_read_resumes_after_owner_approval() {
    run_desktop_case(true, "inspect_desktop_ui").await;
}

#[actix_web::test]
async fn ordinary_followup_reuses_approved_desktop_reads_over_transport() {
    run_desktop_case_kind(true, "inspect_desktop_ui", true).await;
}

async fn run_desktop_case(approve: bool, read_name: &str) {
    run_desktop_case_kind(approve, read_name, false).await;
}

async fn run_desktop_case_kind(approve: bool, read_name: &str, ordinary_followup: bool) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}")),
        api_key: Some("test-only-key".into()),
        max_context_bytes: Some(131_072),
        ..Default::default()
    };
    crate::model_provider::save(&db, config).await.unwrap();
    let registry = device_assistant_provider_registry();
    let capability = registry.capability_for_tool(read_name).unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let mut replies = vec![
        tool_reply("load_capability_details", serde_json::json!({"tool_names":[read_name]})),
        tool_reply("request_capability_grants", serde_json::json!({"items":[{
            "item_id":"read", "provider_id":provider.wire.provider_id,
            "tool_name":read_name, "expected_effect":capability.wire.effect,
            "suggested_ttl_seconds":120, "suggested_max_uses":1, "reason":"Read the desktop session requested by the owner"
        }]})),
        tool_reply(read_name, if read_name == "inspect_desktop_ui" { serde_json::json!({"query":{"any":["Calendar"]}}) } else { serde_json::json!({}) }),
        "data: {\"choices\":[{\"delta\":{\"content\":\"object-read-complete\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
    ];
    if !approve {
        replies.remove(2);
    }
    let capture = actix_web::rt::spawn(async move {
        let mut bodies = Vec::new();
        for reply in replies {
            bodies.push(capture_one_openai_request_with_sse(&listener, &reply).await);
        }
        bodies
    });
    let map = Arc::new(SharedConnectionMap::new());
    let pending = crate::remote_tool_edge::global_remote_tool_pending();
    let observer = Arc::new(SignalRemoteToolObserver::new(pending.clone()));
    let host = format!("host-{}", uuid::Uuid::new_v4());
    let notified = Arc::new(tokio::sync::Notify::new());
    let server_map = map.clone();
    let server_host = host.clone();
    let server_notify = notified.clone();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(move || {
        let map = server_map.clone();
        let host = server_host.clone();
        let observer = observer.clone();
        let notify = server_notify.clone();
        actix_web::App::new().route(
            "/edge",
            actix_web::web::get().to(
                move |request: actix_web::HttpRequest, payload: actix_web::web::Payload| {
                    let map = map.clone();
                    let host = host.clone();
                    let observer = observer.clone();
                    let notify = notify.clone();
                    async move {
                        let (response, socket, mut stream) = actix_ws::handle(&request, payload)?;
                        let peer = ConnectionState {
                            model: ConnectionModel {
                                connection_id: host.clone(),
                                ip: None,
                                version_info: VersionInfo::new(
                                    1,
                                    1,
                                    "fixture".into(),
                                    RemoteDeskTypeEnum::Server,
                                    None,
                                    Some("device".into()),
                                ),
                                device_id: None,
                                owner_node_id: None,
                            },
                            session: Arc::new(tokio::sync::RwLock::new(socket)),
                            terminal_connection_ids: Default::default(),
                            request_callback_map: Default::default(),
                            device_code: None,
                            auth_context: AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
                        };
                        map.write().await.insert(host, peer.clone());
                        notify.notify_one();
                        actix_web::rt::spawn(async move {
                            while let Some(Ok(message)) = stream.next().await {
                                match message {
                                    actix_ws::Message::Text(text) => {
                                        let frame: SignalingModel =
                                            serde_json::from_str(&text).unwrap();
                                        observer.on_remote_tool_response(&peer, &frame).await;
                                    }
                                    actix_ws::Message::Close(_) => break,
                                    _ => {}
                                }
                            }
                        });
                        Ok::<_, actix_web::Error>(response)
                    }
                },
            ),
        )
    })
    .workers(1)
    .disable_signals()
    .shutdown_timeout(1)
    .listen(listener)
    .unwrap()
    .run();
    let handle = server.handle();
    let task = actix_web::rt::spawn(server);
    let (_, mut socket) = awc::Client::default()
        .ws(format!("http://{address}/edge"))
        .connect()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), notified.notified())
        .await
        .unwrap();

    let now = Utc::now();
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
                interactive_session_incarnation: "worker".into(),
                local_ceiling_revision: 1,
                capabilities: vec![ComputerUseCapabilityReadiness {
                    capability: capability.required_capability,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::MacosAccessibility,
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
    let connections = web::Data::from(map.clone());
    let client_id = "desktop-permission";
    let run_id = derive_conversation_key("1", "device", Some(client_id), "unused");
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(Some(client_id.into()), AgentSessionSurface::DeviceAssistant);
    run_turn_inner(
        connections.clone(),
        db.clone(),
        "first".into(),
        "controller".into(),
        host.clone(),
        1,
        "device".into(),
        DeviceAssistantAsk {
            question: "Inspect the desktop session".into(),
            client_message_id: "input".into(),
            conversation_id: Some(client_id.into()),
            ..Default::default()
        },
        None,
    )
    .await;
    let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert!(
        !snapshot
            .scope_snapshot
            .granted
            .contains(&capability.required_capability)
    );
    assert_eq!(snapshot.permission_requests.len(), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), socket.next())
            .await
            .is_err(),
        "no desktop read before approval"
    );
    let request = &snapshot.permission_requests[0];
    let (registry, inventory, _, _) = current_capability_projection(
        &db,
        connections.as_ref(),
        &host,
        ModelCapabilities { image_input: false },
    )
    .await;
    sessions
        .decide_permission_request(
            &run_id,
            "1",
            "device",
            &request.request_id,
            vec![PermissionDecisionItem {
                item_id: "read".into(),
                decision: if approve {
                    PermissionItemDecision::Approve {
                        resource_scope: request.items[0].resource_scope.clone(),
                        operation_scope: request.items[0].operation_scope.clone(),
                        export_destinations: vec![],
                        ttl_seconds: 120,
                        max_uses: 1,
                    }
                } else {
                    PermissionItemDecision::Deny
                },
            }],
            crate::agent_session_store::PermissionGrantIssuanceContext {
                surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms: Utc::now().timestamp_millis() as u64,
                implicit_fresh_object_refs: &[],
            },
            &Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
    let resume = async {
        if ordinary_followup {
            run_turn_inner(
                connections.clone(),
                db.clone(),
                "followup".into(),
                "controller".into(),
                host.clone(),
                1,
                "device".into(),
                DeviceAssistantAsk {
                    question: "Continue inspecting".into(),
                    client_message_id: "followup".into(),
                    conversation_id: Some(client_id.into()),
                    ..Default::default()
                },
                None,
            )
            .await;
        } else {
            resume_after_permission_decision(
                connections.clone(),
                db.clone(),
                "resume".into(),
                host.clone(),
                1,
                "device".into(),
                run_id.clone(),
                request.request_id.clone(),
                DeviceAssistantAsk {
                    question: "Inspect the desktop session".into(),
                    client_message_id: "resume".into(),
                    conversation_id: Some(client_id.into()),
                    ..Default::default()
                },
            )
            .await;
        }
    };
    if !approve {
        tokio::time::timeout(Duration::from_secs(10), resume)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), socket.next())
                .await
                .is_err()
        );
        let bodies = tokio::time::timeout(Duration::from_secs(5), capture)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bodies.len(), 3);
        let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
            .list_for_subject(&run_id, "1", "device")
            .await
            .unwrap();
        assert!(grants.is_empty());
        socket.send(awc::ws::Message::Close(None)).await.unwrap();
        drop(socket);
        map.write().await.clear();
        handle.stop(true).await;
        task.await.unwrap().unwrap();
        db.close().await.unwrap();
        return;
    }
    let peer = async {
        let Some(Ok(awc::ws::Frame::Text(text))) = socket.next().await else {
            panic!("missing desktop read")
        };
        let frame: SignalingModel = serde_json::from_slice(&text).unwrap();
        let request: RemoteToolRequest = frame.get_data().unwrap();
        assert!(matches!(
            request.envelope.operation.input,
            ReadContextInput {
                kind: ContextKind::DesktopSessionInspect(_) | ContextKind::DesktopUiInspect(_)
            }
        ));
        let mut output = RemoteToolOutput {
            outcome: AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::DesktopSessionInspect(
                    DesktopSessionInspectOutput {
                        session: ObjectRef {
                            token: "session-token".into(),
                            snapshot_id: "snapshot".into(),
                            object_kind: ObjectKind::DesktopSession,
                            expires_at: String::new(),
                        },
                        os: "macos".into(),
                        interactive_session_incarnation: "synthetic-original-marker".into(),
                        active_application: None,
                        active_application_name: None,
                    },
                ),
            )),
            image: None,
        };

        if read_name == "inspect_desktop_ui" {
            output.outcome = AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::DesktopUiInspect(UiInspectOutput {
                    snapshot_id: "synthetic-original-marker".into(),
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::MacosAccessibility,
                        version: "1".into(),
                    },
                    nodes: vec![],
                    owner_selectable_windows: vec![],
                    truncated: false,
                }),
            ));
        }

        for chunk in chunk_bytes(
            &request.request_id,
            &serde_json::to_vec(&output).unwrap(),
            32,
        ) {
            let response = SignalingModel::new(
                "result",
                SignalingType::RemoteToolOutputUpdated,
                None,
                None,
                Some(serde_json::to_value(RemoteToolResponse::Chunk(chunk)).unwrap()),
                None,
            );
            socket
                .send(awc::ws::Message::Text(
                    serde_json::to_string(&response).unwrap().into(),
                ))
                .await
                .unwrap();
        }
    };
    let completed = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(resume, peer)
    })
    .await;
    assert!(
        completed.is_ok(),
        "desktop resume failed: {:?}",
        sessions.read_snapshot(&run_id).await.unwrap()
    );
    let bodies = tokio::time::timeout(Duration::from_secs(5), capture)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bodies.len(), 4);
    let model_body = String::from_utf8_lossy(&bodies[3]);
    if read_name == "inspect_desktop_ui" {
        assert!(model_body.contains("DesktopUiInspect"));
        assert!(!model_body.contains("synthetic-original-marker"));
    } else {
        assert!(model_body.contains("synthetic-original-marker"));
    }
    assert_eq!(
        latest_committed_answer(&sessions.read_snapshot(&run_id).await.unwrap().unwrap())
            .as_deref(),
        Some("object-read-complete")
    );
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&run_id, "1", "device")
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].remaining_uses, 0);
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    drop(socket);
    map.write().await.clear();
    handle.stop(true).await;
    task.await.unwrap().unwrap();
    db.close().await.unwrap();
}

#[test]
fn desktop_permission_flow_fits_production_thread_stack() {
    std::thread::Builder::new()
        .name("desktop-permission-stack".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            actix_web::rt::System::new()
                .block_on(Box::pin(run_desktop_case(true, "inspect_desktop_ui")));
        })
        .unwrap()
        .join()
        .unwrap();
}
