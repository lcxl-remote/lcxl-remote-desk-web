use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use sysinfo::System;

use crate::{
    TauriIsAdminOverride,
    error::DeskError,
    model::{
        info::{BackendInfo, MacosAutologin, ServerInfo, SystemInfo, WaylandPortalInfo},
        settings::{SharedSettings, StartupMode},
    },
};
use desk_capture_engine::model::image_capture::ImageCaptureTypeHelper as _;
use desk_server_version::SERVER_API_VERSION;
use desk_signal_facade::model::desk_settings::{DeskSettings, LinuxInputControlMode};

#[cfg(target_os = "linux")]
use crate::model::info::{
    BackendDiagnosticItem, BackendDiagnosticSection, BackendDiagnosticStatus,
};
#[cfg(target_os = "linux")]
use desk_utils::linux_display::{LinuxDisplayServer, detect_linux_display_environment};
#[cfg(target_os = "linux")]
use desk_wayland_portal::{PortalPhase, PortalSnapshot};

#[cfg(target_os = "linux")]
fn configured_input_mode(value: Option<&str>) -> LinuxInputControlMode {
    value
        .and_then(LinuxInputControlMode::parse)
        .unwrap_or(LinuxInputControlMode::Auto)
}

#[cfg(target_os = "linux")]
fn resolved_input_control_label(
    mode: LinuxInputControlMode,
    active_server: LinuxDisplayServer,
) -> &'static str {
    match mode {
        LinuxInputControlMode::Auto if matches!(active_server, LinuxDisplayServer::Wayland) => {
            "portal(auto)"
        }
        LinuxInputControlMode::Auto => "uinput(auto)",
        mode => mode.as_str(),
    }
}

#[cfg(target_os = "linux")]
fn remote_desktop_diagnostic(
    snapshot: Option<&PortalSnapshot>,
    automatic: bool,
) -> (String, BackendDiagnosticStatus, Option<String>) {
    let (ready_value, error_value, error_status) = if automatic {
        (
            "ready(portal-auto)",
            "waiting(portal-auto)",
            BackendDiagnosticStatus::Warning,
        )
    } else {
        (
            "ready(portal)",
            "waiting(portal)",
            BackendDiagnosticStatus::Error,
        )
    };
    match snapshot {
        Some(snapshot) if snapshot.admits(true) => (
            ready_value.to_string(),
            BackendDiagnosticStatus::Ready,
            Some(format!(
                "Portal generation {} is ready",
                snapshot.generation
            )),
        ),
        Some(snapshot) => (
            error_value.to_string(),
            error_status,
            Some(
                snapshot
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("Portal session state is {:?}", snapshot.phase)),
            ),
        ),
        None => (
            error_value.to_string(),
            error_status,
            Some("Desktop worker has not reported Portal readiness".to_string()),
        ),
    }
}

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
    coordinator: web::Data<crate::model::settings_coordinator::SettingsCoordinator>,
) -> Result<HttpResponse, DeskError> {
    let (startup_mode, initialized, wayland_default_mode) = {
        let settings = settings.read().await;
        let mode = settings.args.startup_mode.clone();
        let init = !settings.user.login_password.is_empty();
        (mode, init, settings.desk.wayland_control_mode.clone())
    };
    #[cfg(not(target_os = "linux"))]
    let _ = &wayland_default_mode;

    #[cfg(target_os = "linux")]
    let platform_service_name = crate::daemon::linux_service::SERVICE_UNIT_NAME;
    #[cfg(not(target_os = "linux"))]
    let platform_service_name = "LcxlDeskService";
    let service_installed = desk_utils::permission::is_service_installed(platform_service_name);
    let service_running = desk_utils::permission::is_service_running(platform_service_name);
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

    #[cfg(target_os = "linux")]
    let wayland_portal = if coordinator
        .worker_manager()
        .is_some_and(|worker| worker.linux_display_server() == LinuxDisplayServer::Wayland)
    {
        let mut info = coordinator
            .worker_manager()
            .and_then(|worker| worker.wayland_portal_snapshot())
            .map(WaylandPortalInfo::from)
            .unwrap_or_else(WaylandPortalInfo::worker_unavailable);
        info.recommended_target = match wayland_default_mode.as_deref() {
            Some("none" | "uinput") => crate::model::info::WaylandAuthorizationTarget::ScreenOnly,
            _ => crate::model::info::WaylandAuthorizationTarget::ScreenAndInput,
        };
        Some(info)
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let wayland_portal = None;

    let info = ServerInfo {
        platform: std::env::consts::OS.to_string(),
        startup_mode,
        api_version: SERVER_API_VERSION,
        initialized,
        service_installed,
        service_running,
        is_admin,
        server_binary_available,
        default_install_path,
        background_start,
        macos_permissions,
        wayland_portal,
        device_assistant: Some(crate::model::info::DeviceAssistantClientCapabilities::oss()),
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
    coordinator: web::Data<crate::model::settings_coordinator::SettingsCoordinator>,
) -> Result<HttpResponse, DeskError> {
    let desk_settings: DeskSettings = {
        let guard = settings.read().await;
        guard.desk.clone()
    };

    let resolved_image_capture = desk_settings
        .get_image_capture_type()
        .map(|value| Into::<&'static str>::into(value).to_string())
        .unwrap_or_else(|error| {
            log::warn!("Backend diagnostics could not resolve image capture: {error}");
            "<unavailable>".to_string()
        });
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut backend_info = BackendInfo {
        os: std::env::consts::OS.to_string(),
        requested_image_capture: desk_settings.image_capture.clone(),
        resolved_image_capture,
        resolved_input_control: "native".to_string(),
        input_backend_runtime_status: "ready".to_string(),
        platform_diagnostics: Vec::new(),
    };

    #[cfg(target_os = "linux")]
    {
        let environment = detect_linux_display_environment();
        let configured_mode = desk_settings.wayland_control_mode.as_deref();
        let mode = configured_input_mode(configured_mode);
        if configured_mode
            .is_some_and(|value| !matches!(value, "auto" | "none" | "uinput" | "portal"))
        {
            log::warn!(
                "Unknown wayland_control_mode {:?}; treating it as auto",
                configured_mode
            );
        }
        let active_server = environment.active_server();
        backend_info.resolved_input_control =
            resolved_input_control_label(mode, active_server).to_string();

        let portal_snapshot = coordinator
            .worker_manager()
            .and_then(|worker| worker.wayland_portal_snapshot());
        let (input_value, input_status, input_detail) = match mode {
            LinuxInputControlMode::None => (
                "disabled".to_string(),
                BackendDiagnosticStatus::Warning,
                None,
            ),
            LinuxInputControlMode::Uinput => (
                "ready(uinput)".to_string(),
                BackendDiagnosticStatus::Ready,
                None,
            ),
            LinuxInputControlMode::Portal => {
                remote_desktop_diagnostic(portal_snapshot.as_ref(), false)
            }
            LinuxInputControlMode::Auto if matches!(active_server, LinuxDisplayServer::Wayland) => {
                remote_desktop_diagnostic(portal_snapshot.as_ref(), true)
            }
            LinuxInputControlMode::Auto => (
                "ready(uinput-auto)".to_string(),
                BackendDiagnosticStatus::Ready,
                None,
            ),
        };
        backend_info.input_backend_runtime_status = input_value.clone();

        let (portal_value, portal_status, portal_detail) =
            if environment.active_server() == LinuxDisplayServer::Wayland {
                match portal_snapshot.as_ref() {
                    Some(snapshot)
                        if snapshot.phase != PortalPhase::Unsupported
                            && snapshot.availability.monitor_available =>
                    {
                        (
                            "available".to_string(),
                            BackendDiagnosticStatus::Ready,
                            Some(format!(
                                "AvailableSourceTypes={}",
                                snapshot.availability.available_source_types
                            )),
                        )
                    }
                    Some(snapshot) => (
                        "unavailable".to_string(),
                        BackendDiagnosticStatus::Error,
                        snapshot.reason.clone().or_else(|| {
                            Some(format!("Portal session state is {:?}", snapshot.phase))
                        }),
                    ),
                    None => (
                        "unavailable".to_string(),
                        BackendDiagnosticStatus::Error,
                        Some("Desktop worker has not reported Portal readiness".to_string()),
                    ),
                }
            } else {
                (
                    "unavailable".to_string(),
                    BackendDiagnosticStatus::Neutral,
                    Some("ScreenCast Portal is only advertised in a Wayland session".to_string()),
                )
            };

        backend_info
            .platform_diagnostics
            .push(BackendDiagnosticSection {
                platform: "linux".to_string(),
                key: "linux_display".to_string(),
                items: vec![
                    BackendDiagnosticItem {
                        key: "wayland_display".to_string(),
                        value: environment.wayland_present.to_string(),
                        status: BackendDiagnosticStatus::Neutral,
                        detail: None,
                    },
                    BackendDiagnosticItem {
                        key: "x11_display".to_string(),
                        value: environment.x11_present.to_string(),
                        status: BackendDiagnosticStatus::Neutral,
                        detail: None,
                    },
                    BackendDiagnosticItem {
                        key: "remote_desktop_input".to_string(),
                        value: input_value,
                        status: input_status,
                        detail: input_detail,
                    },
                    BackendDiagnosticItem {
                        key: "screencast_portal".to_string(),
                        value: portal_value,
                        status: portal_status,
                        detail: portal_detail,
                    },
                ],
            });
    }

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(backend_info)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use actix_web::{App, test};

    #[cfg(target_os = "linux")]
    #[actix_web::test]
    async fn linux_input_control_mode_normalizes_unknown_values_to_auto() {
        assert_eq!(configured_input_mode(None), LinuxInputControlMode::Auto);
        assert_eq!(
            configured_input_mode(Some("auto")),
            LinuxInputControlMode::Auto
        );
        assert_eq!(
            configured_input_mode(Some("future-value")),
            LinuxInputControlMode::Auto
        );
        assert_eq!(
            configured_input_mode(Some("portal")),
            LinuxInputControlMode::Portal
        );
    }

    #[cfg(target_os = "linux")]
    #[actix_web::test]
    async fn normalized_auto_mode_resolves_from_the_display_server() {
        let mode = configured_input_mode(Some("unknown"));
        assert_eq!(
            resolved_input_control_label(mode, LinuxDisplayServer::Wayland),
            "portal(auto)"
        );
        assert_eq!(
            resolved_input_control_label(mode, LinuxDisplayServer::X11),
            "uinput(auto)"
        );
        assert_eq!(
            resolved_input_control_label(mode, LinuxDisplayServer::Headless),
            "uinput(auto)"
        );
    }

    #[cfg(target_os = "linux")]
    #[actix_web::test]
    async fn missing_portal_probe_never_reports_ready_or_zero_device_types() {
        let (value, status, detail) = remote_desktop_diagnostic(None, true);
        assert_eq!(value, "waiting(portal-auto)");
        assert_eq!(status, BackendDiagnosticStatus::Warning);
        let detail = detail.expect("missing broker snapshot must explain the wait");
        assert!(detail.contains("has not reported"));
        assert!(!detail.contains("AvailableDeviceTypes=0"));
    }

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

    #[actix_web::test]
    async fn test_query_backend_info() {
        let settings = std::sync::Arc::new(SharedSettings::from(Settings::default()));
        let coordinator = crate::model::settings_coordinator::SettingsCoordinator::from_settings(
            settings.clone(),
        )
        .await;
        let app = test::init_service(
            App::new()
                .app_data(web::Data::from(settings))
                .app_data(web::Data::new(coordinator))
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
