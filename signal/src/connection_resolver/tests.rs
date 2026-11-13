use super::*;
use actix_web::{FromRequest, HttpResponse, body::MessageBody, test::TestRequest};
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;
use desk_server_user::model::CurrentUser;
use desk_signal_facade::model::peer::{
    HostExecCapabilities, PeerInfo, ServerRegistration, SignalingPeer,
};
use desk_signal_facade::{
    grant::{GrantClock, GrantPrincipal, GrantSessionRecord, InProcessAccessGrantStore},
    model::{
        auth_context::AuthContext,
        security_settings::SecuritySettings,
        signal::{SignalingModel, SignalingType},
        version::VersionInfo,
    },
    service::{CredentialPolicy, SignalingHandler},
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, Schema};
use std::{
    pin::Pin,
    sync::atomic::{AtomicI64, Ordering},
};

struct Clock(Arc<AtomicI64>);
impl GrantClock for Clock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
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

async fn make_handler(
    id: &str,
    auth: AuthContext,
    map: web::Data<SharedConnectionMap>,
) -> (HttpResponse, SignalingHandler<CurrentUser>) {
    let (response, session) = websocket().await;
    let version = SignalingPeer {
        version_info: Some(VersionInfo {
            api_version: 1,
            build_number: 0,
            commit_hash: String::new(),
            debug_build: cfg!(debug_assertions),
        }),
        peer: PeerInfo {
            remote_desk_type: auth.remote_desk_type,
            operation_system: Default::default(),
            display_name: None,
        },
        server: (Some(format!("client-{id}"))).map(|client_id| ServerRegistration {
            client_id,
            exec_capabilities: HostExecCapabilities {
                available_exec_shells: Vec::new(),
                exec_pty: false,
                exec_pty_elevation: false,
                max_ai_command_runtime_ms: None,
            },
            ai_assistant_settings: desk_agent_protocol::ai_assistant::AiAssistantSettings {
                revision: 0,
                enabled: false,
            },
        }),
    };
    let handler = SignalingHandler::init(
        id.into(),
        version,
        map,
        session,
        CurrentUser::new_admin("fixture"),
        None,
        None,
        None,
        auth,
        CredentialPolicy::Plain,
        1,
        None,
    )
    .await
    .unwrap();
    (response, handler)
}

async fn fetch(
    handler: &mut SignalingHandler<CurrentUser>,
    body: &mut actix_web::body::BoxBody,
) -> SignalingModel {
    let request =
        SignalingModel::new_request::<()>(SignalingType::FetchConnections, None, None).unwrap();
    handler
        .handle_message(serde_json::to_string(&request).unwrap().into())
        .await
        .unwrap();
    let bytes = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        futures::future::poll_fn(|cx| Pin::new(&mut *body).poll_next(cx)),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let offset = match bytes[1] & 0x7f {
        126 => 4,
        127 => 10,
        _ => 2,
    };
    let response: SignalingModel = serde_json::from_slice(&bytes[offset..]).unwrap();
    assert_eq!(response.request_id, request.request_id);
    assert_eq!(response.signaling_type, SignalingType::ConnectionsFetched);
    response
}

fn assert_denied(response: SignalingModel) {
    assert_eq!(
        response.response_state.as_ref().unwrap().error_code,
        DeskErrorCode::PERMISSION_ERROR.code()
    );
    assert!(response.get_raw_data().is_none());
}

#[actix_web::test]
async fn real_handler_scopes_enumeration_and_rechecks_live_grants() {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    let schema = Schema::new(db.get_database_backend());
    db.execute(&schema.entity_table(crate::entity::device_code::Entity))
        .await
        .unwrap();
    crate::entity::device_code::ActiveModel {
        client_id: Set("client-a".into()),
        device_code: Set("AAAAAA".into()),
        generation: Set(2),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    let generations: Arc<dyn DeviceGenerationLookup> =
        Arc::new(crate::request_remote_authorizer::DbDeviceGenerationLookup::new(db.clone()));
    let clock = Arc::new(AtomicI64::new(100));
    let grants: Arc<dyn AccessGrantStore> = Arc::new(InProcessAccessGrantStore::with_clock(
        Box::new(Clock(clock.clone())),
    ));
    let record = GrantSessionRecord {
        principal: GrantPrincipal::from_code_session("principal-a"),
        target_device: "client-a".into(),
        access_ceiling: Some(SecuritySettings::default()),
        generation: 2,
    };
    let grant = grants.mint(&record, 30).await.unwrap();
    let cookie = CodeSessionCookie {
        code_session_id: "principal-a".into(),
        grant_session_id: grant.grant_session_id.clone(),
        target_connection_id: "a".into(),
    };
    let map = web::Data::new(SharedConnectionMap::new());
    let (_response_a, host_a) = make_handler(
        "a",
        AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        map.clone(),
    )
    .await;
    let (_response_b, host_b) = make_handler(
        "b",
        AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        map.clone(),
    )
    .await;
    let public_b = serde_json::to_value(map.read().await["b"].public_model()).unwrap();
    assert!(public_b["version_info"].get("token").is_none());
    let (response, browser) = make_handler(
        "browser",
        AuthContext::code_session("principal-a"),
        map.clone(),
    )
    .await;
    let mut browser =
        browser.with_fetch_connections_resolver(Arc::new(SignalFetchConnectionsResolver::new(
            map.clone(),
            Some(cookie.clone()),
            grants.clone(),
            generations.clone(),
        )));
    let mut body = response.into_body();
    let response = fetch(&mut browser, &mut body).await;
    assert!(response.response_state.as_ref().unwrap().is_success());
    let data = response.get_raw_data().as_ref().unwrap();
    assert!(data["connection_map"].get("a").is_some());
    assert_eq!(data["connection_map"].as_object().unwrap().len(), 1);
    assert!(
        !serde_json::to_string(data)
            .unwrap()
            .contains("unrelated-node-secret")
    );

    browser.connection_state.auth_context = AuthContext::cookie(1, RemoteDeskTypeEnum::Browser);
    let response = fetch(&mut browser, &mut body).await;
    let data = response.get_raw_data().as_ref().unwrap();
    assert_eq!(data["connection_map"].as_object().unwrap().len(), 2);
    assert!(
        !serde_json::to_string(data)
            .unwrap()
            .contains("unrelated-node-secret")
    );
    for auth in [
        AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Browser),
        AuthContext::anonymous(RemoteDeskTypeEnum::Browser),
        AuthContext::code_session("other-principal"),
    ] {
        browser.connection_state.auth_context = auth;
        assert_denied(fetch(&mut browser, &mut body).await);
    }
    browser.connection_state.auth_context = AuthContext::code_session("principal-a");
    clock.store(131, Ordering::SeqCst);
    assert_denied(fetch(&mut browser, &mut body).await);
    clock.store(100, Ordering::SeqCst);
    let grant = grants.mint(&record, 30).await.unwrap();
    let mut cookie = cookie;
    cookie.grant_session_id = grant.grant_session_id.clone();
    browser.fetch_connections_resolver = Some(Arc::new(SignalFetchConnectionsResolver::new(
        map.clone(),
        Some(cookie.clone()),
        grants.clone(),
        generations.clone(),
    )));
    grants.revoke(&grant.grant_session_id).await.unwrap();
    assert_denied(fetch(&mut browser, &mut body).await);

    let mut stale = record.clone();
    stale.generation = 1;
    cookie.grant_session_id = grants.mint(&stale, 30).await.unwrap().grant_session_id;
    browser.fetch_connections_resolver = Some(Arc::new(SignalFetchConnectionsResolver::new(
        map.clone(),
        Some(cookie),
        grants,
        generations,
    )));
    assert_denied(fetch(&mut browser, &mut body).await);
    drop(host_a);
    drop(host_b);
}
