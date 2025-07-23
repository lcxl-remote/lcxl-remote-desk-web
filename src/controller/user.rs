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
use log::{info, warn};

use crate::{
    model::{
        settings::SharedSettings,
        user::{CurrentUser, NoLogintUser, NoticeIconList, UserRespone},
    },
    service::user::SessionExt,
};

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
        if let Ok(client_ip) = client_ip_str.parse::<IpAddr>() {
            info!("Parsed client IP: {:?}", client_ip);
            let mut loopback = client_ip.is_loopback();
            if let IpAddr::V6(ipv6) = client_ip {
                if let Some(ipv4) = ipv6.to_ipv4_mapped() {
                    info!("Parsed IPv4 from IPv6-mapped address: {:?}", ipv4);
                    loopback = ipv4.is_loopback();
                }
            }
            if loopback {
                info!("Client IP is loopback, auto login as admin");
                // Allow access for loopback IPs
                let login_user_name = {
                    let settings = settings.read().await;
                    settings.user.login_user_name.clone()
                };
                let user_info = CurrentUser::new_admin(&login_user_name);

                session.set_current_user(&user_info).unwrap(); // Store user information in session
            }
        } else {
            warn!("Failed to parse client IP: {}", client_ip_str);
        }
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
    let user = session.get_current_user()?;
    if user.is_none() {
        return Ok(HttpResponse::Unauthorized().body("User is not logged in."));
    }
    let current_user = user.unwrap();
    info!("Fetching notices for user: {}", current_user.name);

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

    match session.get_current_user()? {
        Some(_) => next.call(req).await,
        None => {
            warn!("Anonymous user tried to access protected resource.");
            Err(actix_web::error::ErrorUnauthorized(
                "User is not logged in.",
            ))
        }
    }
}
