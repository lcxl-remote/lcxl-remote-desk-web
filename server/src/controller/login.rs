use actix_session::Session;
use actix_web::{Error as AWError, HttpResponse, post, web};
use desk_server_user::{model::CurrentUser, service::SessionExt};
use log::{error, info};

use crate::model::{
    login::{FakeCaptcha, FakeCaptchaParams, LoginParams, LoginResult, PasswordParams},
    settings::SharedSettings,
};

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
    requst_json: web::Json<LoginParams>,
    settings: web::Data<SharedSettings>,
    session: Session,
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();
    {
        let settings = settings.read().await;
        if settings.user.login_user_name != params.username {
            error!("Username does not match");
            return Ok(HttpResponse::Forbidden().body("Illegal username or password"));
        }
        if settings.user.login_password != params.password {
            error!("Password does not match");
            return Ok(HttpResponse::Forbidden().body("Illegal username or password"));
        }
    }
    let result = LoginResult {
        status: String::from("ok"),
        login_type: params.login_type,
        current_authority: String::from("admin"),
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
            info!(
                "Change password from {} to {}",
                settings.user.login_password, new_password
            );
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
