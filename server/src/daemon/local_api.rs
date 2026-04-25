use crate::model::settings::SharedSettings;
use actix_web::{App, HttpResponse, HttpServer, get, web};
use desk_server_version::SERVER_API_VERSION;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const SERVICE_API_PORT: u16 = 8082;

#[derive(Serialize, Deserialize)]
struct ServiceServerInfo {
    startup_mode: String,
    api_version: i32,
    initialized: bool,
    service_installed: bool,
    is_admin: bool,
}

#[derive(Serialize)]
struct ApiResponse<T: Serialize> {
    success: bool,
    code: i32,
    data: T,
}

#[get("/api/server_info")]
async fn service_server_info(settings: web::Data<Arc<SharedSettings>>) -> HttpResponse {
    let initialized = {
        let s = settings.read().await;
        !s.user.login_password.is_empty()
    };

    let info = ServiceServerInfo {
        startup_mode: "service-daemon".into(),
        api_version: SERVER_API_VERSION,
        initialized,
        service_installed: true,
        is_admin: desk_utils::permission::is_admin(),
    };

    HttpResponse::Ok().json(ApiResponse {
        success: true,
        code: 200,
        data: info,
    })
}

pub async fn run_local_api(
    settings: Arc<SharedSettings>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("ServiceDaemon local API starting on 127.0.0.1:{SERVICE_API_PORT}");

    let settings_data = web::Data::new(settings);

    let server = HttpServer::new(move || {
        App::new()
            .app_data(settings_data.clone())
            .service(service_server_info)
    })
    .bind(("127.0.0.1", SERVICE_API_PORT))
    .map_err(|e| format!("Failed to bind local API on port {SERVICE_API_PORT}: {e}"))?
    .run();

    server.await.map_err(|e| {
        error!("Local API server error: {e}");
        Box::new(e) as Box<dyn std::error::Error>
    })
}
