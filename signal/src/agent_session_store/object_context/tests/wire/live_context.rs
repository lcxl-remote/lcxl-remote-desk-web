//! Real OSS 638/639 loopback; authentication and desktop readiness are synthetic.
use super::*;
use desk_agent_protocol::device_assistant::{
    DeviceAssistantContextUpdate, DeviceAssistantContextUpdated,
};
#[actix_web::test]
async fn live_wire_replays_first_receipts_and_returns_correlated_rejections() {
    let store = memory().await;
    let schema = Schema::new(store.db.get_database_backend());
    for table in [
        schema.create_table_from_entity(model_provider::Entity),
        schema.create_table_from_entity(model_probe_observation::Entity),
    ] {
        store.db.execute(&table).await.unwrap();
    }
    crate::model_provider::save(
        &store.db,
        crate::model_provider::ModelProviderConfig {
            wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
            model: Some("synthetic".into()),
            base_url: Some("https://example.invalid/v1".into()),
            api_key: Some("synthetic-not-a-secret".into()),
            max_context_bytes: Some(65536),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let map = actix_web::web::Data::new(SharedConnectionMap::new());
    let authorizer = Arc::new(SignalControlAuthorizer::new(store.db.clone(), map.clone()));
    let server_map = map.clone();
    let server_authorizer = authorizer.clone();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(move || {
        let map = server_map.clone();
        let authorizer = server_authorizer.clone();
        actix_web::App::new().route(
            "/controller",
            actix_web::web::get().to(
                move |request: actix_web::HttpRequest, payload: actix_web::web::Payload| {
                    let map = map.clone();
                    let authorizer = authorizer.clone();
                    async move {
                        let (response, socket, mut stream) = actix_ws::handle(&request, payload)?;
                        let actor = ConnectionState {
                            model: ConnectionModel {
                                connection_id: "controller".into(),
                                ip: None,
                                version_info: VersionInfo::new(
                                    1,
                                    1,
                                    "fixture".into(),
                                    RemoteDeskTypeEnum::Browser,
                                    None,
                                    None,
                                ),
                                device_id: None,
                                owner_node_id: None,
                            },
                            session: Arc::new(tokio::sync::RwLock::new(socket)),
                            terminal_connection_ids: Default::default(),
                            request_callback_map: Default::default(),
                            device_code: None,
                            auth_context: AuthContext::cookie(7, RemoteDeskTypeEnum::Browser),
                        };
                        // The object update is central metadata, not an edge call.
                        // A synthetic registered peer supplies the bound audience.
                        let mut target = actor.clone();
                        target.model.connection_id = "live-context-host".into();
                        target.model.version_info = VersionInfo::new(
                            1,
                            1,
                            "fixture".into(),
                            RemoteDeskTypeEnum::Server,
                            None,
                            Some("device".into()),
                        );
                        target.auth_context =
                            AuthContext::token_auth(7, 1, RemoteDeskTypeEnum::Server);
                        {
                            let mut entries = map.write().await;
                            entries.insert("live-context-host".into(), target);
                            entries.insert("controller".into(), actor.clone());
                        }
                        actix_web::rt::spawn(async move {
                            while let Some(Ok(message)) = stream.next().await {
                                match message {
                                    actix_ws::Message::Text(text) => {
                                        let frame: SignalingModel =
                                            serde_json::from_str(&text).unwrap();
                                        assert_eq!(
                                            frame.signaling_type,
                                            SignalingType::UpdateDeviceAssistantContext
                                        );
                                        assert!(matches!(
                                            authorizer.authorize(&actor, &map, &frame).await,
                                            ControlFrameOutcome::Handled
                                        ));
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
    let server_task = actix_web::rt::spawn(server);
    let client = awc::Client::default();
    let (_, mut socket) = client
        .ws(format!("http://{address}/controller"))
        .connect()
        .await
        .unwrap();
    use desk_agent_protocol::{Capability, computer_use::*};
    let now = Utc::now();
    let cache = crate::computer_use_readiness::global_computer_use_readiness_cache();
    cache
        .update(
            "live-context-host",
            ComputerUseReadiness {
                schema_version: COMPUTER_USE_SCHEMA_VERSION,
                revision: 1,
                observed_at: now.to_rfc3339(),
                expires_at: (now + Duration::seconds(60)).to_rfc3339(),
                server_api_version: desk_server_version::SERVER_API_VERSION,
                os: "macos".into(),
                interactive_session_incarnation: "live-worker".into(),
                local_ceiling_revision: 1,
                capabilities: [
                    Capability::DesktopSessionInspect,
                    Capability::DesktopUiInspect,
                ]
                .into_iter()
                .map(|capability| ComputerUseCapabilityReadiness {
                    capability,
                    adapter: ComputerUseAdapterRef {
                        kind: ComputerUseAdapterKind::MacosAccessibility,
                        version: "1".into(),
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
    let original = DeviceAssistantContextUpdate {
        conversation_id: "client-conversation".into(),
        client_request_id: "live-selection".into(),
        selected_capability_ids: vec!["desktop.ui.inspect".into()],
    };

    macro_rules! exchange {
        ($request:expr, $transport_id:expr) => {{
            let update: &DeviceAssistantContextUpdate = $request;
            let frame = SignalingModel::new(
                $transport_id,
                SignalingType::UpdateDeviceAssistantContext,
                Some("untrusted-sender-is-ignored".into()),
                Some("live-context-host".into()),
                Some(serde_json::to_value(update).unwrap()),
                None,
            );
            socket
                .send(awc::ws::Message::Text(
                    serde_json::to_string(&frame).unwrap().into(),
                ))
                .await
                .unwrap();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let awc::ws::Frame::Text(text) = reply else {
                panic!("expected response frame")
            };
            let reply: SignalingModel = serde_json::from_slice(&text).unwrap();
            assert_eq!(reply.request_id, $transport_id);
            assert_eq!(
                reply.signaling_type,
                SignalingType::DeviceAssistantContextUpdated
            );
            assert_eq!(reply.to_connection_id.as_deref(), Some("controller"));
            let ack: DeviceAssistantContextUpdated = reply.get_data().unwrap();
            assert_eq!(ack.conversation_id, update.conversation_id);
            assert_eq!(ack.client_request_id, update.client_request_id);
            ack
        }};
    }
    let first = exchange!(&original, "first-transport");
    assert!(first.changed && first.error.is_none());
    let saved = row(&store).await;
    let attachment = state(&store).await.context_attachments[0].clone();
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    let (_, new_socket) = client
        .ws(format!("http://{address}/controller"))
        .connect()
        .await
        .unwrap();
    socket = new_socket;
    model_provider::Entity::delete_many()
        .exec(&store.db)
        .await
        .unwrap();
    cache.remove_connection("live-context-host");
    let retry = exchange!(&original, "after-reconnect");
    assert!(retry.changed && retry.error.is_none());
    assert_eq!(row(&store).await, saved);

    let mut conflicting = original.clone();
    conflicting.selected_capability_ids.clear();
    let rejected = exchange!(&conflicting, "conflict");
    assert!(!rejected.changed && rejected.error.is_some());
    assert_eq!(row(&store).await, saved);

    let mut new_attach = original.clone();
    new_attach.client_request_id = "new-unconfigured-selection".into();
    let rejected = exchange!(&new_attach, "no-model");
    assert!(!rejected.changed && rejected.error.is_some());
    assert_eq!(row(&store).await, saved);

    let detach = DeviceAssistantContextUpdate {
        client_request_id: "detach".into(),
        selected_capability_ids: vec![],
        ..original.clone()
    };
    let detached = exchange!(&detach, "detach-without-model");
    assert!(detached.changed && detached.error.is_none());
    let saved = row(&store).await;
    let retry = exchange!(&detach, "detach-retry");
    assert!(retry.changed && retry.error.is_none());
    let retry = exchange!(&original, "attach-retry-after-detach");
    assert!(retry.changed && retry.error.is_none());
    assert_eq!(row(&store).await, saved);
    assert_eq!(events(&store).await.len(), 2);
    let session = state(&store).await;
    assert_eq!(session.input_revision, 0);
    assert!(session.scope_snapshot.granted.is_empty());
    assert_eq!(
        session.context_attachments[0].expires_at_unix_ms,
        attachment.expires_at_unix_ms
    );
    assert!(!session.context_attachments[0].is_active_at(Utc::now().timestamp_millis() as u64));

    // A historical receipt is not authority: the owner gate still runs first.
    let mut anonymous = map.read().await.get("controller").unwrap().clone();
    anonymous.auth_context = AuthContext::anonymous(RemoteDeskTypeEnum::Browser);
    let frame = SignalingModel::new(
        "anonymous-retry",
        SignalingType::UpdateDeviceAssistantContext,
        None,
        Some("live-context-host".into()),
        Some(serde_json::to_value(&original).unwrap()),
        None,
    );
    assert!(matches!(
        authorizer.authorize(&anonymous, &map, &frame).await,
        ControlFrameOutcome::Handled
    ));
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let awc::ws::Frame::Text(bytes) = reply else {
        panic!("expected rejection")
    };
    let frame: SignalingModel = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(frame.request_id, "anonymous-retry");
    assert_eq!(
        frame.signaling_type,
        SignalingType::DeviceAssistantContextUpdated
    );
    assert!(
        frame
            .get_data::<DeviceAssistantContextUpdated>()
            .unwrap()
            .error
            .is_some()
    );
    let malformed = SignalingModel::new(
        "malformed",
        SignalingType::UpdateDeviceAssistantContext,
        None,
        Some("live-context-host".into()),
        Some(
            serde_json::json!({"conversation_id": original.conversation_id, "client_request_id":"malformed-client", "selected_capability_ids":42}),
        ),
        None,
    );
    socket
        .send(awc::ws::Message::Text(
            serde_json::to_string(&malformed).unwrap().into(),
        ))
        .await
        .unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let awc::ws::Frame::Text(bytes) = reply else {
        panic!("expected rejection")
    };
    let frame: SignalingModel = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(frame.request_id, "malformed");
    assert_eq!(
        frame.signaling_type,
        SignalingType::DeviceAssistantContextUpdated
    );
    let ack = frame.get_data::<DeviceAssistantContextUpdated>().unwrap();
    assert_eq!(ack.client_request_id, "malformed-client");
    assert!(ack.error.is_some());
    assert_eq!(row(&store).await, saved);
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    drop(socket);
    drop(anonymous);
    map.write().await.clear();
    cache.remove_connection("live-context-host");
    handle.stop(true).await;
    server_task.await.unwrap().unwrap();
}
