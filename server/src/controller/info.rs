use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use sysinfo::System;

use crate::{
    error::DeskError,
    model::{
        image_capture::ImageCaptureTypeHelper as _,
        info::{BackendInfo, ServerInfo, SystemInfo},
        settings::{SharedSettings, StartupMode},
    },
};
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::model::desk_settings::DeskSettings;

#[cfg(target_os = "linux")]
use crate::service::image_capture::portal_client::PortalClient;
#[cfg(target_os = "linux")]
use crate::service::wayland_remote_desktop::WaylandRemoteDesktop;

#[utoipa::path(
    summary = "Get system information",
    responses(
        (status = 200, description = "Get system information successfully", body=RestResponse<SystemInfo>),
    ),
)]
#[get("/sysinfo")]
pub async fn query_sysinfo(settings: web::Data<SharedSettings>) -> Result<HttpResponse, DeskError> {
    let system_settings = {
        let settings = settings.read().await;
        settings.system.clone()
    };
    log::info!(
        "Query settings successfully, settings: {:?}",
        system_settings
    );

    let mut sys = System::new_all();
    sys.refresh_all();
    let mut system_info = SystemInfo::from(&sys);
    let startup_mode = {
        let settings = settings.read().await;
        settings.args.startup_mode.clone()
    };
    system_info.startup_mode = startup_mode.clone();
    system_info.is_admin = if startup_mode != StartupMode::Signaling {
        Some(desk_utils::permission::is_admin())
    } else {
        None
    };
    log::info!(
        "Get system information successfully, info: {:?}",
        system_info
    );
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(system_info)))
}

#[utoipa::path(
    summary = "Get server information",
    responses(
        (status = 200, description = "Get server information successfully", body=RestResponse<ServerInfo>),
    ),
)]
#[get("/api/server_info")]
pub async fn query_server_info(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    let (startup_mode, initialized) = {
        let settings = settings.read().await;
        let mode = settings.args.startup_mode.clone();
        let mode_str = mode.as_ref().to_string();
        let init = !settings.user.login_password.is_empty();
        (mode_str, init)
    };

    let service_installed = desk_utils::permission::is_service_installed("LcxlDeskService");
    let is_admin = desk_utils::permission::is_admin();

    let info = ServerInfo {
        startup_mode,
        api_version: SERVER_API_VERSION,
        initialized,
        service_installed,
        is_admin,
    };

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(info)))
}

#[utoipa::path(
    summary = "Get backend diagnostics",
    responses(
        (status = 200, description = "Get backend diagnostics successfully", body=RestResponse<BackendInfo>),
    ),
)]
#[get("/backend_info")]
pub async fn query_backend_info(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    let desk_settings: DeskSettings = {
        let guard = settings.read().await;
        guard.desk.clone()
    };

    let resolved = desk_settings.get_image_capture_type()?;
    let mut backend_info = BackendInfo {
        os: std::env::consts::OS.to_string(),
        wayland_env: std::env::var("WAYLAND_DISPLAY").is_ok(),
        x11_env: std::env::var("DISPLAY").is_ok(),
        requested_image_capture: desk_settings.image_capture.clone(),
        resolved_image_capture: Into::<&'static str>::into(resolved).to_string(),
        resolved_input_control: "native".to_string(),
        input_backend_runtime_status: "ready".to_string(),
        input_backend_error: None,
        portal_available: None,
        portal_error: None,
    };

    #[cfg(target_os = "linux")]
    {
        let mode = desk_settings
            .wayland_control_mode
            .as_deref()
            .unwrap_or("auto");
        backend_info.resolved_input_control = match mode {
            "none" => "none".to_string(),
            "uinput" => "uinput".to_string(),
            "portal" => "portal".to_string(),
            _ => {
                if backend_info.wayland_env {
                    "portal(auto)".to_string()
                } else {
                    "uinput(auto)".to_string()
                }
            }
        };

        let remote_desktop_probe = WaylandRemoteDesktop::probe_portal();
        match mode {
            "none" => {
                backend_info.input_backend_runtime_status = "disabled".to_string();
            }
            "uinput" => {
                backend_info.input_backend_runtime_status = "ready(uinput)".to_string();
            }
            "portal" => match remote_desktop_probe {
                Ok(_) => {
                    backend_info.input_backend_runtime_status = "ready(portal)".to_string();
                }
                Err(e) => {
                    backend_info.input_backend_runtime_status = "error(portal)".to_string();
                    backend_info.input_backend_error = Some(e.to_string());
                }
            },
            _ => {
                if backend_info.wayland_env {
                    match remote_desktop_probe {
                        Ok(_) => {
                            backend_info.input_backend_runtime_status =
                                "ready(portal-auto)".to_string();
                        }
                        Err(e) => {
                            backend_info.input_backend_runtime_status =
                                "fallback(uinput-auto)".to_string();
                            backend_info.input_backend_error = Some(e.to_string());
                        }
                    }
                } else {
                    backend_info.input_backend_runtime_status = "ready(uinput-auto)".to_string();
                }
            }
        }

        match PortalClient::new() {
            Ok(_) => backend_info.portal_available = Some(true),
            Err(e) => {
                backend_info.portal_available = Some(false);
                backend_info.portal_error = Some(e.to_string());
            }
        }
    }

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(backend_info)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use actix_web::{App, test};

    #[actix_web::test]
    async fn test_query_sysinfo() {
        let settings = SharedSettings::from(Settings::default());
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .service(query_sysinfo),
        )
        .await;

        let req = test::TestRequest::get().uri("/sysinfo").to_request();
        let resp = test::call_service(&app, req).await;

        assert!(resp.status().is_success());

        let body: RestResponse<SystemInfo> = test::read_body_json(resp).await;
        assert_eq!(body.code, 200);
        assert!(body.data.is_some());
    }

    #[actix_web::test]
    async fn test_query_backend_info() {
        let settings = SharedSettings::from(Settings::default());
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .service(query_backend_info),
        )
        .await;

        let req = test::TestRequest::get().uri("/backend_info").to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: RestResponse<BackendInfo> = test::read_body_json(resp).await;
        assert!(body.success);
        let data = body.data.expect("backend info data should be present");
        assert!(!data.resolved_image_capture.is_empty());
        assert!(!data.resolved_input_control.is_empty());
        assert!(!data.input_backend_runtime_status.is_empty());
    }
}
