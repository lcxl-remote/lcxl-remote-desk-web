//! Real loopback sockets with synthetic hosts; no desktop or model side effects.
use super::*;
use crate::{
    computer_cancel_dispatch::SignalComputerCancelDispatcher,
    remote_tool_edge::{SignalComputerActionObserver, SignalComputerActionPendingStore},
};
use desk_agent_protocol::{authz::AuthorizedControlPayload, computer_use::ComputerActionCancel};
use desk_signal_facade::{
    model::{
        auth_context::AuthContext,
        connection::{ConnectionModel, ConnectionState, SharedConnectionMap},
        signal::{RemoteDeskTypeEnum, SignalingModel, SignalingResponseState, SignalingType},
        version::VersionInfo,
    },
    service::ComputerActionObserver,
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};

#[actix_web::test]
async fn original_stop_socket_retries_only_stop_after_ack_loss_and_rejects_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stop-wire.db");
    let f = Fixture::new_for_actor(file_db(&path).await, "1").await;
    ready(&f).await;
    request(&f, "owner-stop-1", "private owner reason")
        .await
        .unwrap();
    let id = work(&f).await.id;
    let connections = Arc::new(SharedConnectionMap::new());
    let observer = Arc::new(SignalComputerActionObserver::new(
        Arc::new(SignalComputerActionPendingStore::default()),
        f.store.db.clone(),
    ));
    let observed = Arc::new(tokio::sync::Notify::new());
    let connected = Arc::new(tokio::sync::Notify::new());
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let map = connections.clone();
    let notify = observed.clone();
    let connect = connected.clone();
    let server = actix_web::HttpServer::new(move || {
        let map = map.clone();
        let observer = observer.clone();
        let notify = notify.clone();
        let connect = connect.clone();
        actix_web::App::new().route(
            "/edge",
            actix_web::web::get().to(
                move |req: actix_web::HttpRequest, payload: actix_web::web::Payload| {
                    let map = map.clone();
                    let observer = observer.clone();
                    let notify = notify.clone();
                    let connect = connect.clone();
                    async move {
                        let (response, socket, mut stream) = actix_ws::handle(&req, payload)?;
                        let state = ConnectionState {
                            model: ConnectionModel {
                                connection_id: "host-original".into(),
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
                        map.write()
                            .await
                            .insert("host-original".into(), state.clone());
                        connect.notify_one();
                        actix_web::rt::spawn(async move {
                            while let Some(Ok(message)) = stream.next().await {
                                match message {
                                    actix_ws::Message::Text(text) => {
                                        let frame: SignalingModel =
                                            serde_json::from_str(&text).unwrap();
                                        observer.on_computer_action_lifecycle(&state, &frame).await;
                                        notify.notify_one();
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
    let server_task = actix_web::rt::spawn(server);
    let (_, mut socket) = awc::Client::default()
        .ws(format!("http://{address}/edge"))
        .connect()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), connected.notified())
        .await
        .unwrap();

    let dispatcher = SignalComputerCancelDispatcher::new(f.store.db.clone(), connections.clone());
    let original = connections.write().await.remove("host-original").unwrap();
    assert!(!dispatcher.send_original(id).await.unwrap());
    let mut replacement = original.clone();
    replacement.model.connection_id = "new-host".into();
    connections
        .write()
        .await
        .insert("new-host".into(), replacement);
    assert!(!dispatcher.send_original(id).await.unwrap());
    connections.write().await.remove("new-host");
    for bad in ["cookie", "browser", "audience", "connection"] {
        let mut target = original.clone();
        match bad {
            "cookie" => target.auth_context = AuthContext::cookie(1, RemoteDeskTypeEnum::Server),
            "browser" => {
                target.auth_context = AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Browser)
            }
            "audience" => target.model.version_info.client_id = Some("other".into()),
            _ => target.model.connection_id = "other".into(),
        }
        connections
            .write()
            .await
            .insert("host-original".into(), target);
        assert!(dispatcher.send_original(id).await.is_err());
    }
    connections
        .write()
        .await
        .insert("host-original".into(), original.clone());
    // Neither an expired grant nor missing readiness revokes stop authority.
    let grant = agent_capability_grant::Entity::find()
        .one(&f.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut revoked: agent_capability_grant::ActiveModel = grant.into();
    revoked.status = Set(GRANT_STATUS_REVOKED.into());
    revoked.update(&f.store.db).await.unwrap();
    assert_eq!(dispatcher.scan_once(0).await.unwrap(), (None, 1));
    let awc::ws::Frame::Text(first) = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    else {
        panic!("stop frame required")
    };
    let first: SignalingModel = serde_json::from_slice(&first).unwrap();
    assert_eq!(first.signaling_type, SignalingType::CancelComputerAction);
    let wrapper: AuthorizedControlPayload<ComputerActionCancel> = first.get_data().unwrap();
    assert_eq!(wrapper.authz.actor.user_id, Some(1));
    assert_eq!(wrapper.authz.issuer, "signal");
    assert_eq!(wrapper.authz.audience, "device-1");
    assert!(wrapper.authz.scope.granted.is_empty() && wrapper.authz.orchestrator_grants.is_empty());
    assert_eq!(
        wrapper.inner.execution_generation,
        f.plan.execution_generation
    );
    assert_eq!(wrapper.inner.action_request_id, f.plan.action_request_id);
    assert!(
        !serde_json::to_string(&first)
            .unwrap()
            .contains("private owner reason")
    );
    assert!(
        f.store
            .computer_cancel_candidate(id)
            .await
            .unwrap()
            .is_some()
    );
    drop(dispatcher);
    // A reopened SQLite connection and new dispatcher have no delivery memory.
    let dispatcher = SignalComputerCancelDispatcher::new(reopen(&path).await, connections.clone());
    assert_eq!(dispatcher.scan_once(0).await.unwrap(), (None, 1));
    let awc::ws::Frame::Text(retry) = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    else {
        panic!("stop retry required")
    };
    let retry: SignalingModel = serde_json::from_slice(&retry).unwrap();
    assert_eq!(retry.signaling_type, SignalingType::CancelComputerAction);
    assert_eq!(retry.request_id, first.request_id);
    let repeated: AuthorizedControlPayload<ComputerActionCancel> = retry.get_data().unwrap();
    assert_eq!(repeated.inner, wrapper.inner);
    let ack = SignalingModel::success_response(
        &first.request_id,
        SignalingType::ComputerActionStateReported,
        None,
        None,
        Some(&state(&f)),
    )
    .unwrap();
    let mut error_ack = ack.clone();
    error_ack.response_state = Some(SignalingResponseState {
        error_code: 8,
        message: None,
    });
    let mut routed = ack.clone();
    routed.to_connection_id = Some("another-peer".into());
    let mut wrong_request = ack.clone();
    wrong_request.request_id = "other".into();
    for (index, frame) in [error_ack, routed, wrong_request, ack.clone(), ack]
        .into_iter()
        .enumerate()
    {
        socket
            .send(awc::ws::Message::Text(
                serde_json::to_string(&frame).unwrap().into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), observed.notified())
            .await
            .unwrap();
        assert_eq!(
            f.store
                .computer_cancel_candidate(id)
                .await
                .unwrap()
                .is_none(),
            index >= 3
        );
    }
    assert_eq!(dispatcher.scan_once(0).await.unwrap(), (None, 0));
    assert_eq!(work(&f).await.result_json, None);
    let native = super::super::completion::verified(&f.plan);
    let complete = SignalingModel::success_response(
        &f.plan.execution_generation,
        SignalingType::ComputerActionCompleted,
        None,
        None,
        Some(&native),
    )
    .unwrap();
    socket
        .send(awc::ws::Message::Text(
            serde_json::to_string(&complete).unwrap().into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), observed.notified())
        .await
        .unwrap();
    assert_eq!(
        request(&f, "owner-stop-1", "private owner reason")
            .await
            .unwrap()
            .unwrap()
            .state,
        BackgroundTaskState::Succeeded
    );
    assert_eq!(dispatcher.scan_once(0).await.unwrap(), (None, 0));
    assert_eq!(
        agent_action_item::Entity::find()
            .count(&f.store.db)
            .await
            .unwrap(),
        1
    );
    socket.send(awc::ws::Message::Close(None)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.stop(false))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
