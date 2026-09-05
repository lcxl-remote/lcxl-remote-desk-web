use actix_session::Session;
use actix_web::{
    Error as AWError, FromRequest, HttpRequest, HttpResponse,
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    error::InternalError,
    get,
    middleware::Next,
};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};

use desk_signal_facade::model::{
    auth::{CurrentUserDto, EmptyResponseDto},
    code_session::{CODE_SESSION_KEY, CodeSessionCookie},
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use log::{info, warn};

pub const TAG: &str = "User";

#[utoipa::path(
    tag = TAG,
    summary = "Get current user",
    responses(
        (status = 200, description = "Current user info", body = RestResponse<CurrentUserDto>),
        (status = 401, description = "Unauthorized", body = RestResponse<EmptyResponseDto>),
    ),
)]
#[get("/api/auth/me")]
pub async fn get_current_user(req: HttpRequest, session: Session) -> Result<HttpResponse, AWError> {
    info!("Connection Info: {:?}", req.connection_info());
    if let Some(client_ip_str) = req.connection_info().realip_remote_addr() {
        info!("Client IP: {}", client_ip_str);
    } else {
        warn!("No client IP found in request");
    }

    if let Some(current_user) = session.get_current_user::<CurrentUser>()? {
        let user = CurrentUserDto {
            user_id: None,
            name: current_user.name,
            avatar: current_user.avatar,
            email: current_user.email,
            access: current_user.access,
            target_connection_id: current_user.target_connection_id,
        };

        info!("Current user: {}", user.name);
        return Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(user)));
    }
    warn!("User is not logged in.");
    Ok(
        HttpResponse::Unauthorized().json(RestResponse::<()>::failed(
            DeskErrorCode::PERMISSION_ERROR,
            "User is not logged in.".to_string(),
        )),
    )
}

/// Default-deny guard for the REST `/api` surface. Owners keep full access;
/// code sessions exercise scoped capabilities exclusively through signaling.
pub async fn enforce_device_scope(
    mut req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    let session = {
        let (http_request, payload) = req.parts_mut();
        Session::from_request(http_request, payload).await
    }?;

    // Owner (single account) — full access, current behavior.
    if session.get_current_user::<CurrentUser>()?.is_some() {
        return next.call(req).await;
    }

    if session
        .get::<CodeSessionCookie>(CODE_SESSION_KEY)?
        .is_some()
    {
        warn!("Code session denied access to the REST API.");
        let response = HttpResponse::Forbidden().json(RestResponse::<()>::failed(
            DeskErrorCode::PERMISSION_ERROR,
            "Code sessions must use signaling for scoped capabilities.".to_string(),
        ));
        return Err(InternalError::from_response("Code session is forbidden", response).into());
    }

    warn!("Anonymous user tried to access protected resource.");
    let response = HttpResponse::Unauthorized().json(RestResponse::<()>::failed(
        DeskErrorCode::PERMISSION_ERROR,
        "User is not logged in.".to_string(),
    ));
    Err(InternalError::from_response("User is not logged in", response).into())
}
#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{
        App, HttpResponse, body::to_bytes, cookie::Key, http::StatusCode, middleware::from_fn,
        test as at, web,
    };

    async fn seed_code_session(session: Session) -> HttpResponse {
        session
            .insert(
                CODE_SESSION_KEY,
                CodeSessionCookie {
                    code_session_id: "code-session".to_string(),
                    grant_session_id: "grant-session".to_string(),
                    target_connection_id: "device".to_string(),
                },
            )
            .expect("seed code session");
        HttpResponse::Ok().finish()
    }

    async fn seed_owner_session(session: Session) -> HttpResponse {
        session
            .set_current_user(&CurrentUser::new_admin("owner"))
            .expect("seed owner session");
        HttpResponse::Ok().finish()
    }

    async fn protected_probe() -> HttpResponse {
        HttpResponse::Ok().finish()
    }

    #[actix_web::test]
    async fn code_sessions_cannot_access_rest() {
        let app = at::init_service(
            App::new()
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .route("/seed", web::post().to(seed_code_session))
                .route("/seed-owner", web::post().to(seed_owner_session))
                .service(
                    web::scope("/api")
                        .wrap(from_fn(enforce_device_scope))
                        .service(desk_signal::controller::web_search::get_web_search)
                        .service(desk_signal::controller::web_search::update_web_search)
                        .service(desk_signal::controller::web_search::test_web_search)
                        .route("/probe", web::get().to(protected_probe)),
                ),
        )
        .await;

        let anonymous =
            at::try_call_service(&app, at::TestRequest::get().uri("/api/probe").to_request())
                .await
                .expect_err("anonymous REST request must be rejected");
        let anonymous_response = anonymous.error_response();
        assert_eq!(anonymous_response.status(), StatusCode::UNAUTHORIZED);
        let anonymous_bytes = to_bytes(anonymous_response.into_body())
            .await
            .expect("read anonymous response body");
        let anonymous_body: RestResponse<()> =
            serde_json::from_slice(&anonymous_bytes).expect("decode anonymous response body");
        assert!(!anonymous_body.success);
        assert_eq!(anonymous_body.code, DeskErrorCode::PERMISSION_ERROR.code());

        let seed = at::call_service(&app, at::TestRequest::post().uri("/seed").to_request()).await;
        let cookie = seed
            .response()
            .cookies()
            .next()
            .expect("session cookie")
            .into_owned();
        let code_session = at::try_call_service(
            &app,
            at::TestRequest::get()
                .uri("/api/probe")
                .cookie(cookie.clone())
                .to_request(),
        )
        .await
        .expect_err("code-session REST request must be rejected");
        let code_session_response = code_session.error_response();
        assert_eq!(code_session_response.status(), StatusCode::FORBIDDEN);
        let code_session_bytes = to_bytes(code_session_response.into_body())
            .await
            .expect("read code-session response body");
        let code_session_body: RestResponse<()> =
            serde_json::from_slice(&code_session_bytes).expect("decode code-session response body");
        assert!(!code_session_body.success);
        assert_eq!(
            code_session_body.code,
            DeskErrorCode::PERMISSION_ERROR.code()
        );
        for (method, uri) in [
            (actix_web::http::Method::GET, "/api/admin/system/web-search"),
            (actix_web::http::Method::PUT, "/api/admin/system/web-search"),
            (
                actix_web::http::Method::POST,
                "/api/admin/system/web-search/test",
            ),
        ] {
            let request = || {
                at::TestRequest::default()
                    .method(method.clone())
                    .uri(uri)
                    .set_json(serde_json::json!({"expected_revision":0,"provider":"duck_duck_go"}))
            };
            assert!(
                at::try_call_service(&app, request().to_request())
                    .await
                    .is_err()
            );
            assert!(
                at::try_call_service(&app, request().cookie(cookie.clone()).to_request())
                    .await
                    .is_err()
            );
        }

        let owner_seed = at::call_service(
            &app,
            at::TestRequest::post().uri("/seed-owner").to_request(),
        )
        .await;
        let owner_cookie = owner_seed
            .response()
            .cookies()
            .next()
            .expect("owner session cookie")
            .into_owned();
        let owner = at::call_service(
            &app,
            at::TestRequest::get()
                .uri("/api/probe")
                .cookie(owner_cookie)
                .to_request(),
        )
        .await;
        assert_eq!(owner.status(), StatusCode::OK);
    }
}
