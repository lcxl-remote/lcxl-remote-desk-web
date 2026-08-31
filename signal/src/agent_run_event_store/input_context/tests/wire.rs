//! Production read dispatch/observer over loopback; identity and file bytes are synthetic.

use super::*;
use crate::remote_tool_edge::{
    SignalDeviceAssistantTools, SignalRemoteToolObserver, SignalRemoteToolPendingStore,
};
use desk_agent_protocol::{
    AgentOutcome, ContextKind, ReadContextInput,
    computer_use::{
        COMPUTER_USE_SCHEMA_VERSION, ComputerUseCapabilityReadiness, ComputerUseReadiness,
    },
    remote_tool::{RemoteToolOutput, RemoteToolRequest, RemoteToolResponse},
};
use desk_diagnose_core::{
    chunk::chunk_bytes, device_assistant::device_assistant_provider_registry, seam::ToolSeam,
};
use desk_signal_facade::{
    model::{
        auth_context::AuthContext,
        connection::{ConnectionModel, ConnectionState, SharedConnectionMap},
        signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
        version::VersionInfo,
    },
    service::RemoteToolObserver,
};
use futures_util::{SinkExt, StreamExt};
use sea_orm::EntityTrait;
use std::{sync::Arc, time::Duration};

#[actix_web::test]
async fn real_object_read_transport_keeps_original_refs_bounds_and_lineage_and_rejects_changed_input()
 {
    for case in [
        "success",
        "supersede",
        "oversized_success",
        "oversized_error",
    ] {
        let supersede = case == "supersede";
        let oversized = case.starts_with("oversized_");
        let store = setup("sqlite::memory:").await;
        let object = attach(&store, "first", ObjectKind::File).await;
        let mut params = input("message", vec![object.clone()]);
        params.current_scope.granted = vec![Capability::FileMetadataRead];
        params.read_context.as_mut().unwrap().tool_names =
            vec!["inspect_selected_file_metadata".into()];
        let receipt = store.append_user_followup(params.clone()).await.unwrap();
        let later = attach(&store, "not-selected", ObjectKind::File).await;
        let turn = "original-turn";
        sessions(&store)
            .with_expected_input_revision(receipt.input_revision)
            .claim_turn(ClaimTurnParams {
                conversation_id: "run".into(),
                actor_id: "7".into(),
                device_id: "device".into(),
                policy_revision: params.policy_revision,
                current_pdp_scope: params.current_scope.clone(),
                turn_id: turn.into(),
                request_id: Some("transport".into()),
                connection_id: Some("controller".into()),
                trigger_origin: desk_diagnose_core::session::TriggerOrigin::User,
                now: Utc::now().to_rfc3339(),
            })
            .await
            .unwrap();
        let map = Arc::new(SharedConnectionMap::new());
        let pending = Arc::new(SignalRemoteToolPendingStore::default());
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
                            let (response, socket, mut stream) =
                                actix_ws::handle(&request, payload)?;
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
                                auth_context: AuthContext::token_auth(
                                    7,
                                    1,
                                    RemoteDeskTypeEnum::Server,
                                ),
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
                    expires_at: (now + chrono::Duration::minutes(2)).to_rfc3339(),
                    server_api_version: 1,
                    os: "fixture".into(),
                    interactive_session_incarnation: "worker".into(),
                    local_ceiling_revision: 1,
                    capabilities: vec![ComputerUseCapabilityReadiness {
                        capability: Capability::FileMetadataRead,
                        adapter: desk_agent_protocol::computer_use::ComputerUseAdapterRef { kind: desk_agent_protocol::computer_use::ComputerUseAdapterKind::FileSystem, version: "1".into() },
                        supported: true,
                        ready: true,
                        reason: None,
                    }],
                    context_references: vec![],
                },
                now,
            )
            .unwrap();
        let reference: ObjectRef = serde_json::from_str(&object.object_ref.opaque_token).unwrap();
        let tools = SignalDeviceAssistantTools::new(
            store.db.clone(),
            device_assistant_provider_registry(),
            map.clone(),
            pending,
            host,
            "device".into(),
            "7".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            vec![reference.clone()],
            vec![reference.clone()],
            vec![],
            None,
            None,
            params.message.text.clone(),
            "run".into(),
            turn.into(),
            params.policy_revision,
            1,
            vec![],
            30_000,
        );
        tools
            .bind_original_input(
                receipt.input_revision,
                params.read_context.clone().unwrap(),
                destination(),
                Some("client-run".into()),
            )
            .unwrap();
        let call = ToolCall {
            id: "metadata-call".into(),
            name: "inspect_selected_file_metadata".into(),
            arguments_json: "{}".into(),
        };
        // Seed an owner-approved R1 grant; the transport test must exercise, not
        // bypass, the production authorization/reservation/dispatch pipeline.
        use desk_agent_protocol::capability_grant::{
            CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant, CapabilityGrantIssuer,
            CapabilityGrantLimits, CapabilityGrantUsePolicy, CapabilityRiskTier,
        };
        use sha2::{Digest, Sha256};
        let registry = device_assistant_provider_registry();
        let capability = registry.capability_for_tool(&call.name).unwrap();
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .unwrap();
        let canonical = desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
            &call.name,
            serde_json::json!({}),
        )
        .unwrap();
        let issued_at = Utc::now().timestamp_millis() as u64;
        let grant_expires_at = issued_at + 120_000;
        let output_limit = if oversized {
            512
        } else {
            capability.wire.limits.max_output_bytes
        };
        crate::capability_grant_store::SignalCapabilityGrantStore::new(store.db.clone())
            .issue(&CapabilityGrant {
                schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
                grant_id: "owner-approved-metadata-read".into(),
                actor_id: "7".into(),
                run_id: "run".into(),
                surface: desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
                target_device_id: "device".into(),
                target_session_id: None,
                provider_id: provider.wire.provider_id.clone(),
                capability_id: capability.wire.capability_id.clone(),
                tool_name: call.name.clone(),
                tool_schema_version: capability.wire.input_schema_version,
                effect: capability.wire.effect,
                risk_tier: CapabilityRiskTier::R1,
                resource_scope: desk_diagnose_core::capability_grant::fresh_object_resource_scope(
                    std::slice::from_ref(&reference),
                ),
                operation_scope: desk_diagnose_core::capability_grant::canonical_compiled_scope(
                    &capability.wire.authorization_hint.resources,
                    capability.wire.effect,
                )
                .unwrap()
                .operations,
                export_destinations: vec![],
                allowed_envelope_ids: vec![],
                allowed_content_digests_sha256: vec![],
                use_policy: CapabilityGrantUsePolicy::OneShotExact,
                canonical_input_digest_sha256: Some(format!("{:x}", Sha256::digest(canonical))),
                issued_by: CapabilityGrantIssuer::UserDecision,
                issued_at_unix_ms: issued_at,
                expires_at_unix_ms: grant_expires_at,
                remaining_uses: 1,
                limits: CapabilityGrantLimits {
                    max_bytes_per_call: output_limit,
                    max_items_per_call: capability.wire.limits.max_objects,
                    max_calls: 1,
                },
                policy_revision: params.policy_revision,
                readiness_revision: 1,
                revoked_at_unix_ms: None,
                revoked_reason: None,
            })
            .await
            .unwrap();
        let peer = async {
            let Ok(Some(Ok(awc::ws::Frame::Text(text)))) =
                tokio::time::timeout(Duration::from_secs(5), socket.next()).await
            else {
                return false;
            };
            let frame: SignalingModel = serde_json::from_slice(&text).unwrap();
            assert_eq!(frame.signaling_type, SignalingType::InvokeRemoteTool);
            let request: RemoteToolRequest = frame.get_data().unwrap();
            assert_eq!(request.tool_call_id, call.id);
            assert_eq!(request.envelope.target.device_id, "device");
            assert!(request.envelope.scope.expires_at.is_some());
            let ReadContextInput {
                kind: ContextKind::FileMetadataInspect(read),
            } = &request.envelope.operation.input
            else {
                panic!("expected file metadata")
            };
            assert_eq!(read.roots.as_slice(), std::slice::from_ref(&reference));
            assert!(u64::from(read.max_bytes) <= object.bounds.max_bytes);
            assert!(u64::from(read.max_bytes) <= output_limit);
            assert!(read.max_entries <= object.bounds.max_objects);
            assert!(
                !serde_json::to_string(&request)
                    .unwrap()
                    .contains(&later.object_ref.opaque_token)
            );
            if supersede {
                store
                    .append_user_followup(input("new-requirement", vec![]))
                    .await
                    .unwrap();
            }
            let oversized_marker = "must-not-reach-model-".repeat(128);
            let outcome = if case == "oversized_error" {
                AgentOutcome::Err(desk_agent_protocol::AgentError {
                    kind: desk_agent_protocol::AgentErrorKind::Internal,
                    message: oversized_marker.clone(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                })
            } else {
                AgentOutcome::Ok(desk_agent_protocol::OperationOutput::ReadContext(
                    desk_agent_protocol::ReadContextOutput::FileMetadataInspect(
                        desk_agent_protocol::computer_use::FileMetadataInspectOutput {
                            snapshot_id: "worker".into(),
                            entries: vec![
                                desk_agent_protocol::computer_use::FileMetadataProjection {
                                    object_ref: reference.clone(),
                                    display_name: if case == "oversized_success" {
                                        oversized_marker.clone()
                                    } else {
                                        "synthetic".into()
                                    },
                                    is_directory: false,
                                    byte_len: Some(16),
                                    modified_at: None,
                                },
                            ],
                            directory_entries: vec![],
                            truncated: false,
                        },
                    ),
                ))
            };
            let output = RemoteToolOutput {
                outcome,
                image: None,
            };
            let bytes = serde_json::to_vec(&output).unwrap();
            for chunk in chunk_bytes(&request.request_id, &bytes, 32) {
                let response = SignalingModel::new(
                    "result-frame",
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
            true
        };
        let (output, sent) = tokio::join!(tools.run_read(&call), peer);
        assert!(sent, "read failed before dispatch: {output:?}");
        if case != "success" {
            let error = output.unwrap_err();
            assert!(
                !error.message.contains("must-not-reach-model"),
                "{case}: oversized Provider body escaped its bounded transport error"
            );
        } else {
            let output = output.unwrap();
            let label = tools.read_data_envelope(&call, &output).unwrap().unwrap();
            assert_eq!(
                label.provenance.source_envelope_ids,
                [object.envelope.envelope_id]
            );
            assert_eq!(label.allowed_destinations, [destination()]);
            assert!(label.retention.expires_at_unix_ms.unwrap() <= object.expires_at_unix_ms);
            assert!(label.retention.expires_at_unix_ms.unwrap() <= grant_expires_at);
        }
        let outbox = crate::entity::agent_capability_dispatch_outbox::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            outbox.state,
            crate::capability_grant_store::DISPATCH_OUTBOX_COMPLETED,
            "{case}: a received Provider result is known even when its body is rejected"
        );
        let work = crate::entity::agent_action_item::Entity::find_by_id(outbox.work_id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            work.status,
            if case == "oversized_error" {
                crate::capability_grant_store::CAPABILITY_WORK_FAILED
            } else {
                crate::capability_grant_store::CAPABILITY_WORK_SUCCEEDED
            },
            "{case}: durable outcome must describe the Provider response, not local release"
        );
        socket.send(awc::ws::Message::Close(None)).await.unwrap();
        drop(socket);
        map.write().await.clear();
        handle.stop(true).await;
        task.await.unwrap().unwrap();
    }
}
