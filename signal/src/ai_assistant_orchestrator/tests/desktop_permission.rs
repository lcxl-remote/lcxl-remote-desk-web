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
    session::PersistedAgentSession,
};
use desk_signal_facade::{
    model::{
        auth_context::AuthContext, connection::ConnectionModel, signal::RemoteDeskTypeEnum,
        version::VersionInfo,
    },
    service::{ComputerActionObserver, RemoteToolObserver},
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

#[actix_web::test]
async fn automatic_desktop_approval_uses_runtime_limit_and_resumes_over_transport() {
    Box::pin(run_desktop_case_with_automatic(
        true,
        "inspect_desktop_session",
        false,
        true,
        false,
        false,
        false,
    ))
    .await;
}

async fn run_desktop_case(approve: bool, read_name: &str) {
    run_desktop_case_kind(approve, read_name, false).await;
}

async fn run_desktop_case_kind(approve: bool, read_name: &str, ordinary_followup: bool) {
    Box::pin(run_desktop_case_with_automatic(
        approve,
        read_name,
        ordinary_followup,
        false,
        false,
        false,
        false,
    ))
    .await;
}

async fn run_desktop_case_with_automatic(
    approve: bool,
    read_name: &str,
    ordinary_followup: bool,
    automatic: bool,
    concrete: bool,
    approve_concrete: bool,
    verified_native: bool,
) {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::ai_assistant_gate::enable_test_host();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = crate::model_provider::ModelProviderConfig {
        wire_protocol: Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions),
        model: Some("fake-model".into()),
        base_url: Some(format!("http://{address}")),
        api_key: Some("test-only-key".into()),
        max_context_bytes: Some(131_072),
        runtime_max_output_tokens: 128_000,
        ..Default::default()
    };
    crate::model_provider::save(&db, config).await.unwrap();
    let registry = ai_assistant_provider_registry();
    let capability = registry.capability_for_tool(read_name).unwrap();
    let mut replies = vec![
        tool_reply("describe_tools", serde_json::json!({"tool_names":[read_name]})),
        tool_reply("request_permissions", serde_json::json!({"items":[{
            "item_id":"read",
            "tool_name":read_name,
            "suggested_ttl_seconds":120, "suggested_max_uses":1, "reason":"Read the desktop session requested by the owner"
        }]})),
        tool_reply(read_name, if read_name == "inspect_desktop_ui" { serde_json::json!({"queries":["Calendar"]}) } else { serde_json::json!({}) }),
        "data: {\"choices\":[{\"delta\":{\"content\":\"object-read-complete\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
    ];
    if !approve {
        replies.remove(2);
    }
    if concrete {
        replies[0] = tool_reply(
            "describe_tools",
            serde_json::json!({"tool_names":[read_name,"execute_ui_actions"]}),
        );
        replies.insert(3, tool_reply("request_permissions", serde_json::json!({"items":[{
            "item_id":"ui", "tool_name":"execute_ui_actions", "reason":"Invoke the requested Calendar button",
            "suggested_ttl_seconds":120, "suggested_max_uses":2,
            "application_scope":{"application_id":"a1", "actions":["invoke"]}
        }]})));
        replies[3] = replies[3].replace("call-request_permissions", "call-ui-permission");
        replies.insert(
            4,
            tool_reply(
                "execute_ui_actions",
                serde_json::json!({
                    "application_id":"a1", "steps":[{"element_id":"e1", "action":{"kind":"invoke"}}]
                }),
            ),
        );
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
    let computer_observer = Arc::new(crate::remote_tool_edge::SignalComputerActionObserver::new(
        crate::remote_tool_edge::global_computer_action_pending(),
        db.clone(),
    ));
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
        let computer_observer = computer_observer.clone();
        let notify = server_notify.clone();
        actix_web::App::new().route(
            "/edge",
            actix_web::web::get().to(
                move |request: actix_web::HttpRequest, payload: actix_web::web::Payload| {
                    let map = map.clone();
                    let host = host.clone();
                    let observer = observer.clone();
                    let computer_observer = computer_observer.clone();
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
                                        computer_observer
                                            .on_computer_action_lifecycle(&peer, &frame)
                                            .await;
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
                interactive_user_home: None,
                interactive_session_incarnation: "worker".into(),
                local_ceiling_revision: 1,
                capabilities: if concrete {
                    vec![
                        capability.required_capability,
                        desk_agent_protocol::Capability::DesktopUiActionConfirmed,
                        desk_agent_protocol::Capability::DesktopSessionInspect,
                    ]
                } else {
                    vec![capability.required_capability]
                }
                .into_iter()
                .map(|capability| ComputerUseCapabilityReadiness {
                    capability,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::MacosAccessibility,
                        version: if concrete {
                            desk_diagnose_core::ai_assistant::MACOS_ACCESSIBILITY_ADAPTER_VERSION
                        } else {
                            "1"
                        }
                        .into(),
                    },
                    supported: true,
                    ready: true,
                    reason: None,
                })
                .collect(),
                context_references: vec![],
            },
            now,
        )
        .unwrap();
    let connections = web::Data::from(map.clone());
    let client_id = "desktop-permission";
    let run_id = derive_conversation_key("1", "device", Some(client_id), "unused");
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(Some(client_id.into()), AgentSessionSurface::AiAssistant);
    actix_web::rt::spawn(run_turn_inner(
        connections.clone(),
        db.clone(),
        "first".into(),
        "controller".into(),
        host.clone(),
        1,
        "device".into(),
        AiAssistantAsk {
            question: "Inspect the desktop session".into(),
            client_message_id: "input".into(),
            conversation_id: Some(client_id.into()),
            ..Default::default()
        },
        None,
    ))
    .await
    .unwrap();
    let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert!(
        !snapshot
            .scope_snapshot
            .granted
            .contains(&capability.required_capability)
    );
    assert_eq!(
        snapshot.permission_requests.len(),
        1,
        "{:?}",
        snapshot.terminal_error
    );
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
    if automatic {
        Box::pin(approve_with_independent_model(
            &db,
            &sessions,
            &run_id,
            &request.request_id,
            &registry,
            &inventory,
        ))
        .await;
    } else {
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
                    surface:
                        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
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
    }
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
                AiAssistantAsk {
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
                AiAssistantAsk {
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
                        displays: Vec::new(),
                        display_list_error: None,
                    },
                ),
            )),
            image: None,
            document_preview: None,
            document_preview_page: None,
        };

        if read_name == "inspect_desktop_ui" {
            output.outcome = AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::DesktopUiInspect(UiInspectOutput {
                    snapshot_id: "synthetic-original-marker".into(),
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::MacosAccessibility,
                        version: if concrete {
                            desk_diagnose_core::ai_assistant::MACOS_ACCESSIBILITY_ADAPTER_VERSION
                        } else {
                            "1"
                        }
                        .into(),
                    },
                    nodes: if concrete {
                        concrete_ui_nodes()
                    } else {
                        vec![]
                    },
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
    if concrete {
        let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
        let ui_request = snapshot.permission_requests.last().unwrap();
        assert_eq!(
            ui_request.items[0].tool_name,
            "execute_ui_actions",
            "{:?}",
            snapshot
                .messages
                .iter()
                .rev()
                .take(4)
                .map(|m| &m.text)
                .collect::<Vec<_>>()
        );
        let listener = Box::pin(approve_with_independent_model(
            &db,
            &sessions,
            &run_id,
            &ui_request.request_id,
            &registry,
            &inventory,
        ))
        .await;
        let review = actix_web::rt::spawn(async move {
            capture_one_openai_request_with_reply(listener, move |body| {
                concrete_review_reply(body, approve_concrete)
            })
            .await
        });
        let ui_resume = actix_web::rt::spawn(resume_after_permission_decision(
            connections.clone(),
            db.clone(),
            "concrete-resume".into(),
            host.clone(),
            1,
            "device".into(),
            run_id.clone(),
            ui_request.request_id.clone(),
            AiAssistantAsk {
                question: "Inspect the desktop session".into(),
                client_message_id: "ui-resume".into(),
                conversation_id: Some(client_id.into()),
                ..Default::default()
            },
        ));
        let native = async {
            if !approve_concrete {
                return;
            }
            let Some(Ok(awc::ws::Frame::Text(text))) = socket.next().await else {
                panic!("missing approved UI dispatch")
            };
            let frame: SignalingModel = serde_json::from_slice(&text).unwrap();
            assert_eq!(frame.signaling_type, SignalingType::DispatchComputerAction);
            let wrapper: desk_agent_protocol::authz::AuthorizedControlPayload<serde_json::Value> =
                frame.get_data().unwrap();
            let plan: SealedComputerActionPlan = serde_json::from_value(wrapper.inner).unwrap();
            plan.validate().unwrap();
            let started = ComputerActionStarted {
                work_id: plan.work_id.clone(),
                action_request_id: plan.action_request_id.clone(),
                execution_generation: plan.execution_generation.clone(),
                disposition: ComputerActionStartDisposition::MayHaveStarted,
                executor_accepted: true,
                reason: None,
            };
            let completed = ComputerActionCompleted {
                work_id: plan.work_id,
                action_request_id: plan.action_request_id,
                execution_generation: plan.execution_generation.clone(),
                result: if verified_native {
                    ComputerActionResultClass::Verified
                } else {
                    ComputerActionResultClass::Failed
                },
                facts: if verified_native {
                    vec![desk_agent_protocol::computer_use::ComputerActionStepFact {
                        index: 0,
                        changed: true,
                        verified: true,
                        summary: "synthetic verified invocation".into(),
                    }]
                } else {
                    vec![]
                },
                message: Some(if verified_native {
                    "synthetic verified completion".into()
                } else {
                    "synthetic native failure".into()
                }),
                output: None,
            };
            for (kind, data) in [
                (
                    SignalingType::ComputerActionStarted,
                    serde_json::to_value(started).unwrap(),
                ),
                (
                    SignalingType::ComputerActionCompleted,
                    serde_json::to_value(completed).unwrap(),
                ),
            ] {
                let frame = SignalingModel::new(
                    &plan.execution_generation,
                    kind,
                    None,
                    None,
                    Some(data),
                    None,
                );
                socket
                    .send(awc::ws::Message::Text(
                        serde_json::to_string(&frame).unwrap().into(),
                    ))
                    .await
                    .unwrap();
            }
        };
        let finished = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                async {
                    ui_resume.await.unwrap();
                },
                native
            );
        })
        .await;
        assert!(
            finished.is_ok(),
            "concrete dispatch failed: {:?}",
            sessions.read_snapshot(&run_id).await.unwrap()
        );
        let reviewed = tokio::time::timeout(Duration::from_secs(3), review).await;
        assert!(
            reviewed.is_ok(),
            "missing concrete review: {:?}",
            sessions
                .read_snapshot(&run_id)
                .await
                .unwrap()
                .unwrap()
                .messages
                .iter()
                .rev()
                .take(4)
                .map(|message| &message.text)
                .collect::<Vec<_>>()
        );
        let review_body: serde_json::Value =
            serde_json::from_slice(&reviewed.unwrap().unwrap()).unwrap();
        assert_eq!(review_body["max_tokens"], 128_000);
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        let reviews = crate::entity::agent_approval_review::Entity::find()
            .filter(crate::entity::agent_approval_review::Column::SourceKind.eq("concrete_call"))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(
            reviews[0].status,
            if approve_concrete {
                "approved"
            } else {
                "denied"
            }
        );
        assert!(reviews[0].provider_started_at_ms.is_some());
        if verified_native {
            let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
            assert!(
                snapshot
                    .messages
                    .iter()
                    .any(|message| { message.text.contains("synthetic verified invocation") }),
                "the verified device receipt must reach the resumed model history"
            );
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), socket.next())
                .await
                .is_err(),
            "no additional UI operation may be dispatched"
        );
    }
    let bodies = tokio::time::timeout(Duration::from_secs(5), capture)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bodies.len(), if concrete { 6 } else { 4 });
    for body in &bodies {
        let request: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(request["max_tokens"], 128_000);
    }
    let model_body = String::from_utf8_lossy(&bodies[3]);
    if read_name == "inspect_desktop_ui" {
        assert!(model_body.contains("DesktopUiInspect"));
        assert!(!model_body.contains("synthetic-original-marker"));
    } else {
        assert!(model_body.contains("DesktopSessionInspect"));
        assert!(!model_body.contains("synthetic-original-marker"));
        assert!(model_body.contains("s1"));
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
    assert_eq!(grants.len(), if concrete { 4 } else { 1 });
    assert!(
        grants
            .iter()
            .any(|grant| grant.tool_name == read_name && grant.remaining_uses == 0)
    );
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    drop(socket);
    map.write().await.clear();
    handle.stop(true).await;
    task.await.unwrap().unwrap();
    db.close().await.unwrap();
}

/// Keep the source budget, admission, provider I/O and durable decision in this
/// transport regression: a prompt-only test would miss a rejected delegation.
async fn approve_with_independent_model(
    db: &crate::config::connection::DatabaseConnection,
    sessions: &crate::agent_session_store::SignalAgentSessionStore,
    run_id: &str,
    request_id: &str,
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    inventory: &[desk_diagnose_core::capability_availability::CapabilityAvailability],
) -> Arc<TcpListener> {
    use crate::config::ConfigConnection;
    use crate::entity::{agent_approval_review as review_row, agent_session as session_row};
    use desk_agent_protocol::capability_provider::ProductSurface;
    use desk_diagnose_core::approval_review::ApprovalEvidenceTrust;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(run_id))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert!(
        session.delegation_group_id.is_some(),
        "exercise delegated review admission"
    );
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").await.unwrap());
    let address = listener.local_addr().unwrap();
    let mut config = crate::approval_model_provider::ApprovalModelConfig {
        enabled: true,
        ..Default::default()
    };
    config.gateway.wire_protocol =
        Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions);
    config.gateway.model = Some("fake-reviewer".into());
    config.gateway.base_url = Some(format!("http://{address}"));
    config.gateway.api_key = Some("test-only-key".into());
    config.gateway.output_limit_field =
        desk_diagnose_core::model_profile::OutputLimitField::MaxTokens;
    config.gateway.runtime_max_output_tokens = 128_000;
    db.config_context()
        .update::<_, sea_orm::DbErr, _>(|file| {
            file.approval_gateway = config;
            Ok(Some(()))
        })
        .await
        .unwrap();
    let now_ms = || Utc::now().timestamp_millis() as u64;
    crate::agent_approval_store::open_for_subject(
        db,
        run_id,
        "1",
        "device",
        session.input_revision,
        "test-owner-authorization".into(),
        now_ms(),
    )
    .await
    .unwrap();
    let candidates = crate::agent_approval_store::prepare_permission_review_batch(
        db,
        run_id,
        "1",
        "device",
        request_id,
        registry,
        ProductSurface::OssPersonalOwner,
        1,
        now_ms(),
    )
    .await
    .unwrap();
    assert!(!candidates.is_empty());
    for candidate in &candidates {
        let owner_evidence = candidate
            .context
            .evidence
            .iter()
            .find(|entry| entry.trust == ApprovalEvidenceTrust::OwnerInstruction)
            .unwrap();
        let decision = serde_json::json!({
            "candidate_id": candidate.candidate_id,
            "verdict": "approve", "reason_code": "within_scope",
            "reason": "The owner requested this desktop session read",
            "evidence_event_ids": [owner_evidence.event_id],
        });
        let reply = format!(
            "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":32,\"completion_tokens\":2050}}}}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"delta":{"content":decision.to_string()}}]}),
        );
        let capture_listener = listener.clone();
        let capture = actix_web::rt::spawn(async move {
            capture_one_openai_request_with_sse(capture_listener, &reply).await
        });
        let claim = crate::agent_approval_store::claim_permission_review(
            db,
            candidate,
            registry,
            ProductSurface::OssPersonalOwner,
            "test-review-lease",
            1,
            now_ms(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(claim.delegation_call.is_some());
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            crate::approval_reviewer::call_claimed_permission_review(db, candidate, &claim),
        )
        .await
        .unwrap()
        .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&capture.await.unwrap()).unwrap();
        assert_eq!(body["max_tokens"], 128_000);
        assert!(
            response.decision.is_some(),
            "the complete review must produce a decision"
        );
        let status = crate::agent_approval_store::settle_permission_review(
            db,
            candidate,
            &claim.lease_owner,
            claim.lease_epoch,
            response.decision.as_ref(),
            desk_diagnose_core::approval_review::reviewer_billed_tokens(response.usage),
            1,
            now_ms(),
        )
        .await
        .unwrap();
        assert_eq!(status, "approved");
        let row = review_row::Entity::find()
            .filter(review_row::Column::CandidateId.eq(&candidate.candidate_id))
            .one(db)
            .await
            .unwrap()
            .unwrap();
        assert!(row.provider_started_at_ms.is_some());
        assert!(row.provider_receipt_id.is_some());
    }
    sessions
        .decide_permission_request_by_ai(
            crate::agent_session_store::PermissionDecisionSubject {
                conversation_id: run_id,
                actor_id: "1",
                device_id: "device",
            },
            request_id,
            &candidates,
            crate::agent_session_store::PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry,
                inventory,
                readiness_revision: 1,
                now_unix_ms: now_ms(),
                implicit_fresh_object_refs: &[],
            },
            &Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
    listener
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

fn concrete_ui_nodes() -> Vec<UiNodeProjection> {
    [
        ("calendar", ObjectKind::Application, "AXApplication", None),
        ("button", ObjectKind::UiElement, "AXButton", Some(0)),
    ]
    .into_iter()
    .map(
        |(token, object_kind, role, parent_index)| UiNodeProjection {
            location: Default::default(),
            application_state: None,
            element_id: Some(token.into()),
            matched_queries: vec![],
            collapsed_children: 0,
            native_id: None,
            object_ref: ObjectRef {
                token: token.into(),
                snapshot_id: "ui-snapshot".into(),
                object_kind,
                expires_at: (Utc::now() + chrono::Duration::minutes(5)).to_rfc3339(),
            },
            parent_index,
            role: role.into(),
            name: Some("Calendar".into()),
            value: None,
            is_protected: false,
            enabled: true,
            supported_actions: vec![UiSemanticActionKind::Invoke],
        },
    )
    .collect()
}

fn concrete_review_reply(body: &[u8], approve: bool) -> String {
    let request: serde_json::Value = serde_json::from_slice(body).unwrap();
    let prompt = request["messages"][1]["content"].as_str().unwrap();
    let candidate_json = prompt
        .split("<candidate_json>\n")
        .nth(1)
        .unwrap()
        .split("\n</candidate_json>")
        .next()
        .unwrap();
    let candidate: desk_diagnose_core::approval_review::ApprovalReviewCandidate =
        serde_json::from_str(candidate_json).unwrap();
    let owner = candidate
        .context
        .evidence
        .iter()
        .find(|e| {
            e.trust == desk_diagnose_core::approval_review::ApprovalEvidenceTrust::OwnerInstruction
        })
        .unwrap();
    let decision = serde_json::json!({"candidate_id":candidate.candidate_id,"verdict":if approve {"approve"}else{"deny"},
        "reason_code":"test_review","reason":"Synthetic review of this exact action",
        "evidence_event_ids":[owner.event_id]});
    format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":32,\"completion_tokens\":8}}}}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices":[{"delta":{"content":decision.to_string()}}]})
    )
}

#[test]
fn automatic_concrete_ui_review_fits_production_thread_stack() {
    std::thread::Builder::new()
        .name("concrete-ui-approval-stack".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            actix_web::rt::System::new().block_on(Box::pin(run_desktop_case_with_automatic(
                true,
                "inspect_desktop_ui",
                false,
                false,
                true,
                false,
                false,
            )));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn approved_concrete_ui_dispatch_and_completion_fit_production_thread_stack() {
    std::thread::Builder::new()
        .name("approved-concrete-ui-stack".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            actix_web::rt::System::new().block_on(Box::pin(run_desktop_case_with_automatic(
                true,
                "inspect_desktop_ui",
                false,
                false,
                true,
                true,
                false,
            )));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn verified_concrete_ui_completion_fits_production_thread_stack() {
    std::thread::Builder::new()
        .name("verified-concrete-ui-stack".into())
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            actix_web::rt::System::new().block_on(Box::pin(run_desktop_case_with_automatic(
                true,
                "inspect_desktop_ui",
                false,
                false,
                true,
                true,
                true,
            )));
        })
        .unwrap()
        .join()
        .unwrap();
}
