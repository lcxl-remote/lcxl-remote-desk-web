//! Real loopback transport through the OSS authorizer and object orchestrator.
//! Cookie validation and device reference issuance use synthetic fixture inputs.

use super::*;
use crate::{
    control_authorizer::SignalControlAuthorizer,
    entity::{model_probe_observation, model_provider},
};
use desk_agent_protocol::device_assistant::DeviceAssistantObjectContextUpdated;
use desk_diagnose_core::model_profile::WireProtocol;
use desk_signal_facade::{
    model::{
        auth_context::AuthContext,
        connection::{ConnectionModel, ConnectionState, SharedConnectionMap},
        signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
        version::VersionInfo,
    },
    service::{ControlFrameAuthorizer, ControlFrameOutcome},
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

#[actix_web::test]
async fn object_wire_replays_first_receipts_after_reconnect_and_model_removal() {
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
                        target.model.connection_id = "host".into();
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
                            entries.insert("host".into(), target);
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
                                            SignalingType::UpdateDeviceAssistantObjectContext
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
    let original = params();

    macro_rules! exchange {
        ($request:expr, $transport_id:expr) => {{
            let update: &DeviceAssistantObjectContextUpdate = $request;
            let frame = SignalingModel::new(
                $transport_id,
                SignalingType::UpdateDeviceAssistantObjectContext,
                Some("untrusted-sender-is-ignored".into()),
                Some("host".into()),
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
                SignalingType::DeviceAssistantObjectContextUpdated
            );
            assert_eq!(reply.to_connection_id.as_deref(), Some("controller"));
            let ack: DeviceAssistantObjectContextUpdated = reply.get_data().unwrap();
            assert_eq!(ack.conversation_id, update.conversation_id);
            assert_eq!(ack.client_request_id, update.client_request_id);
            ack
        }};
    }
    let first = exchange!(&original.update, "first-transport");
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
    let retry = exchange!(&original.update, "after-reconnect");
    assert!(retry.changed && retry.error.is_none());
    assert_eq!(row(&store).await, saved);

    let mut conflicting = original.update.clone();
    if let DeviceAssistantObjectContextOperation::AttachFile {
        display_summary, ..
    } = &mut conflicting.operation
    {
        *display_summary = "changed-body".into();
    }
    let rejected = exchange!(&conflicting, "conflict");
    assert!(!rejected.changed && rejected.error.is_some());
    assert_eq!(row(&store).await, saved);

    let mut new_attach = original.update.clone();
    new_attach.client_request_id = "new-unconfigured-selection".into();
    let rejected = exchange!(&new_attach, "no-model");
    assert!(!rejected.changed && rejected.error.is_some());
    assert_eq!(row(&store).await, saved);

    let detach = DeviceAssistantObjectContextUpdate {
        client_request_id: "detach".into(),
        operation: DeviceAssistantObjectContextOperation::Detach {
            attachment_id: attachment.attachment_id.clone(),
        },
        ..original.update.clone()
    };
    let detached = exchange!(&detach, "detach-without-model");
    assert!(detached.changed && detached.error.is_none());
    let saved = row(&store).await;
    let retry = exchange!(&detach, "detach-retry");
    assert!(retry.changed && retry.error.is_none());
    let retry = exchange!(&original.update, "attach-retry-after-detach");
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
        SignalingType::UpdateDeviceAssistantObjectContext,
        None,
        Some("host".into()),
        Some(serde_json::to_value(&original.update).unwrap()),
        None,
    );
    assert!(matches!(
        authorizer.authorize(&anonymous, &map, &frame).await,
        ControlFrameOutcome::Reject { .. }
    ));
    assert_eq!(row(&store).await, saved);
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    drop(socket);
    drop(anonymous);
    map.write().await.clear();
    handle.stop(true).await;
    server_task.await.unwrap().unwrap();
}
