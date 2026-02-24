use std::net::IpAddr;

use actix_session::Session;
use actix_web::{
    Error as AWError, FromRequest, HttpRequest, HttpResponse,
    body::MessageBody,
    dev::{ServiceRequest, ServiceResponse},
    get,
    middleware::Next,
    web,
};
use desk_server_user::{
    model::{CurrentUser, NoLogintUser, NoticeIconList, UserRespone},
    service::UserSessionAccessor,
};
use log::{info, warn};

use crate::model::settings::SharedSettings;

#[utoipa::path(
    summary = "Get current user",
    responses(
        (status = 200, description = "Current user info", body = UserRespone<CurrentUser>),
        (status  = 401, description = "Unauthorized", body = UserRespone<NoLogintUser>),
    ),
)]
#[get("/api/currentUser")]
pub async fn get_current_user(
    req: HttpRequest,
    settings: web::Data<SharedSettings>,
    session: Session,
) -> Result<HttpResponse, AWError> {
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
    return Ok(HttpResponse::Unauthorized().json(user_response));
}

#[utoipa::path(
    summary = "Get notices",
    responses(
        (status = 200, description = "Notices", body = NoticeIconList),
        (status  = 401, description = "Unauthorized"),
    ),
)]
#[get("/api/notices")]
pub async fn get_notices(session: Session) -> Result<HttpResponse, AWError> {
    let user_opt = session.get_current_user::<CurrentUser>()?;

    let user = if let Some(user) = user_opt {
        user
    } else {
        return Err(actix_web::error::ErrorUnauthorized("User not logged in"));
    };
    info!("Fetching notices for user: {}", user.name);

    // Simulate fetching notices for the user
    let notice_icon_list = NoticeIconList {
        data: None,
        total: 0,
        success: true,
    };
    return Ok(HttpResponse::Ok().json(notice_icon_list));
}

/// Middleware to reject anonymous users.
pub async fn reject_anonymous_users(
    mut req: ServiceRequest,
    next: Next<impl MessageBody>,
) -> Result<ServiceResponse<impl MessageBody>, actix_web::Error> {
    let session = {
        let (http_request, payload) = req.parts_mut();
        //TypedSession::from_request(http_request, payload).await
        Session::from_request(http_request, payload).await
    }?;

    match session.get_current_user::<CurrentUser>()? {
        Some(_) => next.call(req).await,
        None => {
            warn!("Anonymous user tried to access protected resource.");
            Err(actix_web::error::ErrorUnauthorized(
                "User is not logged in.",
            ))
        }
    }
}
