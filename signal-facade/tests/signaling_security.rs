use actix_web::{
    FromRequest, HttpResponse, body::MessageBody, middleware::Logger, test::TestRequest, web,
};
use desk_signal_facade::model::peer::{PeerInfo, SignalingPeer};
use desk_signal_facade::{
    error::DeskSignalFacadeError,
    model::{
        auth_context::AuthContext,
        connection::{
            ConnectionModel, ConnectionState, FetchConnectionsScope, SharedConnectionMap,
        },
        signal::{
            ForwardSignalingSender, RemoteDeskTypeEnum, SignalingModel, SignalingType,
            SignalingUser,
        },
        version::VersionInfo,
    },
    service::{CredentialPolicy, FetchConnectionsResolver, SignalingHandler},
};
use desk_utils::error::DeskErrorCode;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
};

struct User;
impl SignalingUser for User {
    fn get_access(&self) -> Option<&str> {
        Some("device_user")
    }
    fn get_target_connection_id(&self) -> Option<&str> {
        Some("host-a")
    }
}

async fn websocket() -> (HttpResponse, actix_ws::Session) {
    let req = TestRequest::get()
        .insert_header(("upgrade", "websocket"))
        .insert_header(("connection", "upgrade"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let payload = web::Payload::from_request(&req, &mut actix_web::dev::Payload::None)
        .await
        .unwrap();
    let (response, session, _) = actix_ws::handle(&req, payload).unwrap();
    (response, session)
}

async fn frame(body: &mut actix_web::body::BoxBody) -> SignalingModel {
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        futures_util::future::poll_fn(|cx| Pin::new(&mut *body).poll_next(cx)),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(bytes[0] & 0xf, 1);
    let offset = match bytes[1] & 0x7f {
        126 => 4,
        127 => 10,
        _ => 2,
    };
    serde_json::from_slice(&bytes[offset..]).unwrap()
}

async fn handler(
    id: &str,
    map: web::Data<SharedConnectionMap>,
) -> (HttpResponse, SignalingHandler<User>) {
    let (response, session) = websocket().await;
    let version = SignalingPeer {
        version_info: Some(VersionInfo {
            api_version: 1,
            build_number: 0,
            commit_hash: String::new(),
            debug_build: cfg!(debug_assertions),
        }),
        peer: PeerInfo {
            remote_desk_type: RemoteDeskTypeEnum::Browser,
            operation_system: Default::default(),
            display_name: None,
        },
        server: None,
    };
    let handler = SignalingHandler::init(
        id.into(),
        version,
        map,
        session,
        User,
        None,
        None,
        None,
        AuthContext::code_session("principal-a"),
        CredentialPolicy::Plain,
        1,
        None,
    )
    .await
    .unwrap();
    (response, handler)
}

struct Deny;
impl FetchConnectionsResolver for Deny {
    fn resolve<'a>(
        &'a self,
        _: &'a ConnectionState,
        _: FetchConnectionsScope,
    ) -> Pin<
        Box<dyn Future<Output = Result<Vec<ConnectionModel>, DeskSignalFacadeError>> + Send + 'a>,
    > {
        Box::pin(async {
            DeskSignalFacadeError::custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "sentinel-internal-error",
            )
        })
    }
}

#[actix_web::test]
async fn credential_consumption_and_connection_failure_frames() {
    let map = web::Data::new(SharedConnectionMap::new());
    let (response, mut handler) = handler("browser", map.clone()).await;
    for model in [
        handler.connection_state.public_model(),
        map.read().await["browser"].public_model(),
    ] {
        let value = serde_json::to_value(model).unwrap();
        assert!(value.get("token").is_none());
        assert!(value["version_info"].get("token").is_none());
        assert!(value["version_info"].get("client_id").is_none());
    }
    let mut body = response.into_body();

    // Auxiliary sockets have no resolver: deny with the declared response, not
    // a success-shaped empty list or a callback timeout.
    for resolver in [
        None,
        Some(Arc::new(Deny) as Arc<dyn FetchConnectionsResolver>),
    ] {
        handler.fetch_connections_resolver = resolver;
        let request =
            SignalingModel::new_request::<()>(SignalingType::FetchConnections, None, None).unwrap();
        handler
            .handle_message(serde_json::to_string(&request).unwrap().into())
            .await
            .unwrap();
        let response = frame(&mut body).await;
        assert_eq!(response.request_id, request.request_id);
        assert_eq!(response.signaling_type, SignalingType::ConnectionsFetched);
        assert_eq!(
            response.response_state.as_ref().unwrap().error_code,
            DeskErrorCode::PERMISSION_ERROR.code()
        );
        assert!(response.get_raw_data().is_none());
    }

    // Sending an existing failed response preserves state, data and correlation.
    let response = SignalingModel::new(
        "correlation",
        SignalingType::ConnectionsFetched,
        None,
        None,
        Some(serde_json::json!({"detail": 7})),
        Some(desk_signal_facade::model::signal::SignalingResponseState {
            error_code: DeskErrorCode::INVALID_PARAMS.code(),
            message: Some("safe".into()),
        }),
    );
    handler
        .connection_state
        .send_response(Some("central".into()), &response)
        .await
        .unwrap();
    let delivered = frame(&mut body).await;
    assert_eq!(delivered.request_id, "correlation");
    assert_eq!(delivered.from_connection_id.as_deref(), Some("central"));
    assert_eq!(delivered.to_connection_id.as_deref(), Some("browser"));
    assert_eq!(delivered.get_raw_data(), response.get_raw_data());
    assert_eq!(
        delivered
            .response_state
            .as_ref()
            .unwrap()
            .message
            .as_deref(),
        Some("safe")
    );
    assert_eq!(
        delivered.response_state.unwrap().error_code,
        DeskErrorCode::INVALID_PARAMS.code()
    );
}

static LOGS: LogCapture = LogCapture(Mutex::new(Vec::new()));
struct LogCapture(Mutex<Vec<String>>);
impl log::Log for LogCapture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        self.0.lock().unwrap().push(record.args().to_string());
    }
    fn flush(&self) {}
}

#[actix_web::test]
async fn logs_omit_credentials_headers_frames_and_connection_models() {
    log::set_logger(&LOGS).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let app = actix_web::test::init_service(
        actix_web::App::new()
            .wrap(Logger::new(
                desk_signal_facade::http_log::REQUEST_LOG_FORMAT,
            ))
            .route(
                "/api/desk/signaling",
                web::get().to(|| async { HttpResponse::Unauthorized().finish() }),
            ),
    )
    .await;
    let req = TestRequest::get()
        .uri("/api/desk/signaling?token=sentinel-access-secret&probe=1")
        .insert_header((
            "referer",
            "https://example.test/?token=sentinel-header-secret",
        ))
        .to_request();
    let response = actix_web::test::call_service(&app, req).await;
    actix_web::test::read_body(response).await;
    let (response, mut handler) =
        handler("log-browser", web::Data::new(SharedConnectionMap::new())).await;
    let error = SignalingModel::new(
        "log-error",
        SignalingType::Error,
        None,
        None,
        Some(serde_json::json!({"secret": "sentinel-frame-secret"})),
        None,
    );
    handler
        .handle_message(serde_json::to_string(&error).unwrap().into())
        .await
        .unwrap();
    let _ = handler
        .handle_message("sentinel-invalid-json-secret".into())
        .await;
    drop(response);
    drop(handler);
    let logs = LOGS.0.lock().unwrap().join("\n");
    assert!(
        logs.contains("/api/desk/signaling"),
        "must capture the real access logger"
    );
    assert!(
        logs.contains("Received client error frame"),
        "must exercise the real handler logger"
    );
    for secret in [
        "sentinel-access-secret",
        "sentinel-header-secret",
        "sentinel-node-secret",
        "sentinel-frame-secret",
        "sentinel-invalid-json-secret",
    ] {
        assert!(!logs.contains(secret), "secret leaked: {secret}");
    }
}

#[actix_web::test]
async fn host_public_handle_is_selected_before_connection_publication() {
    use desk_signal_facade::model::peer::PeerQuery;
    let map = web::Data::new(SharedConnectionMap::new());
    for public_handle in [None, Some("manager-public-device".to_string())] {
        let (_, session) = websocket().await;
        let query: PeerQuery = serde_urlencoded::from_str("remote_desk_type=server&client_id=private-audience&available_exec_shells=sh&exec_pty=false&exec_pty_elevation=false&max_ai_command_runtime_ms=60000&ai_assistant_revision=3&ai_assistant_enabled=true").unwrap();
        let peer = query
            .into_peer(
                Some(VersionInfo {
                    api_version: 1,
                    build_number: 2,
                    commit_hash: "source".into(),
                    debug_build: false,
                }),
                RemoteDeskTypeEnum::Server,
                None,
            )
            .unwrap();
        let expected = public_handle
            .as_deref()
            .unwrap_or("private-audience")
            .to_owned();
        let handler = SignalingHandler::init(
            "host".into(),
            peer,
            map.clone(),
            session,
            User,
            None,
            None,
            None,
            AuthContext::token_auth(7, 1, RemoteDeskTypeEnum::Server),
            CredentialPolicy::Plain,
            1,
            public_handle,
        )
        .await
        .unwrap();
        let forwarded_state = handler.connection_state.clone();
        assert!(Arc::ptr_eq(
            &forwarded_state.model,
            &handler.connection_state.model
        ));
        assert_eq!(
            handler.connection_state.client_id.as_deref(),
            Some("private-audience")
        );
        assert_eq!(
            map.read().await["host"].model.device_id.as_deref(),
            Some(expected.as_str())
        );
        let public = serde_json::to_value(handler.connection_state.public_model()).unwrap();
        assert!(public.get("client_id").is_none());
        assert!(public["version_info"].get("client_id").is_none());
    }
}
