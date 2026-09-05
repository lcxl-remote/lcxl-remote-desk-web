//! Actual input, permission persistence, resumed model and chunked object read.
use super::*;
use crate::remote_tool_edge::SignalRemoteToolObserver;
use chrono::Utc;
use desk_agent_protocol::computer_use::*;
use desk_agent_protocol::device_assistant::{
    DeviceAssistantContextUpdate, DeviceAssistantObjectContextOperation,
};
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
    run_case(None, ResumeMode::Direct).await;
}

#[actix_web::test]
async fn changed_model_object_input_subject_or_decision_cannot_resume_or_read() {
    for change in ["model", "detach", "new_input", "subject", "decision"] {
        run_case(Some(change), ResumeMode::Direct).await;
    }
}

#[actix_web::test]
async fn stale_readiness_or_legacy_input_cannot_restore_object_read_authority() {
    for change in ["readiness", "legacy"] {
        run_case(Some(change), ResumeMode::Direct).await;
    }
}

#[actix_web::test]
async fn scanner_recovers_committed_decision_once_with_original_model_and_object_read() {
    Box::pin(run_case(None, ResumeMode::Scan)).await;
}

#[actix_web::test]
async fn scanner_rechecks_model_original_object_and_input_before_resuming() {
    for change in ["model", "detach", "new_input"] {
        run_case(Some(change), ResumeMode::Scan).await;
    }
}

#[actix_web::test]
async fn periodic_executor_discovers_a_committed_decision_without_controller_wakeup() {
    run_case(None, ResumeMode::Loop).await;
}

#[actix_web::test]
async fn live_document_permission_resume_reads_the_frozen_target_over_real_transport() {
    for mode in [ResumeMode::Direct, ResumeMode::Scan] {
        Box::pin(run_case_with_live(None, mode, true)).await;
    }
}

#[actix_web::test]
async fn live_readiness_rotation_after_grant_fails_closed_without_device_read() {
    run_case_with_live(Some("live_target"), ResumeMode::Direct, true).await;
}

#[actix_web::test]
async fn changed_live_worker_selection_model_or_input_cannot_resume() {
    for change in ["live_worker", "live_withdraw", "model", "new_input"] {
        run_case_with_live(Some(change), ResumeMode::Direct, true).await;
    }
}

#[actix_web::test]
async fn a_live_target_change_during_read_prevents_content_from_reaching_the_model() {
    run_case_with_live(Some("live_after"), ResumeMode::Direct, true).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResumeMode {
    Direct,
    Scan,
    Loop,
}

async fn run_case(change: Option<&str>, mode: ResumeMode) {
    run_case_with_live(change, mode, false).await;
}

async fn run_case_with_live(change: Option<&str>, mode: ResumeMode, live: bool) {
    let scanner = mode != ResumeMode::Direct;
    let directory = tempfile::tempdir().unwrap();
    let url = if scanner {
        format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("resume.db").display()
        )
    } else {
        "sqlite::memory:".into()
    };
    let db = Database::connect(&url).await.unwrap();
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
    let read_name = if live {
        "inspect_live_document"
    } else {
        "inspect_selected_file_metadata"
    };
    let capability = registry.capability_for_tool(read_name).unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let mut replies = vec![
        tool_reply("request_capability_grants", serde_json::json!({"items":[{
            "item_id":"read", "provider_id":provider.wire.provider_id,
            "tool_name":read_name, "expected_effect":capability.wire.effect,
            "suggested_ttl_seconds":120, "suggested_max_uses":1, "reason":"Read the selected file metadata"
        }]})),
        tool_reply(read_name, serde_json::json!({})),
        "data: {\"choices\":[{\"delta\":{\"content\":\"object-read-complete\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".into(),
    ];
    if matches!(
        change,
        Some("readiness" | "legacy" | "live_after" | "live_target")
    ) {
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
    let live_reference = ObjectRef {
        token: "original-live-document".into(),
        snapshot_id: "live-snapshot".into(),
        object_kind: ObjectKind::Document,
        expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
    };
    crate::computer_use_readiness::global_computer_use_readiness_cache()
        .update(
            &host,
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 1,
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
                server_api_version: 1,
                os: if live { "macos" } else { "fixture" }.into(),
                interactive_session_incarnation: "worker".into(),
                local_ceiling_revision: 1,
                capabilities: vec![ComputerUseCapabilityReadiness {
                    capability: if live {
                        Capability::DocumentLiveInspect
                    } else {
                        Capability::FileMetadataRead
                    },
                    adapter: ComputerUseAdapterRef {
                        kind: if live {
                            ComputerUseAdapterKind::IworkPages
                        } else {
                            ComputerUseAdapterKind::FileSystem
                        },
                        version: "1".into(),
                    },
                    supported: true,
                    ready: true,
                    reason: None,
                }],
                context_references: if live {
                    vec![ComputerUseContextReference {
                        capability: Capability::DocumentLiveInspect,
                        object_ref: live_reference.clone(),
                    }]
                } else {
                    vec![]
                },
            },
            now,
        )
        .unwrap();
    let connections = web::Data::from(map.clone());
    let client_id = "permission-object";
    let run_id = derive_conversation_key("1", "device", Some(client_id), "unused");
    if live {
        update_context(
            connections.clone(),
            db.clone(),
            "select-live-transport".into(),
            "controller".into(),
            host.clone(),
            1,
            "device".into(),
            DeviceAssistantContextUpdate {
                conversation_id: client_id.into(),
                client_request_id: "select-live".into(),
                selected_capability_ids: vec![capability.wire.capability_id.clone()],
            },
        )
        .await;
    }
    let reference = ObjectRef {
        token: "original-file".into(),
        snapshot_id: "worker".into(),
        object_kind: ObjectKind::File,
        expires_at: (now + chrono::Duration::minutes(5)).to_rfc3339(),
    };
    apply_object_context_update(
        db.clone(),
        1,
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
            1,
            "device".into(),
            DeviceAssistantAsk {
                question: "Inspect only my selected file".into(),
                client_message_id: "original-user".into(),
                conversation_id: Some(client_id.into()),
                selected_attachment_ids: if live {
                    vec![]
                } else {
                    vec![selected.attachment_id.clone()]
                },
                selected_capability_ids: if live {
                    vec![capability.wire.capability_id.clone()]
                } else {
                    vec![]
                },
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
        1,
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
                implicit_fresh_object_refs: if live {
                    std::slice::from_ref(&live_reference)
                } else {
                    &[]
                },
            },
            &Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&run_id, "1", "device")
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(
        grants[0].resource_scope,
        desk_diagnose_core::capability_grant::fresh_object_resource_scope(std::slice::from_ref(
            if live { &live_reference } else { &reference }
        ))
    );
    let executor = crate::permission_resume_executor::SignalPermissionResumeExecutor::new(
        if scanner {
            Database::connect(&url).await.unwrap()
        } else {
            db.clone()
        },
        connections.clone(),
        std::sync::Arc::new(crate::device_assistant_gate::DeviceAssistantGate::new(
            desk_agent_protocol::device_assistant::DeviceAssistantSettings {
                revision: 1,
                enabled: true,
            },
        )),
    );
    if scanner {
        let original = map.write().await.remove(&host).unwrap();
        assert_eq!(executor.scan_once(0).await.unwrap().attempted, 0);
        map.write().await.insert(host.clone(), original.clone());
        let mut duplicate = original;
        duplicate.model.connection_id = format!("duplicate-{host}");
        map.write()
            .await
            .insert(duplicate.model.connection_id.clone(), duplicate);
        assert_eq!(executor.scan_once(0).await.unwrap().attempted, 0);
        map.write().await.remove(&format!("duplicate-{host}"));
        let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
        let ready = cache.get_fresh(&host, Utc::now()).unwrap().readiness;
        cache.remove_connection(&host);
        assert_eq!(executor.scan_once(0).await.unwrap().attempted, 0);
        cache.update(&host, ready, Utc::now()).unwrap();
        let row = crate::entity::agent_permission_resume::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "pending");
    }
    let resume = || async {
        if mode == ResumeMode::Loop {
            let runner = actix_web::rt::spawn(executor.clone().run());
            let settled = tokio::time::timeout(Duration::from_secs(12), async {
                loop {
                    let row = crate::entity::agent_permission_resume::Entity::find()
                        .one(&db)
                        .await
                        .unwrap()
                        .unwrap();
                    if row.state == "settled" {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await;
            runner.abort();
            let _ = runner.await;
            settled.unwrap();
            return;
        }
        if scanner {
            let (first, second) = tokio::join!(executor.scan_once(0), executor.scan_once(0));
            first.unwrap();
            second.unwrap();
            return;
        }
        resume_after_permission_decision(
            connections.clone(),
            db.clone(),
            format!("permission-resume-{}", request.request_id),
            host.clone(),
            if change == Some("subject") { 8 } else { 1 },
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
        .await;
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
            .list_for_subject(&run_id, "1", "device")
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
    if let Some(change) = change.filter(|change| *change != "live_after") {
        match change {
            "live_target" => {
                // Rotation after issuance changes readiness_revision, so the old
                // grant is stale even though the durable frozen target remains
                // the only target that a future freshly-authorized call may use.
                let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
                let mut current = cache.get_fresh(&host, Utc::now()).unwrap().readiness;
                current.revision += 1;
                current.context_references[0].object_ref.token = "other-live-document".into();
                cache.update(&host, current, Utc::now()).unwrap();
            }
            "live_worker" => {
                let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
                let mut current = cache.get_fresh(&host, Utc::now()).unwrap().readiness;
                current.revision += 1;
                current.interactive_session_incarnation = "other-worker".into();
                cache.update(&host, current, Utc::now()).unwrap();
            }
            "model" => {
                let mut config = crate::model_provider::load(&db).await.unwrap();
                config.model = Some("different-model".into());
                config.profile_revision += 1;
                crate::model_provider::save(&db, config).await.unwrap();
            }
            "live_withdraw" => {
                // Exercise the production 638 path after permission was granted.
                // The durable grant remains a historical authorization fact, but
                // withdrawing the live selection must invalidate the frozen input
                // before any resumed model call or device read can occur.
                update_context(
                    connections.clone(),
                    db.clone(),
                    "withdraw-live-transport".into(),
                    "offline-controller".into(),
                    host.clone(),
                    1,
                    "device".into(),
                    DeviceAssistantContextUpdate {
                        conversation_id: client_id.into(),
                        client_request_id: "withdraw-live".into(),
                        selected_capability_ids: vec![],
                    },
                )
                .await;
            }
            "detach" => {
                apply_object_context_update(
                    db.clone(),
                    1,
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
                        actor_id:"1".into(), device_id:"device".into(), surface:AgentSessionSurface::DeviceAssistant,
                        policy_revision:desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                        current_scope:snapshot.scope_snapshot.clone(),
                        read_context:Some(crate::agent_run_event_store::ReadContextSelection {tool_names:vec![], expires_at:None, object_attachments:vec![], live_targets:vec![]}),
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
        let receipt_count = crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .len();
        let after = crate::entity::agent_session::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        if change == "live_target" {
            assert_eq!(receipt_count, 3, "{change}: stale grant flow changed");
            assert_ne!(after, before, "{change}: stale status was not reported");
            let bodies = tokio::time::timeout(Duration::from_secs(5), capture)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(bodies.len(), 3);
            assert!(
                bodies.iter().all(
                    |body| !String::from_utf8_lossy(body).contains("synthetic-original-marker")
                )
            );
            let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
                .list_for_subject(&run_id, "1", "device")
                .await
                .unwrap();
            assert_eq!(grants.len(), 1);
            assert_eq!(grants[0].remaining_uses, 1);
            assert_eq!(
                latest_committed_answer(&sessions.read_snapshot(&run_id).await.unwrap().unwrap())
                    .as_deref(),
                Some("object-read-unavailable")
            );
        } else {
            assert_eq!(receipt_count, 1, "{change}: unexpected model request");
            assert_eq!(after, before, "{change}: unexpected resume mutation");
            capture.abort();
            let _ = capture.await;
        }
        if change == "live_withdraw" {
            let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
                .list_for_subject(&run_id, "1", "device")
                .await
                .unwrap();
            assert_eq!(grants.len(), 1);
            assert_eq!(grants[0].remaining_uses, 1);
        }
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
        if live {
            let ReadContextInput {
                kind: ContextKind::DocumentLiveInspect(read),
            } = &request.envelope.operation.input
            else {
                panic!("wrong live read")
            };
            assert_eq!(read.target.as_ref(), Some(&live_reference));
            assert!(read.batch_file.is_none());
        } else {
            let ReadContextInput {
                kind: ContextKind::FileMetadataInspect(read),
            } = &request.envelope.operation.input
            else {
                panic!("wrong read")
            };
            assert_eq!(read.roots.as_slice(), std::slice::from_ref(&reference));
            assert!(u64::from(read.max_bytes) <= selected.bounds.max_bytes);
        }
        let output = RemoteToolOutput {
            outcome: AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(if live {
                desk_agent_protocol::ReadContextOutput::DocumentLiveInspect(
                    LiveDocumentInspectOutput {
                        snapshot_id: live_reference.snapshot_id.clone(),
                        adapter: ComputerUseAdapterRef {
                            kind: ComputerUseAdapterKind::IworkPages,
                            version: "1".into(),
                        },
                        projection: LiveDocumentProjection::Document {
                            document: live_reference.clone(),
                            body_text: "synthetic-original-marker".into(),
                            body_sha256: "a".repeat(64),
                        },
                        batch_source: None,
                    },
                )
            } else {
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
                )
            })),
            image: None,
        };
        if change == Some("live_after") {
            let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
            let mut current = cache.get_fresh(&host, Utc::now()).unwrap().readiness;
            current.revision += 1;
            current.context_references[0].object_ref.token = "changed-during-read".into();
            cache.update(&host, current, Utc::now()).unwrap();
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
        tokio::join!(resume(), peer)
    })
    .await;
    assert!(
        completed.is_ok(),
        "resume/read timed out: {:?}",
        sessions.read_snapshot(&run_id).await.unwrap().map(|s| s
            .messages
            .into_iter()
            .map(|m| (m.role, m.text))
            .collect::<Vec<_>>())
    );
    let bodies = tokio::time::timeout(Duration::from_secs(5), capture)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bodies.len(), 3);
    assert_eq!(
        String::from_utf8_lossy(&bodies[2]).contains("synthetic-original-marker"),
        change != Some("live_after")
    );
    assert!(
        !bodies
            .iter()
            .any(|body| String::from_utf8_lossy(body).contains("UNTRUSTED REPLAY TEXT"))
    );
    let final_snapshot = sessions.read_snapshot(&run_id).await.unwrap().unwrap();
    assert_eq!(final_snapshot.input_revision, 1);
    assert_eq!(
        latest_committed_answer(&final_snapshot).as_deref(),
        Some(if change == Some("live_after") {
            "object-read-unavailable"
        } else {
            "object-read-complete"
        })
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
    if scanner {
        let before = crate::entity::model_egress_receipt::Entity::find()
            .all(&db)
            .await
            .unwrap();
        assert_eq!(before.len(), 3);
        assert_eq!(executor.scan_once(0).await.unwrap().scanned, 0);
        assert_eq!(
            crate::entity::model_egress_receipt::Entity::find()
                .all(&db)
                .await
                .unwrap(),
            before
        );
        let row = crate::entity::agent_permission_resume::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "settled");
        let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
            .list_for_subject(&run_id, "1", "device")
            .await
            .unwrap();
        assert_eq!(grants[0].remaining_uses, 0);
    }
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    drop(socket);
    map.write().await.clear();
    handle.stop(true).await;
    task.await.unwrap().unwrap();
    db.close().await.unwrap();
}
