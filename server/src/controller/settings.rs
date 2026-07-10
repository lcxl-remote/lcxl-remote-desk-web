use actix_web::{Error as AWError, HttpResponse, get, post, web};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use log::info;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use std::sync::Arc;

use crate::host_control::{ApprovalResponse, HostControlHub};
use crate::model::settings::{
    AiExecutionPolicyPublic, AiExecutionPolicyUpdate, CollectionPolicySettings,
    CollectionPolicySettingsUpdate, LogSettings, SharedSettings, SystemSettings,
    TurnClientSettings,
};
use crate::daemon::manager_link_gate::ManagerLinkGate;
use crate::daemon::signaling_proxy::manager_link_should_connect;
use crate::service::auto_start::update_auto_start_status;
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_turn::model::TurnSettings;

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct TelemetryStatus {
    pub needed: bool,
    pub consented: Option<bool>,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct TelemetryConsent {
    pub consent: bool,
}

#[utoipa::path(
    tag = "Settings",
    summary = "Query settings",
    responses(
        (status = 200, description = "Query settings successfully", body=RestResponse<SystemSettings>),
    ),
)]
#[get("/settings")]
pub async fn query_settings(settings: web::Data<SharedSettings>) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut system_settings = settings.system.clone();
    // On macOS the LaunchAgent plist is the single source of truth for
    // auto_start: derive it here so the console reflects the real OS state even
    // if a prior save raced or failed, instead of trusting the persisted flag.
    #[cfg(target_os = "macos")]
    {
        system_settings.auto_start = Some(crate::macos_agent::is_configured());
    }
    info!(
        "Query settings successfully, settings: {:?}",
        system_settings
    );
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(system_settings)))
}

#[utoipa::path(
    tag = "Settings",
    summary = "Update settings",
    request_body(content = SystemSettings),
    responses(
        (status = 200, description = "Update settings successfully"),
    ),
)]
#[post("/settings")]
pub async fn update_settings(
    requst_json: web::Json<SystemSettings>,
    settings: web::Data<SharedSettings>,
    manager_link_gate: web::Data<Arc<ManagerLinkGate>>,
) -> Result<HttpResponse, AWError> {
    let mut params = requst_json.into_inner();
    let mut settings = settings.write().await;

    // Apply the auto-start change to the OS first. On macOS the LaunchAgent is
    // the single source of truth, so a failure must surface as a business error
    // and must NOT fall through to persisting an inconsistent flag. (Windows /
    // Linux keep the prior behavior via the same call; config_file_path is only
    // consumed on macOS to write an absolute --config-file-path into the plist.)
    if let Some(auto_start_enable) = params.auto_start {
        let config_file_path = settings.args.config_file_path.clone();
        if let Err(e) =
            update_auto_start_status(auto_start_enable, std::path::Path::new(&config_file_path))
        {
            log::error!("Failed to update auto start status: {:?}", e);
            return Ok(HttpResponse::Ok().json(RestResponse::<()>::failed(
                DeskErrorCode::AUTO_START_ERROR,
                e.to_string(),
            )));
        }
    }

    // The console form omits the auto-generated internal fields; carry them over
    // from the current settings so a full replace doesn't wipe client_id and the
    // signaling/IPC/session secrets.
    params.preserve_internal_fields(&settings.system);
    settings.system = params;
    // save new settings to file
    settings.save()?;
    // Re-sync the shared manager-link gate to the freshly persisted config while
    // still holding the settings write lock, so the proxy's reconnect loop cannot
    // observe the new settings before the gate value catches up. Disabling the
    // manager connection here tears down the current upstream; re-enabling lets
    // the reconnect loop bring it back.
    manager_link_gate.set(manager_link_should_connect(
        &settings.system.manager_url,
        &settings.system.manager_api_token,
        settings.system.manager_enabled,
    ));
    info!("Update system settings successfully, {:?}", settings.system);
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    tag = "AiModel",
    summary = "Query the edge AI execution policy",
    responses(
        (status = 200, description = "Query AI execution policy successfully", body=RestResponse<AiExecutionPolicyPublic>),
    ),
)]
#[get("/settings/ai-policy")]
pub async fn query_ai_policy_settings(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    let public = settings.ai_policy.public_view();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(public)))
}

#[utoipa::path(
    tag = "AiModel",
    summary = "Update the edge AI execution policy",
    request_body(content = AiExecutionPolicyUpdate),
    responses(
        (status = 200, description = "Update AI execution policy successfully"),
    ),
)]
#[post("/settings/ai-policy")]
pub async fn update_ai_policy_settings(
    request_json: web::Json<AiExecutionPolicyUpdate>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let mut settings = settings.write().await;
    settings.ai_policy.apply_update(params);
    settings.save()?;
    info!("Update AI execution policy successfully");
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    tag = "AiModel",
    summary = "Query the edge collection policy",
    responses(
        (status = 200, description = "Query collection policy successfully", body=RestResponse<CollectionPolicySettings>),
    ),
)]
#[get("/settings/collection-policy")]
pub async fn query_collection_policy_settings(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    let public = settings.collection_policy.public_view();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(public)))
}

#[utoipa::path(
    tag = "AiModel",
    summary = "Update the edge collection policy",
    request_body(content = CollectionPolicySettingsUpdate),
    responses(
        (status = 200, description = "Update collection policy successfully"),
    ),
)]
#[post("/settings/collection-policy")]
pub async fn update_collection_policy_settings(
    request_json: web::Json<CollectionPolicySettingsUpdate>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let mut settings = settings.write().await;
    settings.collection_policy.apply_update(params);
    settings.save()?;
    info!(
        "Update collection policy successfully: {:?}",
        settings.collection_policy
    );
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    tag = "Turn",
    summary = "Query turn settings",
    responses(
        (status = 200, description = "Query turn settings successfully", body=RestResponse<TurnSettings>),
    ),
)]
#[get("/settings/turn")]
pub async fn query_turn_settings(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    let turn_settings = settings.turn.clone();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(turn_settings)))
}

#[utoipa::path(
    tag = "Turn",
    summary = "Update turn settings",
    request_body(content = TurnSettings),
    responses(
        (status = 200, description = "Update turn settings successfully"),
    ),
)]
#[post("/settings/turn")]
pub async fn update_turn_settings(
    request_json: web::Json<TurnSettings>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let mut settings = settings.write().await;

    settings.turn.realm = params.realm;
    settings.turn.interfaces = params.interfaces;
    settings.turn.enable_stun = params.enable_stun;
    settings.turn.enable_turn = params.enable_turn;
    settings.turn.relay_min_port = params.relay_min_port;
    settings.turn.relay_max_port = params.relay_max_port;

    settings.save()?;
    info!("Update turn settings successfully");
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    tag = "Log",
    summary = "Query log settings",
    responses(
        (status = 200, description = "Query log settings successfully", body=RestResponse<LogSettings>),
    ),
)]
#[get("/settings/log")]
pub async fn query_log_settings(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    let log_settings = settings.log.clone();
    info!(
        "Query log settings successfully, settings: {:?}",
        log_settings
    );
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(log_settings)))
}

#[utoipa::path(
    tag = "Log",
    summary = "Update log settings",
    request_body(content = LogSettings),
    responses(
        (status = 200, description = "Update log settings successfully"),
    ),
)]
#[post("/settings/log")]
pub async fn update_log_settings(
    requst_json: web::Json<LogSettings>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();
    let mut settings = settings.write().await;

    settings.log = params;
    // save new settings to file
    settings.save()?;
    info!("Update log settings successfully, {:?}", settings.log);
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    tag = "Telemetry",
    summary = "Query telemetry status",
    responses(
        (status = 200, description = "Query telemetry status successfully", body=RestResponse<TelemetryStatus>),
    ),
)]
#[get("/telemetry/status")]
pub async fn query_telemetry_status(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    let consented = settings.system.telemetry_consent;
    // needed if consent is None
    let needed = consented.is_none();

    let status = TelemetryStatus { needed, consented };
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(status)))
}

#[utoipa::path(
    tag = "Telemetry",
    summary = "Update telemetry consent",
    request_body(content = TelemetryConsent),
    responses(
        (status = 200, description = "Update telemetry consent successfully"),
    ),
)]
#[post("/telemetry/consent")]
pub async fn update_telemetry_consent(
    requst_json: web::Json<TelemetryConsent>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();
    let mut settings = settings.write().await;
    settings.system.telemetry_consent = Some(params.consent);
    // save new settings to file
    settings.save()?;
    info!(
        "Update telemetry consent successfully, consent: {}",
        params.consent
    );
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    tag = "Turn",
    summary = "Regenerate TURN static_auth_secret",
    responses(
        (status = 200, description = "Regenerate TURN static_auth_secret successfully"),
    ),
)]
#[post("/settings/turn/regenerate-secret")]
pub async fn regenerate_turn_secret(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let mut settings = settings.write().await;
    let new_secret = uuid::Uuid::new_v4().to_string().replace("-", "");
    settings.turn.static_auth_secret = Some(new_secret);
    settings.save()?;
    info!("Regenerate TURN static_auth_secret successfully");
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
    tag = "Security",
    summary = "Query security settings",
    responses(
        (status = 200, description = "Query security settings successfully",
         body = RestResponse<SecuritySettings>),
    ),
)]
#[get("/security-settings")]
pub async fn query_security_settings(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    let security = settings.security.clone();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(security)))
}

#[utoipa::path(
    tag = "Security",
    summary = "Update security settings",
    request_body(content = SecuritySettings),
    responses(
        (status = 200, description = "Update security settings successfully"),
    ),
)]
#[post("/security-settings")]
pub async fn update_security_settings(
    request_json: web::Json<SecuritySettings>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let mut params = request_json.into_inner();
    // Collapse an omitted (`None`) approval timeout to the finite default so an
    // unattended host never keeps inbound control requests hanging; "never" is
    // carried explicitly as the present value `Some(0)`.
    params.normalize();
    let mut settings = settings.write().await;
    settings.security = params;
    settings.save()?;
    info!(
        "Update security settings successfully, {:?}",
        settings.security
    );
    Ok(HttpResponse::Ok().finish())
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct SecurityApprovalSubmitParams {
    pub req_id: String,
    pub approved: bool,
    pub remember: bool,
}

#[utoipa::path(
    tag = "Security",
    summary = "Submit security approval",
    request_body(content = SecurityApprovalSubmitParams),
    responses(
        (status = 200, description = "Submit security approval successfully"),
    ),
)]
#[post("/security-settings/approval/submit")]
pub async fn submit_security_approval(
    request_json: web::Json<SecurityApprovalSubmitParams>,
    hub: web::Data<Option<Arc<HostControlHub>>>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let response = ApprovalResponse {
        approved: params.approved,
        remember: params.remember,
    };

    let dispatched = match hub.as_ref() {
        Some(hub) => hub.submit_approval(&params.req_id, response),
        None => false,
    };

    if !dispatched {
        log::debug!(
            "submit_security_approval: req_id={} not found locally (hub mode={:?})",
            params.req_id,
            hub.as_ref().as_ref().map(|h| h.mode())
        );
    }

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(true)))
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct ApprovalAckParams {
    pub req_id: String,
}

/// Acknowledge that an approval dialog has mounted and can reach the backend.
///
/// This is the readiness signal for the host-control hub's UI-ready probe (see
/// `HostControlHub::request_approval`). The response `data` is `true` when the
/// request is known to the hub (the dialog should enable its buttons) and
/// `false` when it is unknown (the dialog should show a "not ready" state and
/// must NOT submit — the hub's probe timeout produces the authoritative deny).
/// A stale browser session yields a 401 from the session middleware, which the
/// dialog also treats as "not ready".
#[utoipa::path(
    tag = "Security",
    summary = "Acknowledge security approval dialog readiness",
    request_body(content = ApprovalAckParams),
    responses(
        (status = 200, description = "Acknowledge security approval readiness"),
    ),
)]
#[post("/security-settings/approval/ack")]
pub async fn ack_security_approval(
    request_json: web::Json<ApprovalAckParams>,
    hub: web::Data<Option<Arc<HostControlHub>>>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let ready = match hub.as_ref() {
        Some(hub) => hub.notify_approval_ack(&params.req_id),
        None => false,
    };
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(ready)))
}

#[utoipa::path(
    tag = "TurnClient",
    summary = "Query turn client settings",
    responses(
        (status = 200, description = "Query turn client settings successfully", body=RestResponse<TurnClientSettings>),
    ),
)]
#[get("/settings/turn-client")]
pub async fn query_turn_client_settings(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let settings = settings.read().await;
    let turn_client_settings = settings.turn_client.clone();
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(turn_client_settings)))
}

#[utoipa::path(
    tag = "TurnClient",
    summary = "Update turn client settings",
    request_body(content = TurnClientSettings),
    responses(
        (status = 200, description = "Update turn client settings successfully"),
    ),
)]
#[post("/settings/turn-client")]
pub async fn update_turn_client_settings(
    request_json: web::Json<TurnClientSettings>,
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let mut settings = settings.write().await;

    settings.turn_client = params;
    settings.save()?;
    info!("Update turn client settings successfully");
    Ok(HttpResponse::Ok().finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_control::HostControlHub;
    use crate::model::security_approval::SecurityPermissionType;
    use crate::model::settings::Settings;
    use actix_web::{App, test};

    /// `GET /settings/ai-policy` returns the edge execution policy (the local
    /// execution-mode ceiling). It carries no secret.
    #[actix_web::test]
    async fn query_ai_policy_settings_returns_execution_mode() {
        let mut settings = Settings::default();
        settings.ai_policy.execution_mode = desk_agent_protocol::ExecutionMode::ConfirmEachAction;
        let shared = web::Data::new(SharedSettings::from(settings));
        let app = test::init_service(
            App::new()
                .app_data(shared.clone())
                .service(query_ai_policy_settings),
        )
        .await;

        let req = test::TestRequest::get()
            .uri("/settings/ai-policy")
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(
            body["data"]["execution_mode"].as_str(),
            Some("confirm_each_action")
        );
        // The policy has no credential fields to leak.
        assert!(body["data"].get("api_key").is_none());
        assert!(body["data"].get("base_url").is_none());
    }

    /// `POST /settings/ai-policy` applies the execution-mode update and persists
    /// it; a not-yet-selectable mode is ignored.
    #[actix_web::test]
    async fn update_ai_policy_settings_applies_update() {
        let mut settings = Settings::default();
        let tmp = std::env::temp_dir().join(format!("lrd-pr0-{}.toml", uuid::Uuid::new_v4()));
        settings.args.config_file_path = tmp.to_string_lossy().into_owned();
        let shared = web::Data::new(SharedSettings::from(settings));

        let app = test::init_service(
            App::new()
                .app_data(shared.clone())
                .service(update_ai_policy_settings),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/settings/ai-policy")
            .set_json(AiExecutionPolicyUpdate {
                execution_mode: Some(desk_agent_protocol::ExecutionMode::ConfirmEachAction),
            })
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);

        let stored = shared.read().await;
        assert_eq!(
            stored.ai_policy.execution_mode,
            desk_agent_protocol::ExecutionMode::ConfirmEachAction
        );
        let _ = std::fs::remove_file(&tmp);
    }

    fn hub_data(hub: Option<HostControlHub>) -> web::Data<Option<Arc<HostControlHub>>> {
        web::Data::new(hub.map(Arc::new))
    }

    // A known req_id (here registered as worker-originated) acks as ready.
    #[actix_web::test]
    async fn ack_known_req_returns_ready_true() {
        let hub = HostControlHub::new_aggregator();
        hub.register_upstream_request("r1".to_string(), 1, SecurityPermissionType::Terminal, None);

        let app = test::init_service(
            App::new()
                .app_data(hub_data(Some(hub)))
                .service(ack_security_approval),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/security-settings/approval/ack")
            .set_json(ApprovalAckParams {
                req_id: "r1".to_string(),
            })
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["data"].as_bool(), Some(true));
    }

    // An unknown req_id acks as not-ready (ready:false).
    #[actix_web::test]
    async fn ack_unknown_req_returns_ready_false() {
        let hub = HostControlHub::new_local();
        let app = test::init_service(
            App::new()
                .app_data(hub_data(Some(hub)))
                .service(ack_security_approval),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/security-settings/approval/ack")
            .set_json(ApprovalAckParams {
                req_id: "ghost".to_string(),
            })
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(body["data"].as_bool(), Some(false));
    }

    // macOS auto_start is single-source-of-truth: derived from the LaunchAgent
    // plist on read, and OS-applied (not persisted) on write.
    // SystemSettings/Settings have private fields, so these build via Default
    // then assign the public fields under test.
    #[cfg(target_os = "macos")]
    #[actix_web::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn query_settings_derives_auto_start_from_launchagent() {
        // A stale persisted `true` must be overridden by the real plist state.
        let mut s = Settings::default();
        s.system.auto_start = Some(true);
        let shared = web::Data::new(SharedSettings::from(s));
        let app = test::init_service(App::new().app_data(shared).service(query_settings)).await;

        let req = test::TestRequest::get().uri("/settings").to_request();
        let body: RestResponse<SystemSettings> = test::call_and_read_body_json(&app, req).await;
        // The returned value equals the plist-derived state, not the persisted
        // flag (which would differ in a clean env where no plist exists).
        assert_eq!(
            body.data.unwrap().auto_start,
            Some(crate::macos_agent::is_configured())
        );
    }

    #[cfg(target_os = "macos")]
    #[actix_web::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn update_settings_enable_auto_start_outside_app_dir_fails_without_persisting() {
        // The test binary is not inside /Applications, so enabling auto-start
        // must fail the app-dir guard, return a business error (HTTP 200 + a
        // non-success body code), and must NOT fall through to persisting.
        let shared = web::Data::new(SharedSettings::from(Settings::default()));
        let gate = web::Data::new(Arc::new(ManagerLinkGate::new(false)));
        let app = test::init_service(
            App::new()
                .app_data(shared)
                .app_data(gate)
                .service(update_settings),
        )
        .await;

        let mut payload = SystemSettings::default();
        payload.auto_start = Some(true);
        let req = test::TestRequest::post()
            .uri("/settings")
            .set_json(&payload)
            .to_request();
        let resp = test::call_service(&app, req).await;
        // Business errors are HTTP 200 with the error in the body code.
        assert_eq!(resp.status(), 200);
        let body: RestResponse<()> = test::read_body_json(resp).await;
        assert!(!body.success);
        assert_eq!(body.code, DeskErrorCode::AUTO_START_ERROR.code());
    }

    fn settings_with_temp_path() -> SharedSettings {
        let mut settings = Settings::default();
        let mut temp_path = std::env::temp_dir();
        temp_path.push(format!("desk_settings_test_{}.toml", uuid::Uuid::new_v4()));
        settings.args = crate::model::settings::Args {
            config_file_path: temp_path.to_string_lossy().to_string(),
            ..Default::default()
        };
        SharedSettings::from(settings)
    }

    #[actix_web::test]
    #[allow(clippy::field_reassign_with_default)]
    async fn update_settings_syncs_manager_link_gate() {
        let shared = web::Data::new(settings_with_temp_path());
        // Gate starts enabled; the update must drive it from the persisted config.
        let gate = Arc::new(ManagerLinkGate::new(true));
        let gate_data = web::Data::new(gate.clone());
        let app = test::init_service(
            App::new()
                .app_data(shared)
                .app_data(gate_data)
                .service(update_settings),
        )
        .await;

        // Configured manager but explicitly disabled -> gate should go false.
        let mut payload = SystemSettings::default();
        payload.manager_url = Some("wss://manager.example/api/desk/signaling".to_string());
        payload.manager_api_token = Some("tok".to_string());
        payload.manager_enabled = Some(false);
        let req = test::TestRequest::post()
            .uri("/settings")
            .set_json(&payload)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        assert!(!gate.should_connect(), "disabled manager -> gate false");

        // Re-enable (manager_enabled None) with config present -> gate true.
        let mut payload = SystemSettings::default();
        payload.manager_url = Some("wss://manager.example/api/desk/signaling".to_string());
        payload.manager_api_token = Some("tok".to_string());
        payload.manager_enabled = None;
        let req = test::TestRequest::post()
            .uri("/settings")
            .set_json(&payload)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        assert!(gate.should_connect(), "enabled + configured -> gate true");
    }
}
