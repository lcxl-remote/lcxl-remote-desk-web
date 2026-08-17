use actix_session::Session;
use actix_web::{Error as AWError, HttpRequest, HttpResponse, patch, post, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::model::auth::{
    EmptyResponseDto, LoginOutcomeDto, LoginRequest, UpdateCredentialsRequest,
};
use log::{error, info};

use crate::{
    error::DeskErrorCode,
    model::settings::SharedSettings,
    service::{
        client_ip::ClientIpExtractor,
        rate_limit::{AuthRateLimiter, LoginFailureResult},
    },
};
use desk_utils::rest::RestResponse;

pub const TAG: &str = "Auth";

#[utoipa::path(
    tag = TAG,
    summary = "Login the configured local user account",
    description = "The username is matched exactly against the standalone server's configured local username. The standalone server has no email lookup lane.",
    request_body(content = LoginRequest),
    responses(
        (status = 200, description = "Login result", body = RestResponse<LoginOutcomeDto>),
    ),
)]
#[post("/api/auth/login")]
pub async fn login_account(
    req: HttpRequest,
    request_json: web::Json<LoginRequest>,
    settings: web::Data<SharedSettings>,
    client_ip: web::Data<ClientIpExtractor>,
    rate_limiter: web::Data<std::sync::Arc<AuthRateLimiter>>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let network_key = client_ip.network_key(&req);
    let startup_mode = {
        let settings = settings.read().await;
        settings.args.startup_mode.clone()
    };

    if let Some(retry_after_sec) = rate_limiter.login_lock_ttl(&network_key) {
        return Ok(login_failure_response(
            DeskErrorCode::ACCOUNT_LOCKED,
            "Too many attempts. Please try again later.",
            startup_mode,
            Some(retry_after_sec),
        ));
    }

    // Device-code redemption no longer logs in as a full account. An anonymous
    // redeemer is a capability-scoped code-session minted by `redeem_code`
    // (`/api/desk/redeem-code`), never the owner. Account login is
    // username/password only.
    let credentials_match = {
        let settings = settings.read().await;
        settings.user.login_user_name == params.username
            && settings.user.login_password == params.password
    };
    if !credentials_match {
        error!("Account login credentials do not match");
        return Ok(match rate_limiter.record_login_failure(network_key) {
            LoginFailureResult::Locked { retry_after_sec } => login_failure_response(
                DeskErrorCode::ACCOUNT_LOCKED,
                "Too many attempts. Please try again later.",
                startup_mode,
                Some(retry_after_sec),
            ),
            LoginFailureResult::Recorded | LoginFailureResult::UntrackedCapacity => {
                login_failure_response(
                    DeskErrorCode::ILLEGAL_CREDENTIALS,
                    "Illegal username or password",
                    startup_mode,
                    None,
                )
            }
        });
    }
    rate_limiter.clear_login(&network_key);
    let result = LoginOutcomeDto {
        captcha_required: Some(false),
        retry_after_sec: None,
        api_version: Some(SERVER_API_VERSION),
        startup_mode: Some(startup_mode),
        // The standalone server has no email-verified account state.
        email_masked: None,
    };
    let user_info = CurrentUser::new_admin(&params.username);
    // Store user information in session
    session.set_current_user(&user_info)?;
    info!("Login successful, username: {}", params.username);
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(result)))
}

fn login_failure_response(
    code: DeskErrorCode,
    message: &str,
    startup_mode: crate::model::settings::StartupMode,
    retry_after_sec: Option<u64>,
) -> HttpResponse {
    HttpResponse::Ok().json(RestResponse::<LoginOutcomeDto>::failed_with_data(
        code,
        Some(message.to_string()),
        Some(LoginOutcomeDto {
            captcha_required: Some(false),
            retry_after_sec,
            api_version: Some(SERVER_API_VERSION),
            startup_mode: Some(startup_mode),
            email_masked: None,
        }),
    ))
}

#[utoipa::path(
    tag = TAG,
    summary = "Logout user account",
    responses(
        (status = 200, description = "Logout successful", body = RestResponse<EmptyResponseDto>),
    ),
)]
#[post("/api/auth/logout")]
pub async fn logout_account(session: Session) -> Result<HttpResponse, AWError> {
    session.remove_current_user();
    info!("Logout successful");
    Ok(HttpResponse::Ok().json(RestResponse::succeed()))
}

#[utoipa::path(
    tag = TAG,
    summary = "Update the configured local account credentials",
    request_body(content = UpdateCredentialsRequest),
    responses(
        (status = 200, description = "Credentials result", body = RestResponse<EmptyResponseDto>),
        (status = 401, description = "Owner session required", body = RestResponse<EmptyResponseDto>),
        (status = 403, description = "Code sessions cannot update credentials", body = RestResponse<EmptyResponseDto>),
    ),
)]
#[patch("/auth/credentials")]
pub async fn change_password(
    request_json: web::Json<UpdateCredentialsRequest>,
    settings: web::Data<SharedSettings>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let mut settings = settings.write().await;
    if params.current_password.is_empty() || params.current_username.is_empty() {
        error!("Username or password is empty");
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::ILLEGAL_CREDENTIALS,
            "Illegal username or password".to_string(),
        )));
    }
    if params.new_password.is_none() && params.new_username.is_none() {
        error!("All new username and new  password are  empty");
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::INVALID_PARAMS,
            "Illegal new username or new password".to_string(),
        )));
    }

    if settings.user.login_user_name != params.current_username {
        error!("Username does not match");
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::ILLEGAL_CREDENTIALS,
            "Illegal username or password".to_string(),
        )));
    }
    if settings.user.login_password != params.current_password {
        error!("Password does not match");
        return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
            DeskErrorCode::ILLEGAL_CREDENTIALS,
            "Illegal username or password".to_string(),
        )));
    }

    if let Some(new_username) = params.new_username
        && !new_username.is_empty()
    {
        info!(
            "Change username from {} to {}",
            settings.user.login_user_name, new_username
        );
        settings.user.login_user_name = new_username;
    }

    if let Some(new_password) = params.new_password
        && !new_password.is_empty()
    {
        info!("Change password successfully");
        settings.user.login_password = new_password;
    }

    // save new settings to file
    settings.save()?;
    info!("Username / password changed successfully");

    // logout
    session.remove_current_user();
    Ok(HttpResponse::Ok().json(RestResponse::succeed()))
}

/// Query params for tauri login
#[derive(serde::Deserialize)]
pub struct TauriLoginQuery {
    pub token: String,
}

#[utoipa::path(
    tag = TAG,
    summary = "Auto-login from Tauri WebView using one-time token",
    params(
        ("token" = String, Query, description = "One-time login token generated by Tauri")
    ),
    responses(
        (status = 200, description = "Login result", body = RestResponse<LoginOutcomeDto>),
    ),
)]
#[post("/api/auth/tauri-login")]
pub async fn login_tauri(
    query: web::Query<TauriLoginQuery>,
    settings: web::Data<SharedSettings>,
    tauri_token: web::Data<Option<crate::TauriLoginToken>>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    // Check if tauri token is configured
    let token_store = match tauri_token.as_ref() {
        Some(store) => store,
        None => {
            error!("Tauri login attempted but no token configured");
            return Ok(
                HttpResponse::Ok().json(RestResponse::<LoginOutcomeDto>::failed_with_data(
                    DeskErrorCode::FEATURE_UNAVAILABLE,
                    Some("Tauri login is not available".to_string()),
                    None,
                )),
            );
        }
    };

    // Verify and consume the one-time token
    if !token_store.verify_and_consume(&query.token) {
        error!("Tauri login failed: invalid or already consumed token");
        return Ok(
            HttpResponse::Ok().json(RestResponse::<LoginOutcomeDto>::failed_with_data(
                DeskErrorCode::INVALID_OR_EXPIRED_TOKEN,
                Some("Invalid or expired token".to_string()),
                None,
            )),
        );
    }

    // Check system is initialized
    let (startup_mode, initialized, username) = {
        let settings = settings.read().await;
        let mode = settings.args.startup_mode.clone();
        let init = !settings.user.login_password.is_empty();
        let name = settings.user.login_user_name.clone();
        (mode, init, name)
    };

    if !initialized {
        error!("Tauri login failed: system not initialized");
        return Ok(
            HttpResponse::Ok().json(RestResponse::<LoginOutcomeDto>::failed_with_data(
                DeskErrorCode::PRECONDITION_FAILED,
                Some("System not initialized".to_string()),
                None,
            )),
        );
    }

    // Auto-login as admin
    let user_info = CurrentUser::new_admin(&username);
    session.set_current_user(&user_info)?;

    let result = LoginOutcomeDto {
        captcha_required: None,
        retry_after_sec: None,
        api_version: Some(SERVER_API_VERSION),
        startup_mode: Some(startup_mode),
        email_masked: None,
    };

    info!("Tauri auto-login successful, username: {}", username);
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(result)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Settings, SharedSettings};
    use actix_session::{SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{
        App,
        cookie::Key,
        test::{self, TestRequest},
        web,
    };
    use std::env;

    fn create_test_settings() -> SharedSettings {
        // Use a temp file for config to avoid overwriting real config
        let mut temp_path = env::temp_dir();
        temp_path.push(format!("desk_test_config_{}.toml", uuid::Uuid::new_v4()));
        let mut settings = Settings::for_test_config(&temp_path);
        settings.user.login_user_name = "admin".to_string();
        settings.user.login_password = "password".to_string();

        SharedSettings::from(settings)
    }

    fn auth_data() -> (
        web::Data<ClientIpExtractor>,
        web::Data<std::sync::Arc<AuthRateLimiter>>,
    ) {
        (
            web::Data::new(ClientIpExtractor::default()),
            web::Data::new(std::sync::Arc::new(AuthRateLimiter::new(64))),
        )
    }

    #[actix_web::test]
    async fn test_login_success() {
        let settings = create_test_settings();
        let (client_ip, limiter) = auth_data();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(client_ip)
                .app_data(limiter)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_account),
        )
        .await;

        let params = LoginRequest {
            username: "admin".to_string(),
            password: "password".to_string(),
            captcha_token: None,
        };

        let req = TestRequest::post()
            .peer_addr("192.0.2.1:1234".parse().unwrap())
            .uri("/api/auth/login")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert!(resp.status().is_success());
        let body: RestResponse<LoginOutcomeDto> = test::read_body_json(resp).await;
        assert!(body.success);
        assert_eq!(body.data.unwrap().api_version, Some(SERVER_API_VERSION));
    }

    #[actix_web::test]
    async fn test_login_failure() {
        let settings = create_test_settings();
        let (client_ip, limiter) = auth_data();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(client_ip)
                .app_data(limiter)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_account),
        )
        .await;

        let params = LoginRequest {
            username: "admin".to_string(),
            password: "wrong_password".to_string(),
            captcha_token: None,
        };

        let req = TestRequest::post()
            .peer_addr("192.0.2.2:1234".parse().unwrap())
            .uri("/api/auth/login")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
        let body: RestResponse<LoginOutcomeDto> = test::read_body_json(resp).await;
        assert!(!body.success);
        assert_eq!(body.code, DeskErrorCode::ILLEGAL_CREDENTIALS.code());
    }

    #[actix_web::test]
    async fn twentieth_failure_locks_before_a_correct_password_is_checked() {
        let settings = create_test_settings();
        let (client_ip, limiter) = auth_data();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(client_ip)
                .app_data(limiter)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_account),
        )
        .await;

        for attempt in 1..=20 {
            let req = TestRequest::post()
                .peer_addr("192.0.2.20:1234".parse().unwrap())
                .uri("/api/auth/login")
                .set_json(&LoginRequest {
                    username: "admin".to_string(),
                    password: "wrong".to_string(),
                    captcha_token: None,
                })
                .to_request();
            let body: RestResponse<LoginOutcomeDto> =
                test::call_and_read_body_json(&app, req).await;
            if attempt < 20 {
                assert_eq!(body.code, DeskErrorCode::ILLEGAL_CREDENTIALS.code());
            } else {
                assert_eq!(body.code, DeskErrorCode::ACCOUNT_LOCKED.code());
                assert!(
                    body.data
                        .unwrap()
                        .retry_after_sec
                        .is_some_and(|ttl| ttl > 0)
                );
            }
        }

        let correct = TestRequest::post()
            .peer_addr("192.0.2.20:1234".parse().unwrap())
            .uri("/api/auth/login")
            .set_json(&LoginRequest {
                username: "admin".to_string(),
                password: "password".to_string(),
                captcha_token: None,
            })
            .to_request();
        let body: RestResponse<LoginOutcomeDto> =
            test::call_and_read_body_json(&app, correct).await;
        assert_eq!(body.code, DeskErrorCode::ACCOUNT_LOCKED.code());
    }

    #[actix_web::test]
    async fn cross_origin_xff_preflight_is_not_allowed() {
        let settings = create_test_settings();
        let (client_ip, limiter) = auth_data();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(client_ip)
                .app_data(limiter)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_account),
        )
        .await;
        let req = TestRequest::default()
            .method(actix_web::http::Method::OPTIONS)
            .uri("/api/auth/login")
            .insert_header(("origin", "https://evil.example"))
            .insert_header(("access-control-request-method", "POST"))
            .insert_header(("access-control-request-headers", "x-forwarded-for"))
            .to_request();
        let response = test::call_service(&app, req).await;
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );
        assert!(
            response
                .headers()
                .get("access-control-allow-headers")
                .is_none()
        );
    }

    #[actix_web::test]
    async fn test_change_password_success() {
        let settings = create_test_settings();
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(change_password),
        )
        .await;

        let params = UpdateCredentialsRequest {
            current_username: "admin".to_string(),
            current_password: "password".to_string(),
            new_password: Some("new_password".to_string()),
            new_username: None,
        };

        let req = TestRequest::patch()
            .uri("/auth/credentials")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert!(resp.status().is_success());
    }

    #[actix_web::test]
    async fn test_tauri_login_success() {
        let settings = create_test_settings();
        // Set password to mark as initialized
        {
            let mut s = settings.write().await;
            s.user.login_password = "test_password".to_string();
        }
        let token = "test-token-12345";
        let tauri_token = web::Data::new(Some(crate::TauriLoginToken::new(token.to_string())));

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(tauri_token)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_tauri),
        )
        .await;

        let req = TestRequest::post()
            .uri(&format!("/api/auth/tauri-login?token={}", token))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let body: RestResponse<LoginOutcomeDto> = test::read_body_json(resp).await;
        assert!(body.success);
    }

    #[actix_web::test]
    async fn test_tauri_login_invalid_token() {
        let settings = create_test_settings();
        {
            let mut s = settings.write().await;
            s.user.login_password = "test_password".to_string();
        }
        let tauri_token = web::Data::new(Some(crate::TauriLoginToken::new(
            "correct-token".to_string(),
        )));

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(tauri_token)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_tauri),
        )
        .await;

        let req = TestRequest::post()
            .uri("/api/auth/tauri-login?token=wrong-token")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
        let body: RestResponse<LoginOutcomeDto> = test::read_body_json(resp).await;
        assert!(!body.success);
        assert_eq!(body.code, DeskErrorCode::INVALID_OR_EXPIRED_TOKEN.code());
    }

    #[actix_web::test]
    async fn test_tauri_login_token_consumed() {
        let settings = create_test_settings();
        {
            let mut s = settings.write().await;
            s.user.login_password = "test_password".to_string();
        }
        let token = "one-time-token";
        let tauri_token = web::Data::new(Some(crate::TauriLoginToken::new(token.to_string())));

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(tauri_token)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_tauri),
        )
        .await;

        // First use: should succeed
        let req = TestRequest::post()
            .uri(&format!("/api/auth/tauri-login?token={}", token))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        // Second use: should fail (token consumed)
        let req2 = TestRequest::post()
            .uri(&format!("/api/auth/tauri-login?token={}", token))
            .to_request();
        let resp2 = test::call_service(&app, req2).await;
        assert_eq!(resp2.status(), actix_web::http::StatusCode::OK);
        let body: RestResponse<LoginOutcomeDto> = test::read_body_json(resp2).await;
        assert!(!body.success);
    }
}
