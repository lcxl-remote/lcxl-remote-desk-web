use actix_web::{HttpResponse, get, web};
use desk_utils::rest::RestResponse;
use sysinfo::System;

use crate::{
    error::DeskError,
    model::{
        info::{ServerInfo, SystemInfo},
        settings::SharedSettings,
    },
};
use desk_server_version::SERVER_API_VERSION;

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
    let mut system_info = SystemInfo::from(&sys);
    system_info.startup_mode = {
        let settings = settings.read().await;
        settings.args.startup_mode.as_ref().to_string()
    };
    log::info!(
        "Get system information successfully, info: {:?}",
        system_info
    );
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(system_info)))
}

#[utoipa::path(
    summary = "Get server information",
    responses(
        (status = 200, description = "Get server information successfully", body=RestResponse<ServerInfo>),
    ),
)]
#[get("/api/server_info")]
pub async fn query_server_info(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    let (startup_mode, initialized) = {
        let settings = settings.read().await;
        let mode = settings.args.startup_mode.as_ref().to_string();

        let init = !settings.user.login_password.is_empty();
        (mode, init)
    };

    let info = ServerInfo {
        startup_mode,
        api_version: SERVER_API_VERSION,
        initialized,
    };

    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(info)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::settings::Settings;
    use actix_web::{App, test};

    #[actix_web::test]
    async fn test_query_sysinfo() {
        let settings = SharedSettings::from(Settings::default());
        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(settings))
                .service(query_sysinfo),
        )
        .await;

        let req = test::TestRequest::get().uri("/sysinfo").to_request();
        let resp = test::call_service(&app, req).await;

        assert!(resp.status().is_success());

        let body: RestResponse<SystemInfo> = test::read_body_json(resp).await;
        assert_eq!(body.code, 200);
        assert!(body.data.is_some());
    }
}
