use crate::{
    ApiRouteConfig, ExternalChannels,
    daemon::tauri_ipc::TauriIpcBridge,
    model::settings::{SharedSettings, StartupMode},
};
use actix_files;
use actix_service::fn_service;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{App, HttpServer, cookie::Key, dev::ServiceResponse, middleware::Logger, web};
use desk_signal::model::SharedConnectionMap;
use log::{error, info};
use std::collections::BTreeMap;
use std::sync::Arc;

pub const SERVICE_API_PORT: u16 = 8082;

pub async fn run_local_api(
    settings: Arc<SharedSettings>,
    tauri_bridge: Arc<TauriIpcBridge>,
    channels: ExternalChannels,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("ServiceDaemon HTTP server starting on 127.0.0.1:{SERVICE_API_PORT}");

    // Stable cookie-signing key derived from persisted secret (survives daemon restarts)
    let session_key_material = {
        let s = settings.read().await;
        s.system.session_secret_key.clone().unwrap_or_default()
    };
    let secret_key = Key::derive_from(session_key_material.as_bytes());

    // Static file path: <exe_dir>/static
    let static_file_path = std::env::current_exe()
        .map(|mut p| {
            p.pop();
            p.push("static");
            p
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("static"));

    let settings_data = web::Data::from(settings.clone());
    let connection_map = web::Data::new(SharedConnectionMap::from(BTreeMap::new()));
    let tauri_is_admin_data = web::Data::new(Arc::clone(&tauri_bridge.tauri_is_admin));

    // Share the bridge's TauriLoginToken clone with the HTTP server so that
    // refresh() calls from the WS handler are visible to the login controller.
    let login_token_data = web::Data::new(Some(tauri_bridge.tauri_login_token.clone()));

    let bridge_data: web::Data<Arc<TauriIpcBridge>> = web::Data::new(Arc::clone(&tauri_bridge));

    let route_config = ApiRouteConfig {
        settings: settings_data.clone(),
        tauri_login_token: login_token_data,
        connection_map,
        security_approval_sender: web::Data::new(channels.security_approval_sender),
        service_op_sender: web::Data::new(channels.service_op_sender),
        tauri_is_admin: Some(tauri_is_admin_data),
        startup_mode: StartupMode::ServiceDaemon,
    };

    let server = HttpServer::new(move || {
        let default_path = static_file_path.clone();
        let rc = route_config.clone();
        let bd = bridge_data.clone();

        App::new()
            .wrap(Logger::default())
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), secret_key.clone())
                    .cookie_secure(false)
                    .build(),
            )
            .configure(move |cfg| crate::configure_api_routes(cfg, rc.clone()))
            .app_data(bd.clone())
            .route("/ws/tauri_ipc", web::get().to(TauriIpcBridge::ws_handler))
            .service(
                actix_files::Files::new("/", static_file_path.clone())
                    .index_file("index.html")
                    .default_handler(fn_service(move |req: actix_web::dev::ServiceRequest| {
                        let (http_req, _) = req.into_parts();
                        let path = default_path.clone().join("index.html");
                        async move {
                            let response =
                                actix_files::NamedFile::open(path)?.into_response(&http_req);
                            Ok(ServiceResponse::new(http_req, response))
                        }
                    })),
            )
    })
    .bind(("127.0.0.1", SERVICE_API_PORT))
    .map_err(|e| format!("Failed to bind local API on port {SERVICE_API_PORT}: {e}"))?
    .run();

    server.await.map_err(|e| {
        error!("Local API server error: {e}");
        Box::new(e) as Box<dyn std::error::Error>
    })
}
