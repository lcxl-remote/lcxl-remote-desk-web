/// Service management endpoints — available in all modes but only functional
/// when running inside Tauri. After the host-control-hub unification (Step 6)
/// the install / uninstall command travels over the same `/ws/tauri_ipc` link
/// as every other Tauri-bound command, so the endpoint just publishes a hub
/// message and returns 202.
use std::sync::Arc;

use actix_web::{HttpResponse, post, web};
use desk_utils::rest::RestResponse;
use serde::Deserialize;
use utoipa::ToSchema;

use crate::{
    daemon::windows_service::default_install_dir,
    error::DeskError,
    host_control::{HostControlHub, HostControlMessage, ServiceOpKind},
};

/// Request body for `POST /api/service/install`.
#[derive(Debug, Default, Deserialize, serde::Serialize, ToSchema)]
pub struct InstallServiceRequest {
    /// Installation directory. Uses the platform default when absent.
    pub install_path: Option<String>,
}

/// Request the host (Tauri) to install the OS system service.
///
/// Stateless: the handler publishes a `ServiceOp` command on the host control
/// hub and returns 202 Accepted immediately. The caller should poll
/// `GET /api/server_info` to check `service_installed`.
#[utoipa::path(
    summary = "Install OS system service",
    request_body = InstallServiceRequest,
    responses(
        (status = 202, description = "Install request accepted"),
        (status = 503, description = "No host control hub or no Tauri shell connected"),
    ),
)]
#[post("/api/service/install")]
pub async fn install_service(
    hub: web::Data<Option<Arc<HostControlHub>>>,
    req: Option<web::Json<InstallServiceRequest>>,
) -> Result<HttpResponse, DeskError> {
    let install_path = req
        .and_then(|r| r.into_inner().install_path)
        .unwrap_or_else(default_install_dir);

    dispatch_service_op(
        hub.as_ref().as_ref(),
        HostControlMessage::ServiceOp {
            op: ServiceOpKind::Install,
            install_path: Some(install_path),
        },
        "Install request accepted",
    )
}

/// Request the host (Tauri) to uninstall the OS system service.
#[utoipa::path(
    summary = "Uninstall OS system service",
    responses(
        (status = 202, description = "Uninstall request accepted"),
        (status = 503, description = "No host control hub or no Tauri shell connected"),
    ),
)]
#[post("/api/service/uninstall")]
pub async fn uninstall_service(
    hub: web::Data<Option<Arc<HostControlHub>>>,
) -> Result<HttpResponse, DeskError> {
    dispatch_service_op(
        hub.as_ref().as_ref(),
        HostControlMessage::ServiceOp {
            op: ServiceOpKind::Uninstall,
            install_path: None,
        },
        "Uninstall request accepted",
    )
}

fn dispatch_service_op(
    hub: Option<&Arc<HostControlHub>>,
    msg: HostControlMessage,
    accepted_message: &str,
) -> Result<HttpResponse, DeskError> {
    let Some(hub) = hub else {
        return Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "No host control hub configured".into(),
            )),
        );
    };

    if !hub.has_tauri_ui() {
        return Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                "No Tauri shell connected".into(),
            )),
        );
    }

    match hub.send_command(msg) {
        Ok(_) => Ok(
            HttpResponse::Accepted().json(RestResponse::<()>::succeed_with_message(
                accepted_message.into(),
            )),
        ),
        Err(e) => Ok(
            HttpResponse::ServiceUnavailable().json(RestResponse::<()>::failed(
                crate::error::DeskErrorCode::SYSTEM_ERROR,
                format!("Service op dispatch failed: {e}"),
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};

    fn build_app(
        hub: Option<Arc<HostControlHub>>,
    ) -> App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        App::new()
            .app_data(web::Data::new(hub))
            .service(install_service)
            .service(uninstall_service)
    }

    /// Without a hub configured, both install and uninstall return 503 with a
    /// useful body.
    #[actix_web::test]
    async fn install_without_hub_returns_503() {
        let app = test::init_service(build_app(None)).await;
        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .set_json(InstallServiceRequest::default())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 503);
    }

    /// With a hub but no Tauri subscriber, dispatch still rejects with 503 so
    /// the caller is not led to believe the install was queued.
    #[actix_web::test]
    async fn install_without_tauri_subscriber_returns_503() {
        let hub = Arc::new(HostControlHub::new_local());
        let app = test::init_service(build_app(Some(hub))).await;
        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .set_json(InstallServiceRequest::default())
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 503);
    }

    /// Once a Tauri subscriber is registered the dispatch succeeds (202) and
    /// the broadcast carries a `ServiceOp` message.
    #[actix_web::test]
    async fn install_with_tauri_subscriber_returns_202() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let app = test::init_service(build_app(Some(Arc::clone(&hub)))).await;

        let req = test::TestRequest::post()
            .uri("/api/service/install")
            .set_json(InstallServiceRequest {
                install_path: Some("C:/foo".to_string()),
            })
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202);

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        match msg {
            HostControlMessage::ServiceOp { op, install_path } => {
                assert!(matches!(op, ServiceOpKind::Install));
                assert_eq!(install_path.as_deref(), Some("C:/foo"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[actix_web::test]
    async fn uninstall_with_tauri_subscriber_returns_202() {
        let hub = Arc::new(HostControlHub::new_local());
        let mut rx = hub.subscribe_outbound();
        hub.mark_tauri_connected();
        let app = test::init_service(build_app(Some(Arc::clone(&hub)))).await;

        let req = test::TestRequest::post()
            .uri("/api/service/uninstall")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 202);

        let msg = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("must receive")
            .expect("not lagged");
        assert!(matches!(
            msg,
            HostControlMessage::ServiceOp {
                op: ServiceOpKind::Uninstall,
                install_path: None,
            }
        ));
    }
}
