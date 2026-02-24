use actix_session::Session;
use actix_web::{Error as AWError, HttpResponse, post, web};
use desk_server_user::{model::CurrentUser, service::UserSessionAccessor};
use desk_server_version::SERVER_API_VERSION;
use log::{error, info};

use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::RwLock;

use crate::{
    error::DeskErrorCode,
    model::{
        login::{FakeCaptcha, FakeCaptchaParams, LoginParams, LoginResult, PasswordParams},
        settings::SharedSettings,
    },
};
use desk_utils::rest::RestResponse;

static DEVICE_CODE_RATE_LIMIT: std::sync::LazyLock<RwLock<HashMap<String, (u32, Instant)>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

#[utoipa::path(
    summary = "Login user account",
    request_body(content = LoginParams),
    responses(
        (status = 200, description = "Login result", body=LoginResult),
        (status = 403, description = "Illegal username or password"),
    ),
)]
#[post("/api/login/account")]
pub async fn login_account(
    req: actix_web::HttpRequest,
    requst_json: web::Json<LoginParams>,
    settings: web::Data<SharedSettings>,
    session_map: web::Data<desk_signal::model::SharedSessionMap>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();
    let startup_mode = {
        let settings = settings.read().await;
        settings.args.startup_mode.as_ref().to_string()
    };

    if params.login_type == "device_code" {
        let ip = req
            .connection_info()
            .realip_remote_addr()
            .unwrap_or("unknown")
            .to_string();

        let mut rate_limit = DEVICE_CODE_RATE_LIMIT.write().await;
        let now = Instant::now();

        // Rate limit: 5 times per minute per IP
        if let Some((count, last_time)) = rate_limit.get_mut(&ip) {
            if now.duration_since(*last_time).as_secs() < 60 {
                if *count >= 5 {
                    return Ok(HttpResponse::Forbidden().json(RestResponse::<()>::failed(
                        DeskErrorCode::SYSTEM_ERROR,
                        "Too many attempts. Please try again later.".to_string(),
                    )));
                }
                *count += 1;
            } else {
                *count = 1;
                *last_time = now;
            }
        } else {
            rate_limit.insert(ip, (1, now));
        }

        let device_code = params.device_code.clone().unwrap_or_default();
        if device_code.is_empty() {
            return Ok(HttpResponse::Forbidden().json(RestResponse::<()>::failed(
                DeskErrorCode::SYSTEM_ERROR,
                "Device code is empty".to_string(),
            )));
        }

        let session_map_guard = session_map.read().await;
        let mut target_session_id = None;

        for (sid, sstate) in session_map_guard.iter() {
            if sstate.device_code.as_ref() == Some(&device_code) {
                target_session_id = Some(sid.clone());
                break;
            }
        }

        if let Some(target_id) = target_session_id {
            let mut user_info = CurrentUser::new_admin("device_user");
            user_info.access = Some("device_user".to_string());
            user_info.target_session_id = Some(target_id.clone());
            session.set_current_user(&user_info)?;

            let result = LoginResult {
                status: String::from("ok"),
                login_type: params.login_type,
                current_authority: String::from("device_user"),
                api_version: SERVER_API_VERSION,
                target_session_id: Some(target_id),
                startup_mode: Some(startup_mode),
            };
            info!("Device code login successful");
            return Ok(HttpResponse::Ok().json(result));
        } else {
            return Ok(HttpResponse::Forbidden().json(RestResponse::<()>::failed(
                DeskErrorCode::SYSTEM_ERROR,
                "Device code not found or device is offline".to_string(),
            )));
        }
    }
    {
        let settings = settings.read().await;
        if settings.user.login_user_name != params.username {
            error!("Username does not match");
            return Ok(HttpResponse::Forbidden().json(RestResponse::<()>::failed(
                DeskErrorCode::SYSTEM_ERROR,
                "Illegal username or password".to_string(),
            )));
        }
        if settings.user.login_password != params.password {
            error!("Password does not match");
            return Ok(HttpResponse::Forbidden().json(RestResponse::<()>::failed(
                DeskErrorCode::SYSTEM_ERROR,
                "Illegal username or password".to_string(),
            )));
        }
    }
    let result = LoginResult {
        status: String::from("ok"),
        login_type: params.login_type,
        current_authority: String::from("admin"),
        api_version: SERVER_API_VERSION,
        target_session_id: None,
        startup_mode: Some(startup_mode),
    };
    let user_info = CurrentUser::new_admin(&params.username);
    // Store user information in session
    session.set_current_user(&user_info)?;
    info!("Login successful, username: {}", params.username);
    Ok(HttpResponse::Ok().json(result))
}

#[utoipa::path(
    summary = "Get captcha for login",
    request_body(content = FakeCaptchaParams),
    responses(
        (status = 200, description = "Get captcha successfully", body=FakeCaptcha),
        (status = 400, description = "Bad request"),
        (status = 501, description = "Not implemented"),
    ),
)]
#[post("/api/login/captcha")]
pub async fn get_captcha(
    requst_json: web::Json<FakeCaptchaParams>,
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();
    if params.phone.is_none() {
        return Ok(HttpResponse::BadRequest().body("Phone is null"));
    }
    return Ok(HttpResponse::NotImplemented().body("Not implemented"));
}

#[utoipa::path(
    summary = "Logout user account",
    responses(
        (status = 200, description = "Logout successful"),
    ),
)]
#[post("/api/login/outLogin")]
pub async fn logout_account(session: Session) -> Result<HttpResponse, AWError> {
    session.remove_current_user();
    info!("Logout successful");
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    summary = "Change password of user account",
    request_body(content = PasswordParams),
    responses(
        (status = 200, description = "Change password successful"),
        (status = 403, description = "Illegal username or password"),
    ),
)]
#[post("/api/login/password")]
pub async fn change_password(
    requst_json: web::Json<PasswordParams>,
    settings: web::Data<SharedSettings>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();
    let mut settings = settings.write().await;
    if params.password.is_empty() || params.username.is_empty() {
        error!("Username or password is empty");
        return Ok(HttpResponse::Forbidden().body("Illegal username or password"));
    }
    if params.new_password.is_none() && params.new_username.is_none() {
        error!("All new username and new  password are  empty");
        return Ok(HttpResponse::Forbidden().body("Illegal new username or new password"));
    }

    if settings.user.login_user_name != params.username {
        error!("Username does not match");
        return Ok(HttpResponse::Forbidden().body("Illegal username or password"));
    }
    if settings.user.login_password != params.password {
        error!("Password does not match");
        return Ok(HttpResponse::Forbidden().body("Illegal username or password"));
    }

    if let Some(new_username) = params.new_username {
        if !new_username.is_empty() {
            info!(
                "Change username from {} to {}",
                settings.user.login_user_name, new_username
            );
            settings.user.login_user_name = new_username;
        }
    }

    if let Some(new_password) = params.new_password {
        if !new_password.is_empty() {
            info!("Change password successfully");
            settings.user.login_password = new_password;
        }
    }

    // save new settings to file
    settings.save()?;
    info!("Username / password changed successfully");

    // logout
    session.remove_current_user();
    Ok(HttpResponse::Ok().finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Args, Settings, SharedSettings};
    use actix_session::{SessionMiddleware, storage::CookieSessionStore};
    use actix_web::{
        App,
        cookie::Key,
        test::{self, TestRequest},
        web,
    };
    use std::env;

    fn create_test_settings() -> SharedSettings {
        let mut settings = Settings::default();
        settings.user.login_user_name = "admin".to_string();
        settings.user.login_password = "password".to_string();

        // Use a temp file for config to avoid overwriting real config
        let mut temp_path = env::temp_dir();
        temp_path.push(format!("desk_test_config_{}.toml", uuid::Uuid::new_v4()));
        settings.args = Args {
            config_file_path: temp_path.to_string_lossy().to_string(),
            ..Default::default()
        };

        SharedSettings::from(settings)
    }

    #[actix_web::test]
    async fn test_login_success() {
        let settings = create_test_settings();
        let session_map = web::Data::new(desk_signal::model::SharedSessionMap::from(
            std::collections::BTreeMap::new(),
        ));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(session_map)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_account),
        )
        .await;

        let params = LoginParams {
            username: "admin".to_string(),
            password: "password".to_string(),
            login_type: "account".to_string(),
            ..Default::default()
        };

        let req = TestRequest::post()
            .uri("/api/login/account")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert!(resp.status().is_success());
        let body: LoginResult = test::read_body_json(resp).await;
        assert_eq!(body.status, "ok");
    }

    #[actix_web::test]
    async fn test_login_failure() {
        let settings = create_test_settings();
        let session_map = web::Data::new(desk_signal::model::SharedSessionMap::from(
            std::collections::BTreeMap::new(),
        ));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(session_map)
                .wrap(SessionMiddleware::new(
                    CookieSessionStore::default(),
                    Key::generate(),
                ))
                .service(login_account),
        )
        .await;

        let params = LoginParams {
            username: "admin".to_string(),
            password: "wrong_password".to_string(),
            login_type: "account".to_string(),
            ..Default::default()
        };

        let req = TestRequest::post()
            .uri("/api/login/account")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(resp.status(), actix_web::http::StatusCode::FORBIDDEN);
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

        let params = PasswordParams {
            username: "admin".to_string(),
            password: "password".to_string(),
            new_password: Some("new_password".to_string()),
            new_username: None,
        };

        let req = TestRequest::post()
            .uri("/api/login/password")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;

        assert!(resp.status().is_success());
    }
}
