use actix_web::{Error as AWError, HttpResponse, get, post, web};
use desk_utils::rest::RestResponse;
use log::info;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use std::sync::Arc;

use crate::host_control::{ApprovalResponse, HostControlHub};
use crate::model::settings::{LogSettings, SharedSettings, SystemSettings, TurnClientSettings};
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
    let system_settings = settings.system.clone();
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
) -> Result<HttpResponse, AWError> {
    let params = requst_json.into_inner();
    let mut settings = settings.write().await;

    // Check auto_start flag and update system registry/startup folder if needed
    if let Some(auto_start_enable) = params.auto_start
        && let Err(e) = update_auto_start_status(auto_start_enable)
    {
        log::error!("Failed to update auto start status: {:?}", e);
    }

    settings.system = params;
    // save new settings to file
    settings.save()?;
    info!("Update system settings successfully, {:?}", settings.system);
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
    let params = request_json.into_inner();
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
    use actix_web::{App, test};

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
}
