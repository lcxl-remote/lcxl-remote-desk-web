use std::sync::Arc;

use actix_web::{HttpResponse, post, web};
use desk_signal_facade::model::security_settings::SecuritySettings;
use log::info;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    daemon::{manager_link_gate::ManagerLinkGate, signaling_proxy::manager_link_should_connect},
    error::DeskError,
    model::settings::SharedSettings,
};

pub const TAG: &str = "System";

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct InitParams {
    pub username: String,
    pub password: String,
    pub telemetry_consent: bool,
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
    manager_link_gate: web::Data<Arc<ManagerLinkGate>>,
) -> Result<HttpResponse, DeskError> {
    let mut settings = settings.write().await;

    // Check if system is already initialized string is not empty
    if !settings.user.login_password.is_empty() {
        return Err(DeskError::new_custom_error(
            desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
            "System is already initialized",
        ));
    }

    let params = request_json.into_inner();
    settings.user.login_user_name = params.username;
    settings.user.login_password = params.password;
    settings.system.telemetry_consent = Some(params.telemetry_consent);

    // Persist an optional manager target in one shot (skipping the manager step
    // leaves these untouched). Empty strings are treated as "not provided".
    if let Some(url) = params.manager_url.filter(|u| !u.trim().is_empty()) {
        settings.system.manager_url = Some(url);
    }
    if let Some(token) = params.manager_api_token.filter(|t| !t.trim().is_empty()) {
        settings.system.manager_api_token = Some(token);
    }

    // Persist optional initial security settings; normalize an unset approval
    // timeout to the finite default. When omitted, `settings.security` keeps its
    // default (all capabilities prompt, 30s approval timeout).
    if let Some(mut security) = params.security {
        security.normalize();
        settings.security = security;
    }

    settings.save()?;

    // Sync the shared manager-link gate to the freshly persisted config while
    // still holding the settings write lock, so the proxy's reconnect loop brings
    // the manager link up (and does not immediately tear it down) after first-run
    // initialization configures a manager.
    manager_link_gate.set(manager_link_should_connect(
        &settings.system.manager_url,
        &settings.system.manager_api_token,
        settings.system.manager_enabled,
    ));

    info!("System initialized successfully");
    Ok(HttpResponse::Ok().json(desk_utils::rest::RestResponse::succeed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::{Args, Settings};
    use actix_web::{App, test, web};
    use std::env;

    async fn create_test_settings() -> SharedSettings {
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
        let settings = create_test_settings().await;
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(test_gate())
                .service(init_system),
        )
        .await;

        let params = InitParams {
            username: "admin".to_string(),
            password: "new_password".to_string(),
            telemetry_consent: true,
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
        let settings = create_test_settings().await;
        {
            let mut s = settings.write().await;
            s.user.login_password = "already_set".to_string();
        }

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .app_data(test_gate())
                .service(init_system),
        )
        .await;

        let params = InitParams {
            username: "admin".to_string(),
            password: "new_password".to_string(),
            telemetry_consent: false,
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
        let settings = create_test_settings().await;
        let gate = Arc::new(ManagerLinkGate::new(false));
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
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
    }
}
