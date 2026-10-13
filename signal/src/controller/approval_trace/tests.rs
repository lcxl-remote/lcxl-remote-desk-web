use super::*;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{App, cookie::Key, test};
async fn login(session: Session, role: web::Path<String>) -> HttpResponse {
    use desk_server_user::service::UserSessionAccessor;
    let mut user = desk_server_user::model::CurrentUser::new_admin("test");
    if role.as_str() == "scoped" {
        user.target_connection_id = Some("device".into());
    } else {
        user.access = Some(role.to_string());
    }
    session.set_current_user(&user).unwrap();
    HttpResponse::Ok().finish()
}
#[actix_web::test]
async fn approval_routes_reject_ordinary_scoped_and_anonymous_before_accessing_storage() {
    let app = test::init_service(
        App::new()
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), Key::generate())
                    .cookie_secure(false)
                    .build(),
            )
            .route("/login/{role}", web::post().to(login))
            .service(list_approval_reviews)
            .service(get_approval_review)
            .service(get_approval_review_parts)
            .service(get_approval_metrics_overview)
            .service(get_approval_trace_settings)
            .service(update_approval_trace_settings),
    )
    .await;
    for role in [None, Some("guest"), Some("device_user"), Some("scoped")] {
        let cookie = if let Some(role) = role {
            let response = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri(&format!("/login/{role}"))
                    .to_request(),
            )
            .await;
            Some(response.response().cookies().next().unwrap().into_owned())
        } else {
            None
        };
        for path in [
            "/my/ai-assistant-session/approval-reviews?session=foreign",
            "/my/ai-assistant-session/approval-review?session=foreign&id=foreign",
            "/my/ai-assistant-session/approval-review/parts?session=foreign&id=foreign&kind=input",
            "/model/metrics/approval/overview",
            "/admin/approval-trace-settings",
        ] {
            let mut request = test::TestRequest::get().uri(path);
            if let Some(cookie) = &cookie {
                request = request.cookie(cookie.clone());
            }
            let body: serde_json::Value =
                test::call_and_read_body_json(&app, request.to_request()).await;
            assert_eq!(body["code"], DeskErrorCode::PERMISSION_ERROR.code());
            assert!(body["data"].is_null());
        }
        let mut request = test::TestRequest::put()
            .uri("/admin/approval-trace-settings")
            .set_json(ApprovalTraceSettings::defaults(false));
        if let Some(cookie) = cookie {
            request = request.cookie(cookie);
        }
        let body: serde_json::Value =
            test::call_and_read_body_json(&app, request.to_request()).await;
        assert_eq!(body["code"], DeskErrorCode::PERMISSION_ERROR.code());
    }
}
