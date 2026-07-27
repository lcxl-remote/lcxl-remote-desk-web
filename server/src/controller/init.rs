use std::sync::Arc;

use actix_web::{HttpResponse, post, web};
use desk_signal_facade::model::security_settings::SecuritySettings;
use log::info;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    daemon::{manager_link_gate::ManagerLinkGate, signaling_proxy::manager_link_should_connect},
    error::DeskError,
    host_control::HostControlHub,
    model::settings::SharedSettings,
};

pub const TAG: &str = "System";

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct InitParams {
    pub username: String,
    pub password: String,
    pub telemetry_consent: bool,
    pub host_access_indicator_enabled: bool,
    /// Optional manager signaling URL to connect on first run (the wizard's
    /// resolved `ws(s)://.../signaling` URL). Skipping the manager step leaves it
    /// `None`.
    #[serde(default)]
    pub manager_url: Option<String>,
    /// Optional manager API token paired with `manager_url`.
    #[serde(default)]
    pub manager_api_token: Option<String>,
    /// Optional initial security settings (per-capability toggles). When `None`
    /// the defaults apply (all capabilities prompt; approval timeout 30s).
    #[serde(default)]
    pub security: Option<SecuritySettings>,
}

#[utoipa::path(
    tag = TAG,
    summary = "Initialize system",
    request_body(content = InitParams),
    responses(
        (status = 200, description = "System initialized successfully"),
        (status = 403, description = "System already initialized"),
    ),
)]
#[post("/api/init")]
pub async fn init_system(
    request_json: web::Json<InitParams>,
    settings: web::Data<SharedSettings>,
    coordinator: web::Data<crate::model::settings_coordinator::SettingsCoordinator>,
    manager_link_gate: web::Data<Arc<ManagerLinkGate>>,
    host_control_hub: Option<web::Data<Option<Arc<HostControlHub>>>>,
) -> Result<HttpResponse, DeskError> {
    let params = request_json.into_inner();
    coordinator
        .commit_with_effect(
            move |settings| {
                // Checked inside the commit so two first-run requests arriving
                // together cannot both pass the check and each write an account.
                if !settings.user.login_password.is_empty() {
                    return Err(DeskError::new_custom_error(
                        desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
                        "System is already initialized",
                    ));
                }
                settings.user.login_user_name = params.username;
                settings.user.login_password = params.password;
                settings.system.telemetry_consent = Some(params.telemetry_consent);
                settings.system.host_access_indicator_enabled =
                    params.host_access_indicator_enabled;

                // Persist an optional manager target in one shot (skipping the
                // manager step leaves these untouched). Empty strings are
                // treated as "not provided".
                if let Some(url) = params.manager_url.filter(|u| !u.trim().is_empty()) {
                    settings.system.manager_url = Some(url);
                }
                if let Some(token) = params.manager_api_token.filter(|t| !t.trim().is_empty()) {
                    settings.system.manager_api_token = Some(token);
                }

                // Optional initial security settings. When omitted,
                // `settings.security` keeps its default (all capabilities
                // prompt, 30s approval timeout).
                if let Some(security) = params.security {
                    settings.security = security;
                }
                Ok(())
            },
            // Sync the shared manager-link gate while the settings are still
            // locked, so the proxy's reconnect loop brings the manager link up
            // (and does not immediately tear it down) after first-run
            // initialization configures a manager.
            |settings| {
                manager_link_gate.set(manager_link_should_connect(
                    &settings.system.manager_url,
                    &settings.system.manager_api_token,
                    settings.system.manager_enabled,
                ));
            },
        )
        .await?;

    if let Some(hub) = host_control_hub
        .as_ref()
        .and_then(|data| data.get_ref().as_ref())
    {
        hub.host_activity()
            .set_indicator_enabled(settings.read().await.system.host_access_indicator_enabled);
    }

    info!("System initialized successfully");
    Ok(HttpResponse::Ok().json(desk_utils::rest::RestResponse::succeed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Args, Settings};
    use actix_web::{App, test, web};
    use std::env;

    /// The settings plus the coordinator the handler commits through, sharing
    /// one `SharedSettings` exactly as the running host does.
    async fn create_test_settings() -> (
        web::Data<SharedSettings>,
        web::Data<crate::model::settings_coordinator::SettingsCoordinator>,
    ) {
        let shared = Arc::new(build_test_settings().await);
        let coordinator = Arc::new(
            crate::model::settings_coordinator::SettingsCoordinator::from_settings(Arc::clone(
                &shared,
            ))
            .await,
        );
        (web::Data::from(shared), web::Data::from(coordinator))
    }

    async fn build_test_settings() -> SharedSettings {
        let mut settings = Settings::default();
        // Ensure it's not "initialized"
        settings.user.login_password = "".to_string();

        let mut temp_path = env::temp_dir();
        temp_path.push(format!("desk_init_test_{}.toml", uuid::Uuid::new_v4()));
        settings.args = Args {
            config_file_path: temp_path.to_string_lossy().to_string(),
            ..Default::default()
        };

        SharedSettings::from(settings)
    }

    fn test_gate() -> web::Data<Arc<ManagerLinkGate>> {
        web::Data::new(Arc::new(ManagerLinkGate::new(false)))
    }

    #[actix_web::test]
    async fn test_init_system_success() {
        let (settings, coordinator) = create_test_settings().await;
        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator.clone())
                .app_data(test_gate())
                .service(init_system),
        )
        .await;

        let params = InitParams {
            username: "admin".to_string(),
            password: "new_password".to_string(),
            telemetry_consent: true,
            host_access_indicator_enabled: true,
            manager_url: None,
            manager_api_token: None,
            security: None,
        };

        let req = test::TestRequest::post()
            .uri("/api/init")
            .set_json(&params)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: desk_utils::rest::RestResponse<()> = test::read_body_json(resp).await;
        assert!(body.success);
    }

    #[actix_web::test]
    async fn test_init_system_already_initialized() {
        let (settings, coordinator) = create_test_settings().await;
        {
            let mut s = settings.write().await;
            s.user.login_password = "already_set".to_string();
        }

        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator.clone())
                .app_data(test_gate())
                .service(init_system),
        )
        .await;

        let params = InitParams {
            username: "admin".to_string(),
            password: "new_password".to_string(),
            telemetry_consent: false,
            host_access_indicator_enabled: true,
            manager_url: None,
            manager_api_token: None,
            security: None,
        };

        let req = test::TestRequest::post()
            .uri("/api/init")
            .set_json(&params)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

        let body: desk_utils::rest::RestResponse<()> = test::read_body_json(resp).await;
        assert!(!body.success);
    }

    #[actix_web::test]
    async fn test_init_system_persists_manager_and_security_and_gate() {
        let (settings, coordinator) = create_test_settings().await;
        let gate = Arc::new(ManagerLinkGate::new(false));
        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator.clone())
                .app_data(web::Data::new(gate.clone()))
                .service(init_system),
        )
        .await;

        // Security payload with an unset approval timeout must normalize to 30s.
        let security = SecuritySettings {
            allow_remote_control: Some(true),
            approval_timeout: None,
            ..SecuritySettings::default()
        };
        let params = InitParams {
            username: "admin".to_string(),
            password: "pw".to_string(),
            telemetry_consent: true,
            host_access_indicator_enabled: false,
            manager_url: Some("wss://manager.example/api/desk/signaling".to_string()),
            manager_api_token: Some("tok".to_string()),
            security: Some(security),
        };
        let req = test::TestRequest::post()
            .uri("/api/init")
            .set_json(&params)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        // Configuring a manager on init drives the gate to "should connect".
        assert!(gate.should_connect());
        assert!(!settings.read().await.system.host_access_indicator_enabled);
    }
}
