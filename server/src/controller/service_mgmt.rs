/// Service management endpoints — available in all modes but only functional
/// when running inside Tauri (i.e. when `service_op_sender` is present in
/// `ExternalChannels`).
use actix_web::{HttpResponse, post, web};
use desk_utils::rest::RestResponse;

use crate::{ServiceOp, error::DeskError};

/// Request the host (Tauri) to install the OS system service.
///
/// The HTTP handler is stateless: it sends a command to Tauri via the
/// `service_op_sender` channel and returns 202 Accepted immediately.
/// The caller should poll `GET /api/server_info` to check `service_installed`.
#[utoipa::path(
    summary = "Install OS system service",
    responses(
        (status = 202, description = "Install request accepted"),
        (status = 503, description = "Not running inside Tauri (no service op channel)"),
    ),
)]
#[post("/api/service/install")]
pub async fn install_service(
    sender: web::Data<Option<std::sync::mpsc::SyncSender<ServiceOp>>>,
) -> Result<HttpResponse, DeskError> {
    match sender.as_ref() {
        Some(tx) => {
            let _ = tx.try_send(ServiceOp::Install);
            Ok(
                HttpResponse::Accepted().json(RestResponse::<()>::succeed_with_message(
                    "Install request accepted".into(),
                )),
            )
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
            let _ = tx.try_send(ServiceOp::Uninstall);
            Ok(
                HttpResponse::Accepted().json(RestResponse::<()>::succeed_with_message(
                    "Uninstall request accepted".into(),
                )),
            )
        }
        None => Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "Not running in Tauri mode".into(),
            )),
        ),
    }
}
