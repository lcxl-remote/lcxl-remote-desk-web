//! Actual input, permission persistence, resumed model and chunked object read.
use super::*;
use crate::remote_tool_edge::SignalRemoteToolObserver;
use chrono::Utc;
use desk_agent_protocol::computer_use::*;
use desk_agent_protocol::device_assistant::DeviceAssistantObjectContextOperation;
use desk_agent_protocol::remote_tool::{RemoteToolOutput, RemoteToolRequest, RemoteToolResponse};
use desk_agent_protocol::{AgentOutcome, Capability, ContextKind, ReadContextInput};
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

fn tool_reply(name: &str, arguments: serde_json::Value) -> String {
    let delta = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("call-{name}"),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]}}]});
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

#[actix_web::test]
async fn original_input_permission_decision_resumes_model_and_reads_original_object() {
    run_case(None).await;
}

#[actix_web::test]
async fn changed_model_object_input_subject_or_decision_cannot_resume_or_read() {
    for change in ["model", "detach", "new_input", "subject", "decision"] {
        run_case(Some(change)).await;
    }
}

#[actix_web::test]
async fn stale_readiness_or_legacy_input_cannot_restore_object_read_authority() {
    for change in ["readiness", "legacy"] {
        run_case(Some(change)).await;
    }
}

async fn run_case(change: Option<&str>) {
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
    let capability = registry
        .capability_for_tool("inspect_selected_file_metadata")
        .unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let mut replies = vec![
        tool_reply("request_capability_grants", serde_json::json!({"items":[{
            "item_id":"read", "provider_id":provider.wire.provider_id,
            "tool_name":"inspect_selected_file_metadata", "expected_effect":"read_file",
            "suggested_ttl_seconds":120, "suggested_max_uses":1, "reason":"Read the selected file metadata"
        }]})),
        tool_reply("inspect_selected_file_metadata", serde_json::json!({})),
        "data: {\"choices\":[{\"delta\":{\"content\":\"object-read-complete\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
    ];
    if matches!(change, Some("readiness" | "legacy")) {
        replies[2] = replies[2].replace("object-read-complete", "object-read-unavailable");
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
                            auth_context: AuthContext::token_auth(7, 1, RemoteDeskTypeEnum::Server),
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
                os: "fixture".into(),
                interactive_session_incarnation: "worker".into(),
                local_ceiling_revision: 1,
                capabilities: vec![ComputerUseCapabilityReadiness {
                    capability: Capability::FileMetadataRead,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::FileSystem,
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
    let client_id = "permission-object";
    let run_id = derive_conversation_key("7", "device", Some(client_id), "unused");
    let reference = ObjectRef {
        token: "original-file".into(),
        snapshot_id: "worker".into(),
        object_kind: ObjectKind::File,
        expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
    };
    apply_object_context_update(
        db.clone(),
        7,
        "device".into(),
        &DeviceAssistantObjectContextUpdate {
            conversation_id: client_id.into(),
            client_request_id: "attach-original".into(),
            operation: DeviceAssistantObjectContextOperation::AttachFile {
                object_ref: reference.clone(),
                display_summary: "selected".into(),
            },
        },
    )
    .await
    .unwrap();
    let sessions = crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(Some(client_id.into()), AgentSessionSurface::DeviceAssistant);
    let selected = sessions
        .read_snapshot(&run_id)
        .await
        .unwrap()
        .unwrap()
        .context_attachments
        .remove(0);
    tokio::time::timeout(
        Duration::from_secs(10),
        run_turn_inner(
            connections.clone(),
            db.clone(),
            "first".into(),
            "controller".into(),
            host.clone(),
            7,
            "device".into(),
            DeviceAssistantAsk {
                question: "Inspect only my selected file".into(),
                client_message_id: "original-user".into(),
                conversation_id: Some(client_id.into()),
                selected_attachment_ids: vec![selected.attachment_id.clone()],
                ..Default::default()
            },
            None,
        ),
    )
    .await
    .unwrap();
    let snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert_eq!(snapshot.permission_requests.len(), 1, "{snapshot:?}");
    let request = &snapshot.permission_requests[0];
    // A later attachment is not part of the input awaiting this decision.
    let later_reference = ObjectRef {
        token: "later-unselected-file".into(),
        ..reference.clone()
    };
    apply_object_context_update(
        db.clone(),
        7,
        "device".into(),
        &DeviceAssistantObjectContextUpdate {
            conversation_id: client_id.into(),
            client_request_id: "attach-later".into(),
            operation: DeviceAssistantObjectContextOperation::AttachFile {
                object_ref: later_reference,
                display_summary: "not selected for the original input".into(),
            },
        },
    )
    .await
    .unwrap();
    let (registry, inventory, _, _) = current_capability_projection(
        connections.as_ref(),
        &host,
        ModelCapabilities { image_input: false },
    )
    .await;
    sessions
        .decide_permission_request(
            &run_id,
            "7",
            "device",
            &request.request_id,
            vec![PermissionDecisionItem {
                item_id: "read".into(),
                decision: PermissionItemDecision::Approve {
                    resource_scope: request.items[0].resource_scope.clone(),
                    operation_scope: request.items[0].operation_scope.clone(),
                    export_destinations: vec![],
                    ttl_seconds: 120,
                    max_uses: 1,
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
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&run_id, "7", "device")
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(
        grants[0].resource_scope,
        desk_diagnose_core::capability_grant::fresh_object_resource_scope(std::slice::from_ref(
            &reference
        ))
    );
    let resume = || {
        resume_after_permission_decision(
            connections.clone(),
            db.clone(),
            format!("permission-resume-{}", request.request_id),
            host.clone(),
            if change == Some("subject") { 8 } else { 7 },
            "device".into(),
            run_id.clone(),
            if change == Some("decision") {
                "missing-decision".into()
            } else {
                request.request_id.clone()
            },
            DeviceAssistantAsk {
                question: "UNTRUSTED REPLAY TEXT".into(),
                client_message_id: "unused".into(),
                // Exercise the mobile session-only continuation and do not supply current selections.
                conversation_id: None,
                ..Default::default()
            },
        )
    };

    if matches!(change, Some("readiness" | "legacy")) {
        if change == Some("readiness") {
            let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
            let mut readiness = cache.get_fresh(&host, Utc::now()).unwrap().readiness;
            readiness.revision += 1;
            cache.update(&host, readiness, Utc::now()).unwrap();
        } else {
            use sea_orm::{ActiveModelTrait, ColumnTrait, QueryFilter, Set};
            let row = crate::entity::agent_run_event::Entity::find()
                .filter(crate::entity::agent_run_event::Column::EventId.eq("original-user"))
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            let mut payload: serde_json::Value = serde_json::from_str(&row.payload_json).unwrap();
            payload.as_object_mut().unwrap().remove("read_context");
            let mut row: crate::entity::agent_run_event::ActiveModel = row.into();
            row.payload_json = Set(payload.to_string());
            row.payload_schema_version = Set(1);
            row.update(&db).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), resume())
            .await
            .unwrap();
        let bodies = tokio::time::timeout(Duration::from_secs(5), capture)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bodies.len(), 3);
        assert!(!String::from_utf8_lossy(&bodies[2]).contains("synthetic-original-marker"));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), socket.next())
                .await
                .is_err()
        );
        let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
            .list_for_subject(&run_id, "7", "device")
            .await
            .unwrap();
        assert_eq!(grants[0].remaining_uses, 1);
        assert_eq!(
            latest_committed_answer(&sessions.read_snapshot(&run_id).await.unwrap().unwrap())
                .as_deref(),
            Some("object-read-unavailable")
        );
        socket.send(awc::ws::Message::Close(None)).await.unwrap();
        drop(socket);
        map.write().await.clear();
        handle.stop(true).await;
        task.await.unwrap().unwrap();
        db.close().await.unwrap();
        return;
    }
    if let Some(change) = change {
        match change {
            "model" => {
                let mut config = crate::model_provider::load(&db).await.unwrap();
                config.model = Some("different-model".into());
                config.profile_revision += 1;
                crate::model_provider::save(&db, config).await.unwrap();
            }
            "detach" => {
                apply_object_context_update(
                    db.clone(),
                    7,
                    "device".into(),
                    &DeviceAssistantObjectContextUpdate {
                        conversation_id: client_id.into(),
                        client_request_id: "withdraw-original".into(),
                        operation: DeviceAssistantObjectContextOperation::Detach {
                            attachment_id: selected.attachment_id.clone(),
                        },
                    },
                )
                .await
                .unwrap();
            }
            "new_input" => {
                let destination = crate::model_provider::load(&db)
                    .await
                    .unwrap()
                    .destination_identity()
                    .unwrap();
                crate::agent_run_event_store::SignalAgentRunEventStore::new(db.clone()).append_user_followup(
                    crate::agent_run_event_store::AppendUserFollowupParams {
                        event_id:"new-input".into(), run_id:run_id.clone(), client_conversation_id:Some(client_id.into()),
                        actor_id:"7".into(), device_id:"device".into(), surface:AgentSessionSurface::DeviceAssistant,
                        policy_revision:desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                        current_scope:snapshot.scope_snapshot.clone(),
                        read_context:Some(crate::agent_run_event_store::ReadContextSelection {tool_names:vec![], expires_at:None, object_attachments:vec![]}),
                        message:model_bound_user_message("new-input".into(),"A new requirement".into(),destination).unwrap(),
                        created_at:Utc::now().to_rfc3339(),
                    },
                ).await.unwrap();
            }
            "subject" | "decision" => {}
            _ => panic!("unknown negative case"),
        }
        let before = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), resume())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), socket.next())
                .await
                .is_err(),
            "{change}: unexpected device read"
        );
        assert_eq!(
            crate::entity::model_egress_receipt::Entity::find()
                .all(&db)
                .await
                .unwrap()
                .len(),
            1,
            "{change}: unexpected model request"
        );
        assert_eq!(
            crate::entity::agent_session::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            before,
            "{change}: unexpected resume mutation"
        );
        capture.abort();
        let _ = capture.await;
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
            panic!("missing object read")
        };
        let frame: SignalingModel = serde_json::from_slice(&text).unwrap();
        assert_eq!(frame.signaling_type, SignalingType::InvokeRemoteTool);
        let request: RemoteToolRequest = frame.get_data().unwrap();
        let ReadContextInput {
            kind: ContextKind::FileMetadataInspect(read),
        } = &request.envelope.operation.input
        else {
            panic!("wrong read")
        };
        assert_eq!(read.roots.as_slice(), std::slice::from_ref(&reference));
        assert!(u64::from(read.max_bytes) <= selected.bounds.max_bytes);
        let output = RemoteToolOutput {
            outcome: AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                desk_agent_protocol::ReadContextOutput::FileMetadataInspect(
                    FileMetadataInspectOutput {
                        snapshot_id: "worker".into(),
                        entries: vec![FileMetadataProjection {
                            object_ref: reference.clone(),
                            display_name: "synthetic-original-marker".into(),
                            is_directory: false,
                            byte_len: Some(16),
                            modified_at: None,
                        }],
                        directory_entries: vec![],
                        truncated: false,
                    },
                ),
            )),
            image: None,
        };
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
    tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(resume(), peer)
    })
    .await
    .unwrap();
    let bodies = tokio::time::timeout(Duration::from_secs(5), capture)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bodies.len(), 3);
    assert!(String::from_utf8_lossy(&bodies[2]).contains("synthetic-original-marker"));
    assert!(
        !bodies
            .iter()
            .any(|body| String::from_utf8_lossy(body).contains("UNTRUSTED REPLAY TEXT"))
    );
    let final_snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert_eq!(final_snapshot.input_revision, 1);
    assert_eq!(
        latest_committed_answer(&final_snapshot).as_deref(),
        Some("object-read-complete")
    );
    let bridge = final_snapshot
        .messages
        .iter()
        .find(|message| {
            desk_diagnose_core::permission_resume::is_permission_resume_message(message)
        })
        .unwrap();
    assert_eq!(
        bridge
            .data_envelope
            .as_ref()
            .unwrap()
            .provenance
            .source_envelope_ids
            .len(),
        1
    );
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    drop(socket);
    map.write().await.clear();
    handle.stop(true).await;
    task.await.unwrap().unwrap();
    db.close().await.unwrap();
}
