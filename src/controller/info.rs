use actix_web::{Error, HttpResponse, get, web};
use sysinfo::System;

use crate::{
    desk_error::DeskError,
    model::{common::RestResponse, info::SystemInfo, settings::SharedSettings},
};

#[utoipa::path(
    summary = "Get system information",
    responses(
        (status = 200, description = "Get system information successfully", body=RestResponse<SystemInfo>),
    ),
)]
#[get("/sysinfo")]
pub async fn query_sysinfo(settings: web::Data<SharedSettings>) -> Result<HttpResponse, DeskError> {
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
    let system_info = SystemInfo::from(&sys);
    log::info!(
        "Get system information successfully, info: {:?}",
        system_info
    );
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(system_info)))
}
