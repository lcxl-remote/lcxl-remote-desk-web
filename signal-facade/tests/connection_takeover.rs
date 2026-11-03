use actix_web::{FromRequest, HttpResponse, test::TestRequest, web};
use desk_signal_facade::{
    model::{
        auth_context::AuthContext,
        connection::SharedConnectionMap,
        peer::PeerQuery,
        signal::{
            ForwardSignalingSender, RemoteDeskTypeEnum, SignalingModel, SignalingType,
            SignalingUser,
        },
        version::VersionInfo,
    },
    service::{CredentialPolicy, SignalingHandler},
};
use std::time::Duration;

struct Owner;
impl SignalingUser for Owner {
    fn get_access(&self) -> Option<&str> {
        None
    }
    fn get_target_connection_id(&self) -> Option<&str> {
        None
    }
}

async fn connect(
    id: &str,
    client: &str,
    map: web::Data<SharedConnectionMap>,
) -> (HttpResponse, SignalingHandler<Owner>) {
    let (response, handler, _) = connect_with_stream(id, client, map).await;
    (response, handler)
}

async fn connect_with_stream(
    id: &str,
    client: &str,
    map: web::Data<SharedConnectionMap>,
) -> (
    HttpResponse,
    SignalingHandler<Owner>,
    actix_ws::AggregatedMessageStream,
) {
    let req = TestRequest::get()
        .insert_header(("upgrade", "websocket"))
        .insert_header(("connection", "upgrade"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let payload = web::Payload::from_request(&req, &mut actix_web::dev::Payload::None)
        .await
        .unwrap();
    let (response, session, stream) = actix_ws::handle(&req, payload).unwrap();
    let query: PeerQuery = serde_urlencoded::from_str(&format!("remote_desk_type=server&client_id={client}&available_exec_shells=&exec_pty=false&exec_pty_elevation=false&max_ai_command_runtime_ms=60000&ai_assistant_revision=0&ai_assistant_enabled=false")).unwrap();
    let peer = query
        .into_peer(
            Some(VersionInfo {
                api_version: 1,
                build_number: 1,
                commit_hash: String::new(),
                debug_build: false,
            }),
            RemoteDeskTypeEnum::Server,
            None,
        )
        .unwrap();
    let handler = SignalingHandler::init(
        id.into(),
        peer,
        map,
        session,
        Owner,
        None,
        None,
        None,
        AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        CredentialPolicy::Plain,
        1,
        None,
    )
    .await
    .unwrap();
    (response, handler, stream.aggregate_continuations())
}

#[actix_web::test]
async fn takeover_publishes_before_blocked_close_and_fences_every_old_snapshot() {
    let map = web::Data::new(SharedConnectionMap::new_oss());
    let (_body1, mut old) = connect("c1", "device", map.clone()).await;
    let snapshot = old.connection_state.clone();
    let transport = snapshot.session.write().await;
    let (_body2, new) = tokio::time::timeout(
        Duration::from_millis(100),
        connect("c2", "device", map.clone()),
    )
    .await
    .unwrap();
    assert!(snapshot.lifecycle.is_retired());
    assert_eq!(map.read().await.len(), 1);
    assert_eq!(
        map.read()
            .await
            .current_oss_server("device")
            .unwrap()
            .model
            .connection_id,
        "c2"
    );
    let request =
        SignalingModel::new_request::<()>(SignalingType::FetchConnections, None, None).unwrap();
    assert!(
        old.handle_message(serde_json::to_string(&request).unwrap().into())
            .await
            .is_err()
    );
    assert!(old.binary(bytes::Bytes::from_static(b"old")).await.is_err());
    assert!(snapshot.send_to_peer("browser", &request).await.is_err());
    assert!(
        snapshot
            .send_binary(bytes::Bytes::from_static(b"old"))
            .await
            .is_err()
    );
    drop(old);
    drop(transport);
    tokio::task::yield_now().await;
    assert!(!new.connection_state.lifecycle.is_retired());
    assert_eq!(
        map.read()
            .await
            .current_oss_server("device")
            .unwrap()
            .model
            .connection_id,
        "c2"
    );

    let app = actix_web::test::init_service(
        actix_web::App::new()
            .app_data(map.clone())
            .service(desk_signal_facade::controller::connection::get_device_connection),
    )
    .await;
    for (target, code) in [("device", 0), ("unknown", 20)] {
        let response = actix_web::test::call_service(
            &app,
            TestRequest::get()
                .uri(&format!("/devices/{target}/connection"))
                .to_request(),
        )
        .await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        let body: serde_json::Value = actix_web::test::read_body_json(response).await;
        assert_eq!(body["code"], code);
        if code == 0 {
            assert_eq!(body["data"]["connection_id"], "c2");
        }
    }
}

#[actix_web::test]
async fn concurrent_publication_and_repeated_cleanup_keep_the_index_consistent() {
    for count in [16, 64] {
        let map = web::Data::new(SharedConnectionMap::new_oss());
        let connections = futures_util::future::join_all((0..count).map(|n| {
            let map = map.clone();
            async move { connect(&format!("c{n}"), "device", map).await }
        }))
        .await;
        let current = map
            .read()
            .await
            .current_oss_server("device")
            .unwrap()
            .model
            .connection_id
            .clone();
        assert_eq!(map.read().await.len(), 1);
        for (_, handler) in &connections {
            assert_eq!(
                handler.connection_state.lifecycle.is_retired(),
                handler.connection_state.model.connection_id != current
            );
        }
        drop(connections);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !map.read().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(map.read().await.current_oss_server("device").is_none());
    }
}

#[actix_web::test]
async fn manager_table_does_not_install_a_local_device_authority() {
    let map = web::Data::new(SharedConnectionMap::new());
    let (_body1, first) = connect("c1", "device", map.clone()).await;
    let (_body2, second) = connect("c2", "device", map.clone()).await;
    assert_eq!(map.read().await.len(), 2);
    assert!(!first.connection_state.lifecycle.is_retired());
    assert!(!second.connection_state.lifecycle.is_retired());
    assert!(map.read().await.current_oss_server("device").is_none());
}

#[actix_web::test]
async fn cancellation_during_cleanup_allows_drop_to_finish_removal_and_drain_waiters() {
    use desk_signal_facade::model::connection::PendingRequestCallback;
    let map = web::Data::new(SharedConnectionMap::new_oss());
    let (_response, mut handler, stream) = connect_with_stream("c1", "device", map.clone()).await;
    let lifecycle = handler.connection_state.lifecycle.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    handler
        .connection_state
        .request_callback_map
        .write()
        .await
        .insert(
            "pending".into(),
            PendingRequestCallback::new(&[SignalingType::ConnectionsFetched], sender),
        );
    let lock = map.write().await;
    let task = actix_web::rt::spawn(async move { handler.do_handle_signaling(stream).await });
    tokio::time::timeout(Duration::from_secs(1), lifecycle.retired())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(lock);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !map.read().await.is_empty() {
            tokio::task::yield_now().await;
        }
        assert!(receiver.await.is_err());
    })
    .await
    .unwrap();
    assert!(map.read().await.current_oss_server("device").is_none());
}

#[actix_web::test]
async fn a_thousand_reconnects_release_old_callbacks_and_identity_entries() {
    use desk_signal_facade::model::connection::PendingRequestCallback;
    let map = web::Data::new(SharedConnectionMap::new_oss());
    for round in 0..1_000 {
        let client = format!("device-{}", round % 16);
        let (_old_response, old) = connect(&format!("old-{round}"), &client, map.clone()).await;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        old.connection_state
            .request_callback_map
            .write()
            .await
            .insert(
                "pending".into(),
                PendingRequestCallback::new(&[SignalingType::ConnectionsFetched], sender),
            );
        let (_new_response, new) = connect(&format!("new-{round}"), &client, map.clone()).await;
        assert!(old.connection_state.lifecycle.is_retired());
        assert_eq!(map.read().await.len(), 1);
        assert_eq!(
            map.read()
                .await
                .current_oss_server(&client)
                .unwrap()
                .model
                .connection_id,
            format!("new-{round}")
        );
        tokio::time::timeout(Duration::from_secs(1), receiver)
            .await
            .unwrap()
            .unwrap_err();
        drop((old, new));
        tokio::time::timeout(Duration::from_secs(1), async {
            while !map.read().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(map.read().await.current_oss_server(&client).is_none());
    }
    for n in 0..16 {
        assert!(
            map.read()
                .await
                .current_oss_server(&format!("device-{n}"))
                .is_none()
        );
    }
}
