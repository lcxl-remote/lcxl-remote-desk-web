/// Service management endpoints — available in all modes but only functional
/// when running inside Tauri (i.e. when `service_op_sender` is present in
/// `ExternalChannels`).
use actix_web::{HttpResponse, post, web};
use desk_utils::rest::RestResponse;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::{ServiceOp, daemon::windows_service::default_install_dir, error::DeskError};

/// Request body for `POST /api/service/install`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct InstallServiceRequest {
    /// Installation directory. Uses the platform default when absent.
    pub install_path: Option<String>,
}

/// Request the host (Tauri) to install the OS system service.
///
/// The HTTP handler is stateless: it sends a command to Tauri via the
/// `service_op_sender` channel and returns 202 Accepted immediately.
/// The caller should poll `GET /api/server_info` to check `service_installed`.
#[utoipa::path(
    summary = "Install OS system service",
    request_body = InstallServiceRequest,
    responses(
        (status = 202, description = "Install request accepted"),
        (status = 503, description = "Not running inside Tauri (no service op channel)"),
    ),
)]
#[post("/api/service/install")]
pub async fn install_service(
    sender: web::Data<Option<std::sync::mpsc::SyncSender<ServiceOp>>>,
    req: Option<web::Json<InstallServiceRequest>>,
) -> Result<HttpResponse, DeskError> {
    let install_path = req
        .and_then(|r| r.into_inner().install_path)
        .unwrap_or_else(default_install_dir);

    match sender.as_ref() {
        Some(tx) => {
            match tx.try_send(ServiceOp::Install { install_path }) {
                Ok(_) => Ok(HttpResponse::Accepted().json(
                    RestResponse::<()>::succeed_with_message("Install request accepted".into()),
                )),
                Err(_) => Ok(
                    HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                        crate::error::DeskErrorCode::SYSTEM_ERROR,
                        "Service op channel is unavailable".into(),
                    )),
                ),
            }
        }
        None => Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "Not running in Tauri mode".into(),
            )),
        ),
    }
}

/// Request the host (Tauri) to uninstall the OS system service.
#[utoipa::path(
    summary = "Uninstall OS system service",
    responses(
        (status = 202, description = "Uninstall request accepted"),
        (status = 503, description = "Not running inside Tauri (no service op channel)"),
    ),
)]
#[post("/api/service/uninstall")]
pub async fn uninstall_service(
    sender: web::Data<Option<std::sync::mpsc::SyncSender<ServiceOp>>>,
) -> Result<HttpResponse, DeskError> {
    match sender.as_ref() {
        Some(tx) => {
            match tx.try_send(ServiceOp::Uninstall) {
                Ok(_) => Ok(HttpResponse::Accepted().json(
                    RestResponse::<()>::succeed_with_message("Uninstall request accepted".into()),
                )),
                Err(_) => Ok(
                    HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                        crate::error::DeskErrorCode::SYSTEM_ERROR,
                        "Service op channel is unavailable".into(),
                    )),
                ),
            }
        }
        None => Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "Not running in Tauri mode".into(),
            )),
        ),
    }
}
