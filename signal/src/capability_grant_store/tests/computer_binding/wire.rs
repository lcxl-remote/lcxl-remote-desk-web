//! Loopback transport exercises the production observer with synthetic peers.

use super::*;
use crate::remote_tool_edge::{SignalComputerActionObserver, SignalComputerActionPendingStore};
use desk_signal_facade::model::peer::{
    HostExecCapabilities, PeerInfo, ServerRegistration, SignalingPeer,
};
use desk_signal_facade::{
    model::{
        auth_context::AuthContext,
        connection::{ConnectionModel, ConnectionState},
        signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
        version::VersionInfo,
    },
    service::ComputerActionObserver,
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};

mod reads;

#[actix_web::test]
async fn real_socket_observer_rejects_wrong_subject_and_frame_then_commits_original_acceptance_and_completion()
 {
    let directory = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(file_db(&directory.path().join("wire.db")).await).await;
    fixture.bind().await;
    let observer = Arc::new(SignalComputerActionObserver::new(
        Arc::new(SignalComputerActionPendingStore::default()),
        fixture.store.db.clone(),
    ));
    let observed = Arc::new(tokio::sync::Notify::new());
    let notify = observed.clone();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let server = actix_web::HttpServer::new(move || {
        let observer = observer.clone();
        let notify = notify.clone();
        actix_web::App::new().route(
            "/{identity}",
            actix_web::web::get().to(
                move |request: actix_web::HttpRequest, payload: actix_web::web::Payload| {
                    let observer = observer.clone();
                    let notify = notify.clone();
                    async move {
                        let (response, socket, mut stream) = actix_ws::handle(&request, payload)?;
                        // Fixture identities stand in for the endpoint's validated
                        // token/cookie resolution, never production request fields.
                        let identity = request.match_info().get("identity").unwrap();
                        let auth_context = match identity {
                            "cookie" => AuthContext::cookie(1, RemoteDeskTypeEnum::Server),
                            "browser" => AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Browser),
                            _ => AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
                        };
                        let state = {
                            let (client_id, model) =
                                (SignalingPeer {
                                    version_info: Some(VersionInfo {
                                        api_version: 1,
                                        build_number: 1,
                                        commit_hash: "fixture".into(),
                                        debug_build: cfg!(debug_assertions),
                                    }),
                                    peer: PeerInfo {
                                        remote_desk_type: RemoteDeskTypeEnum::Server,
                                        operation_system: Default::default(),
                                        display_name: None,
                                    },
                                    server: (Some(
                                        if identity == "audience" {
                                            "other-device"
                                        } else {
                                            "device-1"
                                        }
                                        .into(),
                                    ))
                                    .map(|client_id| ServerRegistration {
                                        client_id,
                                        exec_capabilities: HostExecCapabilities {
                                            available_exec_shells: Vec::new(),
                                            exec_pty: false,
                                            exec_pty_elevation: false,
                                            max_ai_command_runtime_ms: None,
                                        },
                                        ai_assistant_settings:
                                            desk_agent_protocol::ai_assistant::AiAssistantSettings {
                                                revision: 0,
                                                enabled: false,
                                            },
                                    }),
                                })
                                .into_connection_model(
                                    if identity == "reconnected" {
                                        "other-host"
                                    } else {
                                        "host-original"
                                    }
                                    .into(),
                                    None,
                                );
                            ConnectionState {
                                lifecycle: Default::default(),
                                client_id,
                                model: (ConnectionModel {
                                    device_id: None,
                                    owner_node_id: None,
                                    ..model
                                })
                                .into(),
                                session: Arc::new(tokio::sync::RwLock::new(socket)),
                                terminal_connection_ids: Default::default(),
                                request_callback_map: Default::default(),
                                device_code: None,
                                auth_context,
                            }
                        };
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
    let task = actix_web::rt::spawn(server);
    let mut frame = SignalingModel::new_request(
        SignalingType::ComputerActionStarted,
        None,
        Some(&fixture.started),
    )
    .unwrap();
    frame.request_id = fixture.plan.execution_generation.clone();
    for identity in ["cookie", "browser", "reconnected", "audience", "original"] {
        let (_, mut socket) = awc::Client::default()
            .ws(format!("http://{address}/{identity}"))
            .connect()
            .await
            .unwrap();
        let mut frames = vec![frame.clone()];
        if identity == "original" {
            let mut wrong_frame = frame.clone();
            wrong_frame.request_id = "wrong-frame".into();
            let mut peer_routed = frame.clone();
            peer_routed.to_connection_id = Some("browser".into());
            frames = vec![wrong_frame, peer_routed, frame.clone(), frame.clone()];
        }
        for (index, item) in frames.into_iter().enumerate() {
            let before = fixture.outbox().await;
            socket
                .send(awc::ws::Message::Text(
                    serde_json::to_string(&item).unwrap().into(),
                ))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), observed.notified())
                .await
                .unwrap();
            let after = fixture.outbox().await;
            if identity == "original" && index >= 2 {
                assert!(after.computer_acceptance_json.is_some());
                if index == 3 {
                    assert_eq!(after, before);
                }
            } else {
                assert_eq!(after, before);
                assert!(after.computer_acceptance_json.is_none());
            }
        }
        socket.send(awc::ws::Message::Close(None)).await.unwrap();
    }
    // There is deliberately no pending foreground waiter in this observer.
    // Only the authenticated original host may persist a late completion.
    for identity in ["cookie", "browser", "reconnected", "audience", "original"] {
        let (_, mut socket) = awc::Client::default()
            .ws(format!("http://{address}/{identity}"))
            .connect()
            .await
            .unwrap();
        let native = super::completion::failed(&fixture.plan);
        let mut frame = SignalingModel::new_request(
            SignalingType::ComputerActionCompleted,
            None,
            Some(&native),
        )
        .unwrap();
        frame.request_id = fixture.plan.execution_generation.clone();
        let mut wrong = frame.clone();
        wrong.request_id = "wrong-frame".into();
        let mut routed = frame.clone();
        routed.to_connection_id = Some("browser".into());
        for (index, message) in [wrong, routed, frame.clone(), frame]
            .into_iter()
            .enumerate()
        {
            let before = fixture.outbox().await;
            socket
                .send(awc::ws::Message::Text(
                    serde_json::to_string(&message).unwrap().into(),
                ))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), observed.notified())
                .await
                .unwrap();
            let after = fixture.outbox().await;
            if identity == "original" && index >= 2 {
                let result = fixture
                    .store
                    .read_computer_result(
                        &fixture.plan.execution_generation,
                        "run-1",
                        "actor-1",
                        "device-1",
                    )
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(result.outcome, CapabilityDispatchOutcome::Failed);
                assert_eq!(result.original_call_id, fixture.call.id);
                assert_eq!(result.work.completion_delivery_state, "pending");
                if index == 3 {
                    assert_eq!(after, before);
                }
            } else {
                assert_eq!(after, before);
            }
        }
        socket.send(awc::ws::Message::Close(None)).await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), handle.stop(false))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture.accept().await.unwrap(),
        AcceptanceOutcome::Duplicate
    );
}
