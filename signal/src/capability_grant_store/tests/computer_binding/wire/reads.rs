use super::*;
use crate::remote_tool_edge::{
    SignalDeviceAssistantTools, SignalRemoteToolPendingStore, global_computer_action_pending,
};
use desk_agent_protocol::{
    Capability,
    authz::AuthorizedControlPayload,
    browser_control::{BrowserAction, BrowserActionResult},
    computer_use::{
        ComputerActionCompleted, ComputerActionOutput, ComputerActionResultClass,
        ComputerActionStepFact, ComputerUseCapabilityReadiness, ComputerUseReadiness,
    },
};
use desk_diagnose_core::seam::{ExecContext, ExecOutcome, ToolSeam};
use desk_signal_facade::model::connection::SharedConnectionMap;
use serde_json::json;

#[actix_web::test]
async fn actual_browser_read_tools_remain_inline_without_mutating_origin_or_acceptance() {
    let directory = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(file_db(&directory.path().join("read-tools.db")).await).await;
    let connection_id = format!("read-host-{}", uuid::Uuid::new_v4());
    let connections = Arc::new(SharedConnectionMap::new());
    let map = connections.clone();
    let bound_id = connection_id.clone();
    let observer = Arc::new(SignalComputerActionObserver::new(
        global_computer_action_pending(),
        fixture.store.db.clone(),
    ));
    let connected = Arc::new(tokio::sync::Notify::new());
    let notify = connected.clone();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(move || {
        let map = map.clone();
        let bound_id = bound_id.clone();
        let observer = observer.clone();
        let notify = notify.clone();
        actix_web::App::new().route(
            "/edge",
            actix_web::web::get().to(
                move |req: actix_web::HttpRequest, payload: actix_web::web::Payload| {
                    let map = map.clone();
                    let bound_id = bound_id.clone();
                    let observer = observer.clone();
                    let notify = notify.clone();
                    async move {
                        let (response, socket, mut stream) = actix_ws::handle(&req, payload)?;
                        let state = ConnectionState {
                            model: ConnectionModel {
                                connection_id: bound_id.clone(),
                                ip: None,
                                version_info: VersionInfo::new(
                                    1,
                                    1,
                                    "fixture".into(),
                                    RemoteDeskTypeEnum::Server,
                                    None,
                                    Some("device-1".into()),
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
                        map.write().await.insert(bound_id, state.clone());
                        notify.notify_one();
                        actix_web::rt::spawn(async move {
                            while let Some(Ok(message)) = stream.next().await {
                                match message {
                                    actix_ws::Message::Text(text) => {
                                        let frame: SignalingModel =
                                            serde_json::from_str(&text).unwrap();
                                        observer.on_computer_action_lifecycle(&state, &frame).await;
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
    tokio::time::timeout(Duration::from_secs(5), connected.notified())
        .await
        .unwrap();
    let now = Utc::now();
    let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
    cache
        .update(
            &connection_id,
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 9,
                observed_at: now.to_rfc3339(),
                expires_at: (now + chrono::Duration::minutes(2)).to_rfc3339(),
                server_api_version: 1,
                os: "fixture".into(),
                interactive_session_incarnation: "desktop-1".into(),
                local_ceiling_revision: 1,
                capabilities: [
                    Capability::BrowserPageObserve,
                    Capability::BrowserPageNavigateConfirmed,
                ]
                .into_iter()
                .map(|capability| ComputerUseCapabilityReadiness {
                    capability,
                    adapter: fixture.plan.adapter.clone(),
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
    let tools = SignalDeviceAssistantTools::new(
        fixture.store.db.clone(),
        desk_diagnose_core::device_assistant::device_assistant_provider_registry(),
        connections.clone(),
        Arc::new(SignalRemoteToolPendingStore::default()),
        connection_id.clone(),
        "device-1".into(),
        "actor-1".into(),
        None,
        None,
        None,
        None,
        None,
        None,
        vec![],
        vec![],
        vec![],
        Some(fixture.plan.actions[0].target.clone()),
        None,
        "inspect selected browser page".into(),
        "run-1".into(),
        "turn-1".into(),
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        9,
        vec![],
        30_000,
    );
    let page = json!({"schema_version":1, "adapter": {
        "engine":"chrome_devtools_mcp", "device_id":"device-1", "os_session_id":"desktop-1", "browser_major_version":145,
        "browser_version":"145", "adapter_id":"fixture", "adapter_version":"1", "profile_incarnation":"profile-1", "connection_revision":1
    }, "page_id":"page-1", "page_incarnation":"page-1-first", "origin":{"kind":"https","host_ascii":"example.test","port":443},
    "document_revision":1, "url_sha256":"a".repeat(64), "observed_at_unix_ms":now.timestamp_millis()});
    let element = json!({"page_id":"page-1", "page_incarnation":"page-1-first", "document_revision":1,
        "element_id":"field-1", "role":"textbox", "accessible_name":"approved field", "value":null, "element_revision":1});
    for name in [
        "browser_take_snapshot",
        "browser_wait_for",
        "browser_open_page",
    ] {
        let mutating = name == "browser_open_page";
        let arguments = if name == "browser_take_snapshot" {
            json!({"page":page,"max_elements":10})
        } else if mutating {
            serde_json::from_str(&fixture.call.arguments_json).unwrap()
        } else {
            json!({"page":page,"element":element,"state":"present","timeout_ms":1000})
        };
        let call = ToolCall {
            id: name.into(),
            name: name.into(),
            arguments_json: arguments.to_string(),
        };
        if mutating {
            // Retain a real approved scope and a persisted labelled proposal.
            // No test shortcut bypasses Prepare, DispatchIntent or send claim.
            let original = agent_capability_grant::Entity::find()
                .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
                .one(&fixture.store.db)
                .await
                .unwrap()
                .unwrap();
            let mut permission: CapabilityGrant =
                serde_json::from_str(&original.payload_json).unwrap();
            permission.grant_id = "grant-runtime-open".into();
            permission.remaining_uses = 1;
            fixture.store.issue(&permission).await.unwrap();
            let row = agent_session::Entity::find()
                .one(&fixture.store.db)
                .await
                .unwrap()
                .unwrap();
            let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
            session.conversation.last_mut().unwrap().tool_calls = vec![call.to_ref()];
            let mut active: agent_session::ActiveModel = row.into();
            active.state_json = Set(session.encode_json_for_storage().unwrap());
            active.update(&fixture.store.db).await.unwrap();
        }
        let edge = async {
            let frame = socket.next().await.unwrap().unwrap();
            let awc::ws::Frame::Text(text) = frame else {
                panic!("expected dispatch");
            };
            let frame: SignalingModel = serde_json::from_slice(&text).unwrap();
            assert_eq!(frame.signaling_type, SignalingType::DispatchComputerAction);
            let wrapper: AuthorizedControlPayload<SealedComputerActionPlan> =
                frame.get_data().unwrap();
            let plan = wrapper.inner;
            let ComputerActionKind::Browser(request) = &plan.actions[0].action else {
                panic!("browser action required");
            };
            assert!(matches!(
                request.action,
                BrowserAction::TakeSnapshot { .. }
                    | BrowserAction::WaitFor { .. }
                    | BrowserAction::OpenPage { .. }
            ));
            let output: BrowserActionResult = serde_json::from_value(json!({
                "schema_version":1, "call_id":plan.action_request_id,
                "outcome":if name == "browser_take_snapshot" { "snapshot_captured" } else if mutating { "page_opened" } else { "wait_satisfied" },
                "page":page, "snapshot":{"schema_version":1,"page":page,"elements":[element],"truncated":false,"captured_at_unix_ms":now.timestamp_millis()},
                "form_readback":[], "completed_at_unix_ms":now.timestamp_millis()
            })).unwrap();
            output.validate().unwrap();
            let started = ComputerActionStarted {
                work_id: plan.work_id.clone(),
                action_request_id: plan.action_request_id.clone(),
                execution_generation: plan.execution_generation.clone(),
                disposition: ComputerActionStartDisposition::MayHaveStarted,
                executor_accepted: true,
                reason: None,
            };
            let completion = ComputerActionCompleted {
                work_id: plan.work_id,
                action_request_id: plan.action_request_id,
                execution_generation: plan.execution_generation,
                result: ComputerActionResultClass::Verified,
                facts: vec![ComputerActionStepFact {
                    index: 0,
                    changed: mutating,
                    verified: true,
                    summary: "observed".into(),
                }],
                message: None,
                output: Some(ComputerActionOutput::Browser(output.clone())),
            };
            for reply in [
                SignalingModel::success_response(
                    &frame.request_id,
                    SignalingType::ComputerActionStarted,
                    None,
                    None,
                    Some(&started),
                )
                .unwrap(),
                SignalingModel::success_response(
                    &frame.request_id,
                    SignalingType::ComputerActionCompleted,
                    None,
                    None,
                    Some(&completion),
                )
                .unwrap(),
            ] {
                socket
                    .send(awc::ws::Message::Text(
                        serde_json::to_string(&reply).unwrap().into(),
                    ))
                    .await
                    .unwrap();
            }
            output
        };
        let invoke = async {
            if !mutating {
                return tools.run_read(&call).await;
            }
            let context = ExecContext {
                assistant_turn_fence:
                    desk_diagnose_core::action_turn_fence::AssistantTurnFence::from_session(
                        &fixture.session,
                    )
                    .unwrap(),
                conversation_id: "run-1".into(),
                turn_id: "turn-1".into(),
                tool_call_id: call.id.clone(),
                actor_id: "actor-1".into(),
                policy_revision: fixture.session.policy_revision,
                scope: fixture.session.scope_snapshot.clone(),
                connection_id: None,
            };
            match tools.confirm_and_exec(&call, &context).await? {
                ExecOutcome::Executed { output, .. } => Ok(output),
                other => panic!("unexpected mutation outcome: {other:?}"),
            }
        };
        let (result, expected) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(invoke, async {
                Ok::<_, desk_agent_protocol::AgentError>(edge.await)
            })
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::from_str::<BrowserActionResult>(&result.content).unwrap(),
            expected
        );
        let work = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ActionRequestId.eq(&expected.call_id))
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.is_side_effecting, mutating);
        assert_eq!(work.status, CAPABILITY_WORK_SUCCEEDED);
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(work.id))
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outbox.computer_binding_json.is_some(), mutating);
        assert_eq!(outbox.computer_acceptance_json.is_some(), mutating);
        if let Some(json) = &outbox.computer_binding_json {
            let binding: ComputerBinding = serde_json::from_str(json).unwrap();
            assert_eq!(binding.origin.tool_call_id, call.id);
            assert_eq!(binding.connection_id, connection_id);
            assert_eq!(binding.origin.sensitivity, Sensitivity::Secret);
        }
        assert!(
            fixture
                .store
                .claim_dispatch(
                    &outbox.dispatch_id,
                    u64::try_from(now.timestamp_millis()).unwrap()
                )
                .await
                .is_err()
        );
    }
    cache.remove_connection(&connection_id);
    connections.write().await.clear();
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.stop(false))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
