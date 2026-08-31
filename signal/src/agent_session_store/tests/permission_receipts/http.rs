//! Production controller replay with isolated storage and synthetic connection identity.
use super::*;
use crate::controller::device_assistant_session::{PermissionDecisionBody, decide_permission_on};
use desk_signal_facade::model::{
    auth_context::AuthContext,
    connection::{ConnectionModel, ConnectionState, SharedConnectionMap},
    signal::RemoteDeskTypeEnum,
    version::VersionInfo,
};
use std::sync::Arc;

#[actix_web::test]
async fn controller_replays_before_readiness_and_rejects_changed_target_without_new_work() {
    let (store, decisions) = seed(Database::connect("sqlite::memory:").await.unwrap()).await;
    decide(&store, &decisions, true).await.unwrap();
    let map = web::Data::new(SharedConnectionMap::new());
    let request = actix_web::test::TestRequest::get()
        .insert_header(("connection", "upgrade"))
        .insert_header(("upgrade", "websocket"))
        .insert_header(("sec-websocket-version", "13"))
        .insert_header(("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="))
        .to_http_request();
    let payload = <web::Payload as actix_web::FromRequest>::from_request(
        &request,
        &mut actix_web::dev::Payload::None,
    )
    .await
    .unwrap();
    let (_response, socket, _stream) = actix_ws::handle(&request, payload).unwrap();
    let host = format!("receipt-host-{}", uuid::Uuid::new_v4());
    map.write().await.insert(
        host.clone(),
        ConnectionState {
            model: ConnectionModel {
                connection_id: host.clone(),
                ip: None,
                version_info: VersionInfo::new(
                    1,
                    1,
                    "synthetic".into(),
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
        },
    );
    assert!(
        crate::computer_use_readiness::global_computer_use_readiness_cache()
            .get_fresh(&host, Utc::now())
            .is_none()
    );
    let db = store.db.clone();
    let app = actix_web::test::init_service(actix_web::App::new().app_data(map.clone()).route(
        "/decision",
        web::post().to(
            move |map: web::Data<SharedConnectionMap>, body: web::Json<PermissionDecisionBody>| {
                let db = db.clone();
                async move { decide_permission_on(&db, map, body).await }
            },
        ),
    ))
    .await;
    let payload = serde_json::json!({
        "connection":host, "session":"conversation-1", "requestId":"permission-1",
        "items":[{"itemId":"second", "decision":"deny"},
            {"itemId":"first", "decision":"approve", "resource_scope":["target:device-1"],
            "operation_scope":["observe"], "ttl_seconds":120,"max_uses":1}],
    });
    let saved = state(&store).await;
    for _ in 0..2 {
        let response: serde_json::Value = actix_web::test::call_and_read_body_json(
            &app,
            actix_web::test::TestRequest::post()
                .uri("/decision")
                .set_json(&payload)
                .to_request(),
        )
        .await;
        assert_eq!(response["success"], true, "{response}");
        assert_eq!(response["data"]["state"], "partially_approved");
    }
    let mut changed = payload.clone();
    changed["items"][1]["ttl_seconds"] = serde_json::json!(60);
    let response: serde_json::Value = actix_web::test::call_and_read_body_json(
        &app,
        actix_web::test::TestRequest::post()
            .uri("/decision")
            .set_json(&changed)
            .to_request(),
    )
    .await;
    assert_eq!(response["success"], false);
    map.write()
        .await
        .get_mut(&host)
        .unwrap()
        .model
        .version_info
        .client_id = Some("other-device".into());
    let response: serde_json::Value = actix_web::test::call_and_read_body_json(
        &app,
        actix_web::test::TestRequest::post()
            .uri("/decision")
            .set_json(&payload)
            .to_request(),
    )
    .await;
    assert_eq!(response["success"], false);
    map.write().await.remove(&host);
    let response: serde_json::Value = actix_web::test::call_and_read_body_json(
        &app,
        actix_web::test::TestRequest::post()
            .uri("/decision")
            .set_json(&payload)
            .to_request(),
    )
    .await;
    assert_eq!(response["success"], false);
    assert_eq!(state(&store).await, saved);
}
