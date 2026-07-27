use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use sysinfo::System;

use crate::{
    TauriIsAdminOverride,
    error::DeskError,
    model::{
        info::{BackendInfo, MacosAutologin, ServerInfo, SystemInfo},
        settings::{SharedSettings, StartupMode},
    },
};
use desk_capture_engine::model::image_capture::ImageCaptureTypeHelper as _;
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::model::desk_settings::DeskSettings;

#[cfg(target_os = "linux")]
use desk_capture_engine::image_capture::portal_client::PortalClient;
#[cfg(target_os = "linux")]
use desk_input_injection::service::wayland_remote_desktop::WaylandRemoteDesktop;

pub const TAG: &str = "System";

#[utoipa::path(
    tag = TAG,
    summary = "Get system information",
    responses(
        (status = 200, description = "Get system information successfully", body=RestResponse<SystemInfo>),
    ),
)]
#[get("/sysinfo")]
pub async fn query_sysinfo(
    settings: web::Data<SharedSettings>,
    tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
) -> Result<HttpResponse, DeskError> {
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
        let override_val = tauri_is_admin.as_ref().and_then(|a| *a.lock().unwrap());
        Some(override_val.unwrap_or_else(desk_utils::permission::is_admin))
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
    tag = TAG,
    summary = "Get server information",
    responses(
        (status = 200, description = "Get server information successfully", body=RestResponse<ServerInfo>),
    ),
)]
#[get("/api/server_info")]
pub async fn query_server_info(
    settings: web::Data<SharedSettings>,
    tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
) -> Result<HttpResponse, DeskError> {
    let (startup_mode, initialized) = {
        let settings = settings.read().await;
        let mode = settings.args.startup_mode.clone();
        let init = !settings.user.login_password.is_empty();
        (mode, init)
    };

    let service_installed = desk_utils::permission::is_service_installed("LcxlDeskService");
    // In ServiceDaemon mode the process runs as SYSTEM (is_admin always true).
    // Use the override reported by the Tauri process when available.
    let is_admin = tauri_is_admin
        .as_ref()
        .and_then(|a| *a.lock().unwrap())
        .unwrap_or_else(desk_utils::permission::is_admin);
    let server_binary_available = server_binary_available();
    let default_install_path = crate::daemon::windows_service::default_install_dir();

    // macOS uses a LaunchAgent (not the OS-service path), so it reports
    // background_start + TCC grants instead; other platforms leave both None.
    #[cfg(target_os = "macos")]
    let (background_start, macos_permissions) = {
        let s = crate::macos_agent::status();
        (
            Some(crate::model::info::BackgroundStart {
                configured: s.configured,
                loaded: s.loaded,
                path_valid: s.path_valid,
            }),
            Some(crate::macos_permissions::probe()),
        )
    };
    #[cfg(not(target_os = "macos"))]
    let (background_start, macos_permissions) = (None, None);

    let info = ServerInfo {
        startup_mode,
        api_version: SERVER_API_VERSION,
        initialized,
        service_installed,
        is_admin,
        server_binary_available,
        default_install_path,
        background_start,
        macos_permissions,
    };

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(info)))
}

#[utoipa::path(
    tag = TAG,
    summary = "Get macOS automatic-login helper status",
    responses(
        (status = 200, description = "Get macOS automatic-login status successfully", body=RestResponse<MacosAutologin>),
    ),
)]
#[get("/macos/autologin")]
pub async fn query_macos_autologin() -> Result<HttpResponse, DeskError> {
    // Read-only probe. On macOS this shells out to two fast, non-mutating
    // diagnostic commands (`fdesetup isactive`, `sysadminctl -autologin status`);
    // it is settings-page-only (low frequency), mirroring the synchronous probe
    // already done in `query_server_info`.
    let info = macos_autologin_status();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(info)))
}

/// Build the [`MacosAutologin`] DTO. The app never handles the plaintext
/// password: it reports read-only state and emits guided commands whose
/// `-password -` makes `sysadminctl` prompt for the password interactively.
#[cfg(target_os = "macos")]
fn macos_autologin_status() -> MacosAutologin {
    let status = crate::macos_autologin::probe();
    let current_user = std::env::var("USER").ok().filter(|u| !u.is_empty());
    // Pre-fill the manual command with the live user when known, else a visible
    // placeholder the user can edit before running it.
    let username_for_cmd = current_user.clone().unwrap_or_else(|| "<user>".to_string());
    MacosAutologin {
        supported: true,
        filevault_enabled: status.filevault_enabled,
        configured: status.autologin_user.is_some(),
        available: !status.filevault_enabled,
        autologin_user: status.autologin_user,
        current_user,
        enable_command: crate::macos_autologin::build_enable_command(&username_for_cmd),
        disable_command: crate::macos_autologin::disable_command().to_string(),
    }
}

/// Non-macOS platforms have no automatic-login helper; report it as unsupported
/// so the wire shape stays identical while the UI hides the card.
#[cfg(not(target_os = "macos"))]
fn macos_autologin_status() -> MacosAutologin {
    MacosAutologin {
        supported: false,
        filevault_enabled: false,
        configured: false,
        autologin_user: None,
        available: false,
        current_user: None,
        enable_command: String::new(),
        disable_command: String::new(),
    }
}

/// Check whether the `lcxl-remote-desk-server` binary is available for service
/// installation.
///
/// Returns `true` when:
/// - The current executable IS `lcxl-remote-desk-server` (standalone mode), or
/// - `lcxl-remote-desk-server(.exe)` exists in the same directory as the current
///   executable (Tauri mode — both binaries share the same target/ directory).
fn server_binary_available() -> bool {
    let Ok(current_exe) = std::env::current_exe() else {
        return false;
    };

    // Running as the server binary itself — can always self-install.
    if current_exe
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s == "lcxl-remote-desk-server")
        .unwrap_or(false)
    {
        return true;
    }

    // Running inside Tauri (or another host) — look for the binary alongside.
    let Some(dir) = current_exe.parent() else {
        return false;
    };
    #[cfg(target_os = "windows")]
    let name = "lcxl-remote-desk-server.exe";
    #[cfg(not(target_os = "windows"))]
    let name = "lcxl-remote-desk-server";

    dir.join(name).exists()
}

#[utoipa::path(
    tag = TAG,
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
    // Only the `#[cfg(target_os = "linux")]` block below mutates this; on
    // other platforms the binding is left untouched.
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
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

        let status = resp.status();
        if !status.is_success() {
            let body_bytes = test::read_body(resp).await;
            let body_str = std::str::from_utf8(&body_bytes).unwrap_or("could not parse body");
            panic!("Request failed with status: {}, body: {}", status, body_str);
        }

        let body: RestResponse<SystemInfo> = test::read_body_json(resp).await;
        assert_eq!(body.code, 0);
        assert!(body.data.is_some());
    }

    // Ignored by default: `query_backend_info` probes the input backend via
    // `WaylandRemoteDesktop::probe_portal()`, which pops an `xdg-desktop-portal`
    // RemoteDesktop permission dialog on a Wayland session and blocks
    // indefinitely. Run explicitly with `--ignored` on a non-interactive host.
    #[ignore = "probes the RemoteDesktop portal; hangs on a Wayland portal prompt"]
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
