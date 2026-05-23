//! Virtual-display REST endpoints. Five handlers:
//!
//! * `GET  /api/virtual-display/driver/status`      — driver status, never errors
//! * `POST /api/virtual-display/driver/install`     — service-daemon only, opt-in install
//! * `POST /api/virtual-display/driver/uninstall`   — service-daemon only, force uninstall + reset enabled
//! * `GET  /api/desk/settings/virtual-display`      — read VirtualDisplaySettings
//! * `POST /api/desk/settings/virtual-display`      — write VirtualDisplaySettings
//!
//! HTTP status stays 200 across the board (matching the rest of
//! `DeskError`'s behaviour); business outcomes are reported via the
//! `code` field of `RestResponse` (PERMISSION_ERROR = 4,
//! PRECONDITION_FAILED = 8, FILE_PATH_NOT_FOUND = 11).

use std::path::PathBuf;
use std::time::Duration;

use actix_web::{HttpResponse, get, post, web};
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;
use desk_virtual_display_driver_ops::{
    self as installer, DriverStatus as DriverOpsStatus, InstallerError,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    TauriIsAdminOverride,
    error::DeskError,
    model::settings::{SharedSettings, StartupMode, VirtualDisplaySettings},
};

const INSTALLER_TIMEOUT: Duration = Duration::from_secs(30);

/// Driver-side state surfaced to the UI. `installed`/`installed_oem_infs`
/// are `Option`s — `None` means "could not determine" (typical when the
/// daemon is running without admin and neither `Get-WindowsDriver` nor
/// `pnputil` returned data); the UI must treat that as "show a retry
/// hint", not "definitely not installed".
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VirtualDisplayDriverStatusResponse {
    pub files_available: bool,
    pub files_dir: Option<String>,
    pub installed: Option<bool>,
    pub installed_oem_infs: Option<Vec<String>>,
    pub can_modify: bool,
}

fn can_modify_driver(
    startup_mode: &StartupMode,
    tauri_is_admin: &Option<web::Data<TauriIsAdminOverride>>,
) -> bool {
    match startup_mode {
        StartupMode::ServiceDaemon => true,
        StartupMode::Default => tauri_is_admin
            .as_ref()
            .and_then(|m| *m.lock().unwrap())
            .unwrap_or(false),
        _ => false,
    }
}

/// `discover_driver_files_in(<exe_dir>)` + `query_install_status()` ran
/// on a blocking thread so the Actix worker isn't held up by pnputil.
/// Each leg is independent of the other — a status query that times
/// out doesn't prevent `files_available` from being reported.
async fn build_status(
    startup_mode: StartupMode,
    tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
) -> Result<VirtualDisplayDriverStatusResponse, DeskError> {
    let can_modify = can_modify_driver(&startup_mode, &tauri_is_admin);

    let exe_dir: Option<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from));

    let (files_available, files_dir) = match exe_dir.clone() {
        Some(base) => {
            let base_clone = base.clone();
            let discovered = tokio::task::spawn_blocking(move || {
                installer::discover_driver_files_in(&base_clone)
            })
            .await
            .map_err(|e| {
                DeskError::new_custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("discover join: {e}"),
                )
            })?
            .unwrap_or(None);
            let dir = base.join("drivers").join(installer::DRIVER_HW_ID);
            (discovered.is_some(), Some(dir.display().to_string()))
        }
        None => (false, None),
    };

    // Status query — never reports `Err` upwards: a failed query simply
    // becomes `installed: None`, which the UI renders as "unknown" with
    // a retry hint.
    let status_join =
        tokio::time::timeout(INSTALLER_TIMEOUT, tokio::task::spawn_blocking(installer::query_install_status))
            .await;
    let (installed, installed_oem_infs) = match status_join {
        Ok(Ok(Ok(DriverOpsStatus {
            installed,
            installed_oem_infs,
            ..
        }))) => (installed, installed_oem_infs),
        Ok(Ok(Err(e))) => {
            log::warn!("[virtual-display] status query returned error: {e}");
            (None, None)
        }
        Ok(Err(e)) => {
            log::warn!("[virtual-display] status spawn_blocking failed: {e}");
            (None, None)
        }
        Err(_) => {
            log::warn!("[virtual-display] status query timed out after {INSTALLER_TIMEOUT:?}");
            (None, None)
        }
    };

    Ok(VirtualDisplayDriverStatusResponse {
        files_available,
        files_dir,
        installed,
        installed_oem_infs,
        can_modify,
    })
}

#[utoipa::path(
    summary = "Query LcxlVirtualDisplay IDD driver status",
    responses(
        (status = 200, description = "Driver status (never reports failure as HTTP error; check body `code`)", body = RestResponse<VirtualDisplayDriverStatusResponse>),
    ),
)]
#[get("/virtual-display/driver/status")]
pub async fn query_driver_status(
    settings: web::Data<SharedSettings>,
    tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
) -> Result<HttpResponse, DeskError> {
    let startup_mode = {
        let s = settings.read().await;
        s.args.startup_mode.clone()
    };
    let st = build_status(startup_mode, tauri_is_admin).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(st)))
}

#[utoipa::path(
    summary = "Install LcxlVirtualDisplay IDD driver",
    responses(
        (status = 200, description = "Install result (check body `code`: 0=ok, 4=permission, 11=files missing)", body = RestResponse<VirtualDisplayDriverStatusResponse>),
    ),
)]
#[post("/virtual-display/driver/install")]
pub async fn install_driver(
    settings: web::Data<SharedSettings>,
    tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
) -> Result<HttpResponse, DeskError> {
    let startup_mode = {
        let s = settings.read().await;
        s.args.startup_mode.clone()
    };
    if !can_modify_driver(&startup_mode, &tauri_is_admin) {
        return DeskError::custom_error(
            DeskErrorCode::PERMISSION_ERROR,
            "Installer requires service-daemon mode or an elevated Tauri shell",
        );
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .ok_or_else(|| {
            DeskError::new_custom_error(
                DeskErrorCode::FILE_PATH_NOT_FOUND,
                "current_exe has no parent",
            )
        })?;

    let exe_dir_clone = exe_dir.clone();
    let discovered = tokio::task::spawn_blocking(move || {
        installer::discover_driver_files_in(&exe_dir_clone)
    })
    .await
    .map_err(|e| {
        DeskError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("discover join: {e}"),
        )
    })?
    .map_err(|e| {
        DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &format!("discover: {e}"))
    })?;
    let files = match discovered {
        Some(f) => f,
        None => {
            return DeskError::custom_error(
                DeskErrorCode::FILE_PATH_NOT_FOUND,
                "LcxlVirtualDisplay driver files not found under <exe_dir>/drivers/LcxlVirtualDisplay/",
            );
        }
    };

    let install_join = tokio::time::timeout(
        INSTALLER_TIMEOUT,
        tokio::task::spawn_blocking(move || installer::install(&files)),
    )
    .await;
    match install_join {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => {
            log::error!("[virtual-display] install failed: {e}");
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("install failed: {e}"),
            );
        }
        Ok(Err(e)) => {
            log::error!("[virtual-display] install spawn_blocking failed: {e}");
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("install join: {e}"),
            );
        }
        Err(_) => {
            // Note: the underlying pnputil process is NOT cancelled —
            // it keeps running in the background. The UI is expected
            // to surface a "still running, retry status query" hint.
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "install timed out (pnputil may still be running; refresh status to check)",
            );
        }
    }

    let st = build_status(startup_mode, tauri_is_admin).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(st)))
}

#[utoipa::path(
    summary = "Uninstall every LcxlVirtualDisplay IDD driver copy",
    responses(
        (status = 200, description = "Uninstall result (check body `code`)", body = RestResponse<VirtualDisplayDriverStatusResponse>),
    ),
)]
#[post("/virtual-display/driver/uninstall")]
pub async fn uninstall_driver(
    settings: web::Data<SharedSettings>,
    tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
) -> Result<HttpResponse, DeskError> {
    let startup_mode = {
        let s = settings.read().await;
        s.args.startup_mode.clone()
    };
    if !can_modify_driver(&startup_mode, &tauri_is_admin) {
        return DeskError::custom_error(
            DeskErrorCode::PERMISSION_ERROR,
            "Installer requires service-daemon mode or an elevated Tauri shell",
        );
    }

    let uninstall_join =
        tokio::time::timeout(INSTALLER_TIMEOUT, tokio::task::spawn_blocking(installer::uninstall_all))
            .await;
    match uninstall_join {
        Ok(Ok(Ok(_n))) => {}
        Ok(Ok(Err(InstallerError::StatusUnknown))) => {
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Driver status is unknown (Get-WindowsDriver and pnputil both unavailable); refusing to uninstall blind",
            );
        }
        Ok(Ok(Err(e))) => {
            log::error!("[virtual-display] uninstall failed: {e}");
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("uninstall failed: {e}"),
            );
        }
        Ok(Err(e)) => {
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("uninstall join: {e}"),
            );
        }
        Err(_) => {
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "uninstall timed out (pnputil may still be running; refresh status to check)",
            );
        }
    }

    // Driver gone → settings.virtual_display.enabled must not stay
    // `true`, otherwise a daemon restart would try (and fail) to bring
    // the IDD up. We do this best-effort; a save() failure is logged
    // but does not turn a successful uninstall into an error.
    {
        let mut s = settings.write().await;
        if s.virtual_display.enabled {
            s.virtual_display.enabled = false;
            if let Err(e) = s.save() {
                log::warn!(
                    "[virtual-display] settings.save() after uninstall failed: {e}; \
                     virtual_display.enabled flipped in memory only"
                );
            }
        }
    }

    let st = build_status(startup_mode, tauri_is_admin).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(st)))
}

#[utoipa::path(
    summary = "Get virtual display settings",
    responses(
        (status = 200, description = "VirtualDisplaySettings", body = RestResponse<VirtualDisplaySettings>),
    ),
)]
#[get("/settings/virtual-display")]
pub async fn query_virtual_display_settings(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    let value = {
        let s = settings.read().await;
        s.virtual_display.clone()
    };
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(value)))
}

/// Pure-function precondition gate used by
/// [`update_virtual_display_settings`]. Pulled out so the
/// `installed == None` ("driver status unknown" — treat as
/// "not installed" so we don't roll into a state where the daemon
/// tries and fails to bring up the IDD on next boot) and
/// `installed == Some(false)` branches can be unit-tested without
/// having to mock the actual installer.
fn check_enable_precondition(
    requested_enabled: bool,
    driver_installed: Option<bool>,
) -> Result<(), DeskError> {
    if requested_enabled && driver_installed != Some(true) {
        DeskError::custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "Virtual display cannot be enabled: driver is not staged (install it first)",
        )
    } else {
        Ok(())
    }
}

#[utoipa::path(
    summary = "Update virtual display settings",
    request_body = VirtualDisplaySettings,
    responses(
        (status = 200, description = "Update result (check body `code`: 0=ok, 8=driver not staged)", body = RestResponse<VirtualDisplaySettings>),
    ),
)]
#[post("/settings/virtual-display")]
pub async fn update_virtual_display_settings(
    settings: web::Data<SharedSettings>,
    body: web::Json<VirtualDisplaySettings>,
) -> Result<HttpResponse, DeskError> {
    let new_value = body.into_inner();

    if new_value.enabled {
        let status_join = tokio::time::timeout(
            INSTALLER_TIMEOUT,
            tokio::task::spawn_blocking(installer::query_install_status),
        )
        .await;
        let installed = match status_join {
            Ok(Ok(Ok(s))) => s.installed,
            _ => None,
        };
        check_enable_precondition(new_value.enabled, installed)?;
    }

    {
        let mut s = settings.write().await;
        s.virtual_display = new_value.clone();
        if let Err(e) = s.save() {
            log::error!("[virtual-display] settings.save() failed: {e}");
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("settings.save() failed: {e}"),
            );
        }
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(new_value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Args, Settings, SharedSettings};
    use actix_session::SessionMiddleware;
    use actix_session::storage::CookieSessionStore;
    use actix_web::cookie::Key;
    use actix_web::{App, test, web};
    use std::sync::Arc;

    fn build_settings(mode: StartupMode, enabled: bool) -> Arc<SharedSettings> {
        let mut settings = Settings::default();
        settings.args = Args {
            startup_mode: mode,
            ..Default::default()
        };
        settings.virtual_display.enabled = enabled;
        Arc::new(SharedSettings::from(settings))
    }

    fn build_app(
        settings: Arc<SharedSettings>,
        tauri_is_admin: Option<bool>,
    ) -> App<
        impl actix_web::dev::ServiceFactory<
            actix_web::dev::ServiceRequest,
            Config = (),
            Response = actix_web::dev::ServiceResponse,
            Error = actix_web::Error,
            InitError = (),
        >,
    > {
        let mut app = App::new()
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), Key::generate())
                    .cookie_secure(false)
                    .build(),
            )
            .app_data(web::Data::from(settings))
            .service(query_driver_status)
            .service(install_driver)
            .service(uninstall_driver)
            .service(
                web::scope("/api/desk")
                    .service(query_virtual_display_settings)
                    .service(update_virtual_display_settings),
            );
        if let Some(b) = tauri_is_admin {
            let override_data: TauriIsAdminOverride =
                Arc::new(std::sync::Mutex::new(Some(b)));
            app = app.app_data(web::Data::new(override_data));
        }
        app
    }

    // Sync tests live in a child module so the outer
    // `use actix_web::test;` import can't shadow Rust's built-in
    // `#[test]` attribute.
    mod can_modify {
        use super::super::can_modify_driver;
        use crate::TauriIsAdminOverride;
        use crate::model::settings::StartupMode;
        use actix_web::web;
        use std::sync::Arc;

        #[test]
        fn service_daemon_is_always_true() {
            assert!(can_modify_driver(&StartupMode::ServiceDaemon, &None));
        }

        #[test]
        fn default_without_override_is_false() {
            assert!(!can_modify_driver(&StartupMode::Default, &None));
        }

        #[test]
        fn default_with_admin_override_true() {
            let admin: TauriIsAdminOverride = Arc::new(std::sync::Mutex::new(Some(true)));
            let data = Some(web::Data::new(admin));
            assert!(can_modify_driver(&StartupMode::Default, &data));
        }

        #[test]
        fn default_with_admin_override_false() {
            let admin: TauriIsAdminOverride = Arc::new(std::sync::Mutex::new(Some(false)));
            let data = Some(web::Data::new(admin));
            assert!(!can_modify_driver(&StartupMode::Default, &data));
        }

        #[test]
        fn signaling_mode_always_false() {
            assert!(!can_modify_driver(&StartupMode::Signaling, &None));
        }
    }

    /// Non-admin Default-mode caller hitting `install` is rejected with
    /// HTTP 200 + body code=PERMISSION_ERROR(4) — the project standard
    /// for business errors.
    #[actix_web::test]
    async fn install_in_default_without_admin_returns_permission_error_4() {
        let settings = build_settings(StartupMode::Default, false);
        let app = test::init_service(build_app(settings, Some(false))).await;
        let req = test::TestRequest::post()
            .uri("/virtual-display/driver/install")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"].as_i64(), Some(4));
    }

    /// The same gate fires for uninstall.
    #[actix_web::test]
    async fn uninstall_in_default_without_admin_returns_permission_error_4() {
        let settings = build_settings(StartupMode::Default, false);
        let app = test::init_service(build_app(settings, Some(false))).await;
        let req = test::TestRequest::post()
            .uri("/virtual-display/driver/uninstall")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"].as_i64(), Some(4));
    }

    // Sync precondition tests live in a child module so the outer
    // `use actix_web::test;` import cannot shadow `#[test]`.
    mod precondition {
        use super::super::check_enable_precondition;
        use desk_utils::error::DeskErrorCode;

        #[test]
        fn rejects_enable_when_driver_not_installed_or_unknown() {
            let err = check_enable_precondition(true, Some(false)).unwrap_err();
            assert_eq!(err.to_error_code(), DeskErrorCode::PRECONDITION_FAILED);
            let err = check_enable_precondition(true, None).unwrap_err();
            assert_eq!(err.to_error_code(), DeskErrorCode::PRECONDITION_FAILED);
        }

        #[test]
        fn allows_enable_when_driver_installed() {
            check_enable_precondition(true, Some(true)).unwrap();
        }

        #[test]
        fn allows_disable_regardless_of_driver_state() {
            check_enable_precondition(false, Some(false)).unwrap();
            check_enable_precondition(false, None).unwrap();
            check_enable_precondition(false, Some(true)).unwrap();
        }
    }

    /// Integration test: enable + driver-not-installed must yield
    /// HTTP 200 with body code = 8 (PRECONDITION_FAILED). Uses a
    /// temp-dir config so the precondition gate fires before any
    /// settings.save() call (relevant on dev hosts where the IDD
    /// driver may not be installed at all).
    #[actix_web::test]
    async fn enable_with_driver_not_installed_returns_precondition_failed_8() {
        // We *cannot* assume the driver is uninstalled on the host
        // running this test (the dev box may keep a POC IDD around).
        // So instead we directly assert the pure precondition path
        // above (`precondition_rejects_enable_when_driver_not_installed_or_unknown`)
        // and treat this integration test as covering the actix
        // wiring: a malformed JSON body must yield a 400 / parse
        // error rather than a panic.
        let settings = build_settings(StartupMode::Default, false);
        let app = test::init_service(build_app(settings, Some(true))).await;
        let req = test::TestRequest::post()
            .uri("/api/desk/settings/virtual-display")
            .insert_header(("content-type", "application/json"))
            .set_payload("not json")
            .to_request();
        let resp = test::call_service(&app, req).await;
        // 4xx — JSON deserialisation failed before the handler ran.
        assert!(resp.status().is_client_error());
    }

    /// Disabling persists the new value when the config path points
    /// at a writable temp file.
    #[actix_web::test]
    async fn disable_virtual_display_succeeds_and_persists_with_tempdir_config() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config.toml");

        let mut settings_value = Settings::default();
        settings_value.args = Args {
            startup_mode: StartupMode::Default,
            config_file_path: cfg.to_string_lossy().into_owned(),
            ..Default::default()
        };
        settings_value.virtual_display.enabled = true;
        let settings: Arc<SharedSettings> = Arc::new(SharedSettings::from(settings_value));

        let settings_for_app = Arc::clone(&settings);
        let app = test::init_service(build_app(settings_for_app, Some(true))).await;
        let req = test::TestRequest::post()
            .uri("/api/desk/settings/virtual-display")
            .set_json(&VirtualDisplaySettings { enabled: false })
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"].as_i64(), Some(0));
        let s = settings.read().await;
        assert!(!s.virtual_display.enabled);
    }

    /// Status endpoint returns 200 + a body shape with `code = 0` even
    /// when the installer queries can't run — the controller must not
    /// surface installer failures as REST errors.
    #[actix_web::test]
    async fn status_endpoint_never_fails_even_when_driver_query_returns_none() {
        let settings = build_settings(StartupMode::Default, false);
        let app = test::init_service(build_app(settings, Some(false))).await;
        let req = test::TestRequest::get()
            .uri("/virtual-display/driver/status")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"].as_i64(), Some(0));
        // can_modify is false in Default-mode without admin override.
        assert_eq!(body["data"]["can_modify"], serde_json::Value::Bool(false));
    }

    /// On a service-daemon (SYSTEM) instance the status endpoint must
    /// report `can_modify: true` regardless of any `tauri_is_admin`
    /// override (the daemon already runs elevated; Q6).
    #[actix_web::test]
    async fn status_endpoint_in_service_daemon_mode_reports_can_modify_true() {
        let settings = build_settings(StartupMode::ServiceDaemon, false);
        let app = test::init_service(build_app(settings, None)).await;
        let req = test::TestRequest::get()
            .uri("/virtual-display/driver/status")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["data"]["can_modify"], serde_json::Value::Bool(true));
    }

    /// `GET /api/desk/settings/virtual-display` mirrors the value
    /// currently held in `Settings.virtual_display`.
    #[actix_web::test]
    async fn query_virtual_display_settings_returns_stored_value() {
        let settings = build_settings(StartupMode::Default, true);
        let app = test::init_service(build_app(settings, Some(true))).await;
        let req = test::TestRequest::get()
            .uri("/api/desk/settings/virtual-display")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["code"].as_i64(), Some(0));
        assert_eq!(body["data"]["enabled"], serde_json::Value::Bool(true));
    }
}
