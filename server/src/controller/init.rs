use std::sync::Arc;

use actix_web::{HttpRequest, HttpResponse, get, post, web};
use desk_signal_facade::model::security_settings::SecuritySettings;
use log::info;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    daemon::{manager_link_gate::ManagerLinkGate, signaling_proxy::manager_link_should_connect},
    error::DeskError,
    host_control::HostControlHub,
    model::settings::SharedSettings,
    service::{
        bootstrap::BootstrapToken,
        client_ip::ClientIpExtractor,
        rate_limit::{AuthRateLimiter, BootstrapAttempt},
    },
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
    /// Deployment bootstrap token. Required only when the server process was
    /// started with `LRD_BOOTSTRAP_TOKEN`.
    #[serde(default)]
    pub bootstrap_token: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct InitRequirementsDto {
    pub bootstrap_token_required: bool,
}

#[utoipa::path(
    tag = TAG,
    summary = "Query standalone initialization requirements",
    responses(
        (status = 200, description = "Initialization requirements", body = desk_utils::rest::RestResponse<InitRequirementsDto>),
    ),
)]
#[get("/api/init/requirements")]
pub async fn init_requirements(bootstrap: web::Data<BootstrapToken>) -> HttpResponse {
    HttpResponse::Ok().json(desk_utils::rest::RestResponse::succeed_with_data(
        InitRequirementsDto {
            bootstrap_token_required: bootstrap.is_required(),
        },
    ))
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
#[allow(clippy::too_many_arguments)] // Actix handler extractors are intentionally explicit app data.
#[post("/api/init")]
pub async fn init_system(
    req: HttpRequest,
    request_json: web::Json<InitParams>,
    settings: web::Data<SharedSettings>,
    coordinator: web::Data<crate::model::settings_coordinator::SettingsCoordinator>,
    manager_link_gate: web::Data<Arc<ManagerLinkGate>>,
    host_control_hub: Option<web::Data<Option<Arc<HostControlHub>>>>,
    bootstrap: web::Data<BootstrapToken>,
    client_ip: web::Data<ClientIpExtractor>,
    rate_limiter: web::Data<Arc<AuthRateLimiter>>,
) -> Result<HttpResponse, DeskError> {
    let params = request_json.into_inner();
    if !settings.read().await.user.login_password.is_empty() {
        return Err(DeskError::new_custom_error(
            desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
            "System is already initialized",
        ));
    }
    match bootstrap.evaluate(
        rate_limiter.get_ref().as_ref(),
        client_ip.network_key(&req),
        params.bootstrap_token.as_deref(),
    ) {
        BootstrapAttempt::Allowed => {}
        BootstrapAttempt::Invalid => {
            return Ok(
                HttpResponse::Ok().json(desk_utils::rest::RestResponse::<()>::failed(
                    desk_utils::error::DeskErrorCode::PERMISSION_ERROR,
                    "Invalid bootstrap token".to_string(),
                )),
            );
        }
        BootstrapAttempt::Limited => {
            return Ok(
                HttpResponse::Ok().json(desk_utils::rest::RestResponse::<()>::failed(
                    desk_utils::error::DeskErrorCode::TOO_MANY_ATTEMPTS,
                    "Too many attempts. Please try again later.".to_string(),
                )),
            );
        }
    }
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
    use crate::model::settings::Settings;
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
        let mut temp_path = env::temp_dir();
        temp_path.push(format!("desk_init_test_{}.toml", uuid::Uuid::new_v4()));
        let mut settings = Settings::for_test_config(&temp_path);
        // Ensure it's not "initialized"
        settings.user.login_password = "".to_string();

        SharedSettings::from(settings)
    }

    fn test_gate() -> web::Data<Arc<ManagerLinkGate>> {
        web::Data::new(Arc::new(ManagerLinkGate::new(false)))
    }

    fn auth_data() -> (
        web::Data<BootstrapToken>,
        web::Data<ClientIpExtractor>,
        web::Data<Arc<AuthRateLimiter>>,
    ) {
        (
            web::Data::new(BootstrapToken::disabled()),
            web::Data::new(ClientIpExtractor::default()),
            web::Data::new(Arc::new(AuthRateLimiter::new(64))),
        )
    }

    #[actix_web::test]
    async fn test_init_system_success() {
        let (settings, coordinator) = create_test_settings().await;
        let (bootstrap, client_ip, limiter) = auth_data();
        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator.clone())
                .app_data(test_gate())
                .app_data(bootstrap)
                .app_data(client_ip)
                .app_data(limiter)
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
            bootstrap_token: None,
        };

        let req = test::TestRequest::post()
            .peer_addr("192.0.2.1:1234".parse().unwrap())
            .uri("/api/init")
            .set_json(&params)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: desk_utils::rest::RestResponse<()> = test::read_body_json(resp).await;
        assert!(body.success);
    }

    #[actix_web::test]
    async fn requirements_reports_token_gate_without_leaking_the_token() {
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(BootstrapToken::required("secret-token")))
                .service(init_requirements),
        )
        .await;
        let response = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/init/requirements")
                .to_request(),
        )
        .await;
        let body: serde_json::Value = test::read_body_json(response).await;
        assert_eq!(body["data"]["bootstrap_token_required"], true);
        assert!(!body.to_string().contains("secret-token"));
    }

    #[actix_web::test]
    async fn bootstrap_gate_rejects_wrong_token_before_initialization_side_effects() {
        let (settings, coordinator) = create_test_settings().await;
        let limiter = web::Data::new(Arc::new(AuthRateLimiter::new(64)));
        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator)
                .app_data(test_gate())
                .app_data(web::Data::new(BootstrapToken::required("correct-token")))
                .app_data(web::Data::new(ClientIpExtractor::default()))
                .app_data(limiter)
                .service(init_system),
        )
        .await;
        let mut params = InitParams {
            username: "admin".to_string(),
            password: "new_password".to_string(),
            telemetry_consent: false,
            host_access_indicator_enabled: true,
            manager_url: None,
            manager_api_token: None,
            security: None,
            bootstrap_token: Some("wrong-token".to_string()),
        };
        let wrong = test::TestRequest::post()
            .peer_addr("192.0.2.10:1234".parse().unwrap())
            .uri("/api/init")
            .set_json(&params)
            .to_request();
        let body: desk_utils::rest::RestResponse<()> =
            test::call_and_read_body_json(&app, wrong).await;
        assert_eq!(
            body.code,
            desk_utils::error::DeskErrorCode::PERMISSION_ERROR.code()
        );
        assert!(settings.read().await.user.login_password.is_empty());

        params.bootstrap_token = Some("correct-token".to_string());
        let correct = test::TestRequest::post()
            .peer_addr("192.0.2.10:1234".parse().unwrap())
            .uri("/api/init")
            .set_json(&params)
            .to_request();
        let body: desk_utils::rest::RestResponse<()> =
            test::call_and_read_body_json(&app, correct).await;
        assert!(body.success);
        assert_eq!(settings.read().await.user.login_password, "new_password");
    }

    #[actix_web::test]
    async fn concurrent_initialization_commits_exactly_once() {
        let (settings, coordinator) = create_test_settings().await;
        let (bootstrap, client_ip, limiter) = auth_data();
        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator)
                .app_data(test_gate())
                .app_data(bootstrap)
                .app_data(client_ip)
                .app_data(limiter)
                .service(init_system),
        )
        .await;
        let params = |password: &str| InitParams {
            username: "admin".to_string(),
            password: password.to_string(),
            telemetry_consent: false,
            host_access_indicator_enabled: true,
            manager_url: None,
            manager_api_token: None,
            security: None,
            bootstrap_token: None,
        };
        let first = test::TestRequest::post()
            .peer_addr("192.0.2.20:1234".parse().unwrap())
            .uri("/api/init")
            .set_json(params("first-password"))
            .to_request();
        let second = test::TestRequest::post()
            .peer_addr("192.0.2.21:1234".parse().unwrap())
            .uri("/api/init")
            .set_json(params("second-password"))
            .to_request();

        let (first_response, second_response) = tokio::join!(
            test::call_service(&app, first),
            test::call_service(&app, second)
        );
        let first_body: desk_utils::rest::RestResponse<()> =
            test::read_body_json(first_response).await;
        let second_body: desk_utils::rest::RestResponse<()> =
            test::read_body_json(second_response).await;
        assert_eq!(
            [first_body.success, second_body.success]
                .into_iter()
                .filter(|success| *success)
                .count(),
            1
        );
        assert!(matches!(
            settings.read().await.user.login_password.as_str(),
            "first-password" | "second-password"
        ));
    }

    #[actix_web::test]
    async fn test_init_system_already_initialized() {
        let (settings, coordinator) = create_test_settings().await;
        let (_, client_ip, limiter) = auth_data();
        {
            let mut s = settings.write().await;
            s.user.login_password = "already_set".to_string();
        }

        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator.clone())
                .app_data(test_gate())
                .app_data(web::Data::new(BootstrapToken::required("correct-token")))
                .app_data(client_ip)
                .app_data(limiter)
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
            bootstrap_token: Some("wrong-token".to_string()),
        };

        let req = test::TestRequest::post()
            .peer_addr("192.0.2.2:1234".parse().unwrap())
            .uri("/api/init")
            .set_json(&params)
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);

        let body: desk_utils::rest::RestResponse<()> = test::read_body_json(resp).await;
        assert!(!body.success);
        assert_eq!(
            body.code,
            desk_utils::error::DeskErrorCode::SYSTEM_ERROR.code()
        );
    }

    #[actix_web::test]
    async fn test_init_system_persists_manager_and_security_and_gate() {
        let (settings, coordinator) = create_test_settings().await;
        let (bootstrap, client_ip, limiter) = auth_data();
        let gate = Arc::new(ManagerLinkGate::new(false));
        let app = test::init_service(
            App::new()
                .app_data(settings.clone())
                .app_data(coordinator.clone())
                .app_data(web::Data::new(gate.clone()))
                .app_data(bootstrap)
                .app_data(client_ip)
                .app_data(limiter)
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
            bootstrap_token: None,
        };
        let req = test::TestRequest::post()
            .peer_addr("192.0.2.3:1234".parse().unwrap())
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
