use crate::{config::connection::DatabaseConnection, entity::device_code};
use actix_web::{FromRequest, HttpResponse, test::TestRequest, web};
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;
use desk_server_user::model::CurrentUser;
use desk_signal_facade::{
    model::{
        auth_context::AuthContext, connection::SharedConnectionMap, peer::PeerQuery,
        signal::RemoteDeskTypeEnum, version::VersionInfo,
    },
    service::{CredentialPolicy, SignalingHandler},
};
use sea_orm::{ConnectionTrait, Schema};

pub(super) async fn db() -> DatabaseConnection {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    db.execute(&Schema::new(db.get_database_backend()).entity_table(device_code::Entity))
        .await
        .unwrap();
    db
}

pub(super) async fn publish(
    map: web::Data<SharedConnectionMap>,
    client: &str,
    id: &str,
    code: String,
) -> (HttpResponse, SignalingHandler<CurrentUser>) {
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
        CurrentUser::new_admin("fixture"),
        None,
        None,
        Some(code),
        AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server),
        CredentialPolicy::Plain,
        1,
        None,
    )
    .await
    .unwrap();
    (response, handler)
}
