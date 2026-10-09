use super::*;

const DENIED_ROLES: &[&str] = &["guest", "device_user", "admin-scoped"];

async fn seed_login(session: Session, role: web::Path<String>) -> HttpResponse {
    use desk_server_user::service::UserSessionAccessor;
    let role = role.into_inner();
    let mut user = desk_server_user::model::CurrentUser::new_admin("metrics-test");
    if role == "admin-scoped" {
        user.target_connection_id = Some("only-one-device".into());
    } else {
        user.access = Some(role);
    }
    session.set_current_user(&user).unwrap();
    HttpResponse::Ok().finish()
}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../signal-facade/tests/fixtures/model_metrics_api_authorization.rs"
));
