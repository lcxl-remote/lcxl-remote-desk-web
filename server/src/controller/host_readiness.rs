//! Local-only host readiness mutations.
//!
//! These endpoints may trigger operating-system permission UI. They therefore
//! require all three boundaries at once: the authenticated owner API scope, a
//! kernel-reported loopback TCP peer, and a same-origin browser request.

use std::net::IpAddr;

use actix_web::{HttpRequest, HttpResponse, http::header, post, web};
use desk_ipc_protocol::message::{
    AuthorizeWaylandPortalPayload, CancelWaylandPortalPayload, ServiceToWorker,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    error::DeskError,
    model::{info::WaylandAuthorizationTarget, settings_coordinator::SettingsCoordinator},
};

pub const TAG: &str = "Host readiness";

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AuthorizeWaylandRequest {
    pub operation_id: String,
    pub target: WaylandAuthorizationTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CancelWaylandRequest {
    pub operation_id: String,
    pub generation: u64,
}

fn permission_error(message: &str) -> DeskError {
    DeskError::new_custom_error(DeskErrorCode::PERMISSION_ERROR, message)
}

fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| mapped.is_loopback())
        }
    }
}

fn is_loopback_url_host(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => is_loopback_ip(IpAddr::V6(ip)),
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

#[allow(clippy::result_large_err)]
pub(super) fn validate_local_mutation(req: &HttpRequest) -> Result<(), DeskError> {
    let peer = req
        .peer_addr()
        .ok_or_else(|| permission_error("Local permission requests require a TCP peer address"))?;
    if !is_loopback_ip(peer.ip()) {
        return Err(permission_error(
            "Local permission requests are restricted to the loopback interface",
        ));
    }

    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| permission_error("Local permission requests require an Origin header"))?;
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| permission_error("Local permission requests require a Host header"))?;
    let origin_url = url::Url::parse(origin)
        .map_err(|_| permission_error("Local permission request Origin is invalid"))?;
    if !matches!(origin_url.scheme(), "http" | "https") {
        return Err(permission_error(
            "Local permission request Origin must use HTTP or HTTPS",
        ));
    }
    let expected = url::Url::parse(&format!("{}://{host}/", origin_url.scheme()))
        .map_err(|_| permission_error("Local permission request Host is invalid"))?;
    if origin_url.origin() != expected.origin() {
        return Err(permission_error(
            "Local permission request Origin does not match Host",
        ));
    }
    if !is_loopback_url_host(&origin_url) {
        return Err(permission_error(
            "Open this host UI locally using localhost or 127.0.0.1 to request desktop permissions",
        ));
    }
    Ok(())
}

#[allow(clippy::result_large_err)]
fn current_worker(
    coordinator: &SettingsCoordinator,
) -> Result<crate::daemon::worker_manager::WorkerManager, DeskError> {
    coordinator.worker_manager().ok_or_else(|| {
        DeskError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "No active desktop worker is available",
        )
    })
}

#[utoipa::path(
    tag = TAG,
    summary = "Start local Wayland Portal authorization",
    request_body(content = AuthorizeWaylandRequest),
    responses(
        (status = 202, description = "Authorization command queued"),
    ),
)]
#[post("/host-readiness/wayland/authorize")]
pub async fn authorize_wayland(
    req: HttpRequest,
    request: web::Json<AuthorizeWaylandRequest>,
    coordinator: web::Data<SettingsCoordinator>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let request = request.into_inner();
    if request.operation_id.is_empty() || request.operation_id.len() > 128 {
        return Err(DeskError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "operation_id must contain between 1 and 128 bytes",
        ));
    }
    let worker = current_worker(&coordinator)?;
    worker
        .send_to_worker(ServiceToWorker::AuthorizeWaylandPortal(
            AuthorizeWaylandPortalPayload {
                operation_id: request.operation_id,
                target: request.target.into(),
            },
        ))
        .await
        .map_err(|error| {
            DeskError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                &format!("Could not queue Wayland authorization: {error}"),
            )
        })?;
    Ok(HttpResponse::Accepted().json(RestResponse::succeed()))
}

#[utoipa::path(
    tag = TAG,
    summary = "Cancel local Wayland Portal authorization",
    request_body(content = CancelWaylandRequest),
    responses(
        (status = 202, description = "Cancellation command queued"),
    ),
)]
#[post("/host-readiness/wayland/cancel")]
pub async fn cancel_wayland(
    req: HttpRequest,
    request: web::Json<CancelWaylandRequest>,
    coordinator: web::Data<SettingsCoordinator>,
) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    let request = request.into_inner();
    let worker = current_worker(&coordinator)?;
    worker
        .send_to_worker(ServiceToWorker::CancelWaylandPortal(
            CancelWaylandPortalPayload {
                operation_id: request.operation_id,
                generation: request.generation,
            },
        ))
        .await
        .map_err(|error| {
            DeskError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                &format!("Could not queue Wayland authorization cancellation: {error}"),
            )
        })?;
    Ok(HttpResponse::Accepted().json(RestResponse::succeed()))
}

#[utoipa::path(
    tag = TAG,
    summary = "Request local macOS desktop permissions",
    responses(
        (status = 202, description = "Permission request started"),
    ),
)]
#[post("/host-readiness/macos/request-permissions")]
pub async fn request_macos_permissions(req: HttpRequest) -> Result<HttpResponse, DeskError> {
    validate_local_mutation(&req)?;
    #[cfg(target_os = "macos")]
    crate::macos_permissions::request();
    #[cfg(not(target_os = "macos"))]
    return Err(DeskError::new_custom_error(
        DeskErrorCode::FEATURE_UNAVAILABLE,
        "macOS permissions are unavailable on this platform",
    ));
    #[cfg(target_os = "macos")]
    Ok(HttpResponse::Accepted().json(RestResponse::succeed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test as actix_test;

    #[test]
    fn local_mutation_requires_loopback_and_matching_origin() {
        let allowed = actix_test::TestRequest::default()
            .peer_addr("127.0.0.1:43210".parse().unwrap())
            .insert_header((header::HOST, "127.0.0.1:8080"))
            .insert_header((header::ORIGIN, "http://127.0.0.1:8080"))
            .to_http_request();
        assert!(validate_local_mutation(&allowed).is_ok());

        let mapped_loopback = actix_test::TestRequest::default()
            .peer_addr("[::ffff:127.0.0.1]:43210".parse().unwrap())
            .insert_header((header::HOST, "localhost:5174"))
            .insert_header((header::ORIGIN, "http://localhost:5174"))
            .to_http_request();
        assert!(validate_local_mutation(&mapped_loopback).is_ok());

        let remote = actix_test::TestRequest::default()
            .peer_addr("192.0.2.10:43210".parse().unwrap())
            .insert_header((header::HOST, "127.0.0.1:8080"))
            .insert_header((header::ORIGIN, "http://127.0.0.1:8080"))
            .to_http_request();
        assert!(validate_local_mutation(&remote).is_err());

        let remote_through_local_dev_proxy = actix_test::TestRequest::default()
            .peer_addr("[::ffff:127.0.0.1]:43210".parse().unwrap())
            .insert_header((header::HOST, "192.0.2.20:5174"))
            .insert_header((header::ORIGIN, "http://192.0.2.20:5174"))
            .to_http_request();
        assert!(validate_local_mutation(&remote_through_local_dev_proxy).is_err());

        let cross_origin = actix_test::TestRequest::default()
            .peer_addr("[::1]:43210".parse().unwrap())
            .insert_header((header::HOST, "127.0.0.1:8080"))
            .insert_header((header::ORIGIN, "http://evil.example"))
            .to_http_request();
        assert!(validate_local_mutation(&cross_origin).is_err());
    }
}
