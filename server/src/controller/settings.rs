use actix_web::{Error as AWError, HttpResponse, get, post, web};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};
use log::info;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use std::sync::Arc;

use crate::daemon::manager_link_gate::ManagerLinkGate;
use crate::daemon::signaling_proxy::manager_link_should_connect;
use crate::host_control::{ApprovalResponse, HostControlHub};
use crate::model::settings::{
    AiExecutionPolicyPublic, AiExecutionPolicyUpdate, CollectionPolicySettings,
    CollectionPolicySettingsUpdate, LogSettings, Settings, SharedSettings, SystemSettings,
    TurnClientSettings,
};
use crate::model::settings_coordinator::SettingsCoordinator;
use crate::service::auto_start::update_auto_start_status;
use crate::service::turn_runtime::TurnRuntimeControl;
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
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(
        system_settings.without_internal_secrets(),
    )))
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
    coordinator: web::Data<SettingsCoordinator>,
    manager_link_gate: web::Data<Arc<ManagerLinkGate>>,
    host_control_hub: Option<web::Data<Option<Arc<HostControlHub>>>>,
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();

    // Apply the auto-start change to the OS first. On macOS the LaunchAgent is
    // the single source of truth, so a failure must surface as a business error
    // and must NOT fall through to persisting an inconsistent flag. (Windows /
    // Linux keep the prior behavior via the same call; config_file_path is only
    // consumed on macOS to write an absolute --config-file-path into the plist.)
    if let Some(auto_start_enable) = params.auto_start {
        let config_file_path = settings.read().await.args.config_file_path.clone();
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

    let outcome = coordinator
        .commit_with_effect(
            move |settings| {
                // The console form omits the auto-generated internal fields;
                // carry them over from the current settings so a full replace
                // doesn't wipe client_id and the signaling/IPC/session secrets.
                let mut params = params;
                params.preserve_internal_fields(&settings.system);
                settings.system = params;
                Ok(())
            },
            // Re-sync the shared manager-link gate while the settings are still
            // locked, so the proxy's reconnect loop cannot observe the new
            // settings before the gate value catches up. Disabling the manager
            // connection here tears down the current upstream; re-enabling lets
            // the reconnect loop bring it back.
            |settings| {
                manager_link_gate.set(manager_link_should_connect(
                    &settings.system.manager_url,
                    &settings.system.manager_api_token,
                    settings.system.manager_enabled,
                ));
            },
        )
        .await?;

    let hub = host_control_hub
        .as_ref()
        .and_then(|data| data.get_ref().as_ref());
    if let Some(hub) = hub {
        hub.host_activity()
            .set_indicator_enabled(settings.read().await.system.host_access_indicator_enabled);
        if let Some(locale) = outcome.locale_changed_to {
            let _ = hub.send_command(
                crate::host_control::HostControlMessage::GlobalLocaleChanged { locale },
            );
        }
    }
    info!(
        "Update system settings successfully, {:?}",
        settings.read().await.system
    );
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
    control: Option<web::Data<TurnRuntimeControl>>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    let mut settings = settings.write().await;

    let mut candidate = settings.clone();
    candidate.turn.realm = params.realm;
    candidate.turn.interfaces = params.interfaces;
    candidate.turn.enable_turn = params.enable_turn;
    candidate.turn.relay_min_port = params.relay_min_port;
    candidate.turn.relay_max_port = params.relay_max_port;

    commit_turn_settings(
        &mut settings,
        candidate,
        control.as_ref().map(|c| c.get_ref()),
    )?;
    info!("Update turn settings successfully");
    Ok(HttpResponse::Ok().finish())
}

/// Persist the TURN settings and make the running service match them.
///
/// Every endpoint that writes TURN settings goes through here. Saving without
/// applying is what left a rotated secret on disk while the running server kept
/// validating against the old one — a disagreement that never healed, because
/// nothing else was watching the file.
///
/// The caller edits a copy and hands it over; `live` only takes the new values
/// once they are on disk. Editing the live settings first would leave a failed
/// write with three different answers in play — the process on the new values,
/// the file on the old ones, and the relay still serving the old ones — and
/// nothing would ever reconcile them. Nobody publishes a value it could not
/// store.
///
/// Success means "saved, and the new state has been published". It deliberately
/// does not mean the runtime has already converged: rebinding sockets can fail
/// and be retried, and holding the request open for that would report a
/// transient bind failure as a failed save.
fn commit_turn_settings(
    live: &mut Settings,
    candidate: Settings,
    control: Option<&TurnRuntimeControl>,
) -> Result<(), AWError> {
    candidate.save()?;
    *live = candidate;
    match control {
        Some(control) => control.apply(&live.turn),
        // No runtime control in this process (the daemon's local API serves the
        // settings pages but hosts no relay); saving is the whole job.
        None => log::debug!("no TURN runtime in this process; settings saved only"),
    }
    Ok(())
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
    control: Option<web::Data<TurnRuntimeControl>>,
) -> Result<HttpResponse, AWError> {
    let mut settings = settings.write().await;
    let new_secret = uuid::Uuid::new_v4().to_string().replace("-", "");
    let mut candidate = settings.clone();
    candidate.turn.static_auth_secret = Some(new_secret);
    // The secret is what the running server validates credentials against, so
    // this write restarts the runtime like any other.
    commit_turn_settings(
        &mut settings,
        candidate,
        control.as_ref().map(|c| c.get_ref()),
    )?;
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
    coordinator: web::Data<SettingsCoordinator>,
) -> Result<HttpResponse, AWError> {
    let params = request_json.into_inner();
    // The commit normalizes an omitted (`None`) approval timeout to the finite
    // default, so an unattended host never keeps inbound control requests
    // hanging; "never" is carried explicitly as the present value `Some(0)`.
    coordinator
        .commit(move |settings| {
            settings.security = params;
            Ok(())
        })
        .await?;
    info!(
        "Update security settings successfully, {:?}",
        coordinator.security()
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
                max_concurrent_executions: None,
                max_command_runtime_seconds: None,
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
        let shared = Arc::new(settings_with_temp_path());
        let coordinator = web::Data::from(Arc::new(
            SettingsCoordinator::from_settings(Arc::clone(&shared)).await,
        ));
        // Gate starts enabled; the update must drive it from the persisted config.
        let gate = Arc::new(ManagerLinkGate::new(true));
        let gate_data = web::Data::new(gate.clone());
        let app = test::init_service(
            App::new()
                .app_data(web::Data::from(shared))
                .app_data(coordinator)
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

    /// Both endpoints that persist TURN settings must also make the running
    /// service match them. The rotation endpoint is the one that used to save
    /// and stop there, leaving the file and the live server disagreeing about
    /// the secret forever — the running server kept rejecting every credential
    /// signed with the new one.
    #[actix_web::test]
    async fn both_turn_write_endpoints_reach_the_running_service() {
        use crate::model::settings::StartupMode;
        use crate::service::turn_runtime::HostTurnDriver;
        use desk_turn::model::{TurnInterface, TurnTransport};
        use desk_turn::runtime::{TurnIntent, TurnPosture, TurnRuntimeView};
        use desk_turn::supervisor::{BackoffConfig, DesiredState, spawn};
        use std::time::Duration;

        let mut settings = Settings::default();
        let tmp = std::env::temp_dir().join(format!("lrd-turn-{}.toml", uuid::Uuid::new_v4()));
        settings.args.config_file_path = tmp.to_string_lossy().into_owned();
        let shared = web::Data::new(SharedSettings::from(settings));

        let connection_map = web::Data::new(desk_signal::model::SharedConnectionMap::from(
            std::collections::BTreeMap::new(),
        ));
        let (posture_tx, posture_rx) =
            tokio::sync::watch::channel(TurnPosture::new(TurnIntent::NotConfigured));
        // Nothing here accounts for usage; the retirement queue is dropped.
        let (supervisor, _retired) = spawn(
            Arc::new(HostTurnDriver::new(connection_map)),
            DesiredState {
                revision: 0,
                params: None,
            },
            BackoffConfig {
                min: Duration::from_millis(5),
                max: Duration::from_millis(20),
            },
        );
        let view = TurnRuntimeView::new(supervisor.clone(), posture_rx);
        let control = web::Data::new(TurnRuntimeControl::new(
            StartupMode::Default,
            supervisor.clone(),
            posture_tx,
            0,
        ));

        let app = test::init_service(
            App::new()
                .app_data(shared.clone())
                .app_data(control.clone())
                .service(update_turn_settings)
                .service(regenerate_turn_secret),
        )
        .await;

        // Nothing is running yet: no interface has been configured.
        assert!(view.runtime().is_none());

        // Saving an interface starts the relay without restarting the process.
        let payload = TurnSettings {
            enable_turn: true,
            interfaces: vec![TurnInterface {
                transport: TurnTransport::UDP,
                listen: "127.0.0.1:0".into(),
                external: "203.0.113.11:3478".into(),
            }],
            ..TurnSettings::default()
        };
        let req = test::TestRequest::post()
            .uri("/settings/turn")
            .set_json(&payload)
            .to_request();
        assert!(test::call_service(&app, req).await.status().is_success());
        let running = wait_for_runtime(&view, true)
            .await
            .expect("saving an interface must start the relay");
        assert_eq!(running.settings.interfaces[0].external, "203.0.113.11:3478");
        let before = running.settings.static_auth_secret.clone();

        // Rotating the secret must reach the running server too, and the value
        // it serves with must be the value that was persisted.
        let req = test::TestRequest::post()
            .uri("/settings/turn/regenerate-secret")
            .to_request();
        assert!(test::call_service(&app, req).await.status().is_success());
        let mut rotated = None;
        for _ in 0..200 {
            match view.runtime() {
                Some(current) if current.settings.static_auth_secret != before => {
                    rotated = Some(current);
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
        let rotated = rotated.expect("a rotated secret must restart the running server");
        let persisted = shared.read().await.turn.static_auth_secret.clone();
        assert!(persisted.is_some());
        assert_eq!(
            rotated.settings.static_auth_secret, persisted,
            "the server must validate against the secret that was saved"
        );

        supervisor.shutdown().await;
        let _ = std::fs::remove_file(&tmp);
    }

    /// A write nobody could store must not be visible either.
    ///
    /// Editing the live settings before persisting them left a failed save with
    /// three answers in play — the process on the new values, the file on the
    /// old ones, and the relay still serving the old ones — and nothing that
    /// would ever reconcile them. For the rotation endpoint that is permanent:
    /// the process would hand out credentials signed with a secret the running
    /// server never received and the file never recorded.
    #[actix_web::test]
    async fn a_turn_write_that_cannot_be_saved_changes_nothing() {
        let mut settings = Settings::default();
        settings.turn.realm = "before".into();
        // A regular file where a directory would have to be: the save fails on
        // its first step, before anything is written anywhere.
        let blocker =
            std::env::temp_dir().join(format!("lrd-turn-blocked-{}", uuid::Uuid::new_v4()));
        std::fs::write(&blocker, b"not a directory").unwrap();
        settings.args.config_file_path = blocker.join("config.toml").to_string_lossy().into_owned();
        let shared = web::Data::new(SharedSettings::from(settings));

        let app = test::init_service(
            App::new()
                .app_data(shared.clone())
                .service(update_turn_settings),
        )
        .await;

        let req = test::TestRequest::post()
            .uri("/settings/turn")
            .set_json(&TurnSettings {
                realm: "after".into(),
                ..TurnSettings::default()
            })
            .to_request();
        let resp = test::call_service(&app, req).await;
        // A business failure answers 200 and carries the verdict in the body, so
        // the status on its own says nothing.
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(
            body["success"], false,
            "a write that could not be stored must be reported as failed"
        );

        assert_eq!(
            shared.read().await.turn.realm,
            "before",
            "the process must still agree with the file it failed to write"
        );

        let _ = std::fs::remove_file(&blocker);
    }

    /// Wait until a runtime is (or is no longer) published.
    async fn wait_for_runtime(
        view: &desk_turn::runtime::TurnRuntimeView,
        present: bool,
    ) -> Option<Arc<desk_turn::model::TurnApiState>> {
        for _ in 0..200 {
            let runtime = view.runtime();
            if runtime.is_some() == present {
                return runtime;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("runtime presence never became {present}");
    }
}
