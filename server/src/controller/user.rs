use actix_session::Session;
use actix_web::{
    Error as AWError, FromRequest, HttpRequest, HttpResponse,
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    get,
    middleware::Next,
};
use desk_server_user::{
    model::{CurrentUser, NoLogintUser, UserRespone},
    service::UserSessionAccessor,
};

use desk_signal_facade::model::code_session::{CODE_SESSION_KEY, CodeSessionCookie};
use log::{info, warn};

pub const TAG: &str = "User";

#[utoipa::path(
    tag = TAG,
    summary = "Get current user",
    responses(
        (status = 200, description = "Current user info", body = UserRespone<CurrentUser>),
        (status  = 401, description = "Unauthorized", body = UserRespone<NoLogintUser>),
    ),
)]
#[get("/api/currentUser")]
pub async fn get_current_user(req: HttpRequest, session: Session) -> Result<HttpResponse, AWError> {
    info!("Connection Info: {:?}", req.connection_info());
    if let Some(client_ip_str) = req.connection_info().realip_remote_addr() {
        info!("Client IP: {}", client_ip_str);
    } else {
        warn!("No client IP found in request");
    }

    if let Some(current_user) = session.get_current_user()? {
        let user_response = UserRespone::<CurrentUser> {
            data: current_user,
            error_code: 0,
            error_message: String::from(""),
            success: true,
        };

        info!("Current user: {:?}", user_response.data);
        return Ok(HttpResponse::Ok().json(user_response));
    }
    warn!("User is not logged in.");
    let no_login_user = NoLogintUser { login: false };
    let user_response = UserRespone::<NoLogintUser> {
        data: no_login_user,
        error_code: 401,
        error_message: String::from("User is not logged in."),
        success: true,
    };
    Ok(HttpResponse::Unauthorized().json(user_response))
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
        return Err(actix_web::error::ErrorForbidden(
            "Code sessions must use signaling for scoped capabilities.",
        ));
    }

    warn!("Anonymous user tried to access protected resource.");
    Err(actix_web::error::ErrorUnauthorized(
        "User is not logged in.",
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    use actix_session::{SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{
        App, HttpResponse, cookie::Key, http::StatusCode, middleware::from_fn, test as at, web,
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
                .service(
                    web::scope("/api")
                        .wrap(from_fn(enforce_device_scope))
                        .route("/probe", web::get().to(protected_probe)),
                ),
        )
        .await;

        let anonymous =
            at::try_call_service(&app, at::TestRequest::get().uri("/api/probe").to_request())
                .await
                .expect_err("anonymous REST request must be rejected");
        assert_eq!(
            anonymous.error_response().status(),
            StatusCode::UNAUTHORIZED
        );

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
                .cookie(cookie)
                .to_request(),
        )
        .await
        .expect_err("code-session REST request must be rejected");
        assert_eq!(
            code_session.error_response().status(),
            StatusCode::FORBIDDEN
        );
    }
}
