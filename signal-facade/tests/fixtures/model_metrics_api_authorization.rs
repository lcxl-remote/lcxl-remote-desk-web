// Exercise production route extractors and session guards without injecting a
// process-global metrics store. Login, successful data queries and UI navigation
// remain separate end-to-end acceptance gates.
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{App, cookie::Key, http::{Method, StatusCode}, test};

async fn clear_login(session: Session) -> HttpResponse {
    session.purge();
    HttpResponse::Ok().finish()
}

#[actix_web::test]
async fn metrics_read_detail_export_source_settings_and_runtime_require_the_current_authorized_session() {
    let app = test::init_service(App::new()
        .wrap(SessionMiddleware::builder(CookieSessionStore::default(), Key::generate())
            .cookie_secure(false).build())
        .route("/seed/{role}", web::post().to(seed_login))
        .route("/clear", web::post().to(clear_login))
        .service(web::scope("/api")
            .service(get_model_metrics_status).service(get_model_metrics_overview)
            .service(get_model_metrics_series).service(get_model_metrics_models)
            .service(get_model_metrics_tools).service(get_model_metrics_calls)
            .service(get_model_metrics_call).service(get_model_metrics_settings)
            .service(update_model_metrics_settings).service(get_model_metrics_runtime)
            .service(get_model_metrics_unassociated))).await;
    for role in std::iter::once(None).chain(DENIED_ROLES.iter().map(|role| Some(*role))) {
        let cookie = if let Some(role) = role {
            let seeded = test::call_service(&app, test::TestRequest::post()
                .uri(&format!("/seed/{role}")).to_request()).await;
            Some(seeded.response().cookies().next().unwrap().into_owned())
        } else { None };
        for path in ["status", "overview", "series", "models", "tools", "calls",
            "calls/backend-chain-1", "settings", "runtime", "unassociated"] {
            let mut request = test::TestRequest::get().uri(&format!("/api/model/metrics/{path}"));
            if let Some(cookie) = &cookie { request = request.cookie(cookie.clone()); }
            let response = test::call_service(&app, request.to_request()).await;
            assert_eq!(response.status(), StatusCode::OK, "business errors retain the REST envelope");
            let body: serde_json::Value = test::read_body_json(response).await;
            assert_eq!(body["success"], false, "role={role:?}, path={path}");
            assert_eq!(body["code"], DeskErrorCode::PERMISSION_ERROR.code());
            assert!(body["data"].is_null(), "no model identity, settings or detail disclosure");
        }
        let mut request = test::TestRequest::put().uri("/api/model/metrics/settings")
            .set_json(MetricsSettings::defaults(false));
        if let Some(cookie) = &cookie { request = request.cookie(cookie.clone()); }
        let body: serde_json::Value = test::call_and_read_body_json(&app, request.to_request()).await;
        assert_eq!(body["code"], DeskErrorCode::PERMISSION_ERROR.code());
        assert!(body["data"].is_null());
    }

    let seeded = test::call_service(&app, test::TestRequest::post().uri("/seed/admin").to_request()).await;
    let cookie = seeded.response().cookies().next().unwrap().into_owned();
    let response = test::call_service(&app, test::TestRequest::get().uri("/api/model/metrics/status")
        .cookie(cookie.clone()).to_request()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let status: serde_json::Value = test::read_body_json(response).await;
    assert_eq!(status["success"], true);
    assert!(status["data"]["state"].is_string());

    // Invalid filters/settings fail before storage lookup for the authorized
    // caller. The unassociated endpoint never accepts current model attribution.
    for (method, uri) in [
        (Method::GET, "/api/model/metrics/overview?category=safety"),
        (Method::GET, "/api/model/metrics/overview?outcome=request_error"),
        (Method::GET, "/api/model/metrics/series?min_duration_ms=5000"),
        (Method::GET, "/api/model/metrics/models?record_kind=call"),
        (Method::GET, "/api/model/metrics/tools?permission=waiting"),
        (Method::GET, "/api/model/metrics/runtime?dispatched=true"),
        (Method::GET, "/api/model/metrics/calls?record_kind=tool&outcome=http_error"),
        (Method::GET, "/api/model/metrics/calls?record_kind=call&permission=approved"),
        (Method::GET, "/api/model/metrics/calls?min_duration_ms=86400001"),
        (Method::GET, "/api/model/metrics/runtime?model_id=7"),
        (Method::GET, "/api/model/metrics/unassociated?model_id=7"),
        (Method::PUT, "/api/model/metrics/settings"),
    ] {
        let mut settings = MetricsSettings::defaults(false);
        settings.revision = "invalid".into();
        let response = test::call_service(&app, test::TestRequest::default().method(method).uri(uri)
            .cookie(cookie.clone()).set_json(settings).to_request()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["code"], DeskErrorCode::INVALID_PARAMS.code(), "{uri}");
        assert!(body["data"].is_null());
    }
    let cleared = test::call_service(&app, test::TestRequest::post().uri("/clear")
        .cookie(cookie).to_request()).await;
    let revoked = cleared.response().cookies().next().unwrap().into_owned();
    let body: serde_json::Value = test::call_and_read_body_json(&app, test::TestRequest::get()
        .uri("/api/model/metrics/status").cookie(revoked).to_request()).await;
    assert_eq!(body["code"], DeskErrorCode::PERMISSION_ERROR.code());
    assert!(body["data"].is_null());
}
