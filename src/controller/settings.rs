use actix_web::{Error as AWError, HttpResponse, get, post, web};
use log::info;

use crate::model::{
    common::RestResponse,
    settings::{SharedSettings, SystemSettings},
};

#[utoipa::path(
    summary = "Query settings",
    responses(
        (status = 200, description = "Query settings successfully", body=RestResponse<SystemSettings>),
    ),
)]
#[get("/settings")]
pub async fn query_settings(settings: web::Data<SharedSettings>) -> Result<HttpResponse, AWError> {
    let settings = settings.lock().await;
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
    let mut settings = settings.lock().await;
    settings.system = params;
    // save new settings to file
    settings.save()?;
    info!("Update system settings successfully, {:?}", settings.system);
    Ok(HttpResponse::Ok().finish())
}
