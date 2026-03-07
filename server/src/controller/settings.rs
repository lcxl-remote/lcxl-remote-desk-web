use actix_web::{Error as AWError, HttpResponse, get, post, web};
use desk_utils::rest::RestResponse;
use log::info;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::settings::{SharedSettings, SystemSettings};

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
    if let Some(auto_start_enable) = params.auto_start {
        if let Err(e) = crate::service::auto_start::update_auto_start_status(auto_start_enable) {
            log::error!("Failed to update auto start status: {:?}", e);
        }
    }

    settings.system = params;
    // save new settings to file
    settings.save()?;
    info!("Update system settings successfully, {:?}", settings.system);
    Ok(HttpResponse::Ok().finish())
}

#[utoipa::path(
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
