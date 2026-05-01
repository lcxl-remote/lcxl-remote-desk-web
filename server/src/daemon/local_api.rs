use crate::{
    ApiRouteConfig, ExternalChannels,
    daemon::tauri_ipc::TauriIpcBridge,
    host_control,
    model::settings::{SharedSettings, StartupMode},
    service::signaling::LocalNodeTokenValidator,
};
use actix_files;
use actix_service::fn_service;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{App, HttpServer, cookie::Key, dev::ServiceResponse, middleware::Logger, web};
use desk_signal::{controller::signaling::open_signaling_handle, model::SharedConnectionMap};
use desk_signal_facade::service::NodeTokenValidator;
use log::{error, info};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::oneshot;

pub const SERVICE_API_PORT: u16 = 8082;

/// Run the daemon's local HTTP API.
///
/// `host_control_hub` is the Aggregator-mode hub shared between this server and
/// the rest of the daemon. It owns the `/ws/tauri_ipc` (Tauri shell) and
/// `/ws/host_upstream` (worker forwarder) endpoints.
///
/// `ready_tx`, when supplied, is fired exactly once after the HTTP server has
/// successfully bound its listening socket. Workers should not be spawned until
/// the signal arrives so their forwarder ws clients connect on the first try
/// (plan review #5).
pub async fn run_local_api(
    settings: Arc<SharedSettings>,
    tauri_bridge: Arc<TauriIpcBridge>,
    channels: ExternalChannels,
    host_control_hub: Arc<host_control::HostControlHub>,
    ready_tx: Option<oneshot::Sender<()>>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("ServiceDaemon HTTP server starting on 0.0.0.0:{SERVICE_API_PORT}");

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

    if !static_file_path.exists() {
        error!(
            "Frontend static directory not found: {}. \
             Build the vite project and place the output in a 'static/' directory \
             alongside the server binary before installing the service.",
            static_file_path.display()
        );
    }

    let settings_data = web::Data::from(settings.clone());
    let connection_map = web::Data::new(SharedConnectionMap::from(BTreeMap::new()));
    let tauri_is_admin_data = web::Data::new(Arc::clone(&tauri_bridge.tauri_is_admin));

    let validator: Arc<dyn NodeTokenValidator> = Arc::new(LocalNodeTokenValidator {
        settings: settings_data.clone(),
    });
    let validator_data = web::Data::new(validator);

    // Share the bridge's TauriLoginToken clone with the HTTP server so that
    // refresh() calls from the WS handler are visible to the login controller.
    let login_token_data = web::Data::new(Some(tauri_bridge.tauri_login_token.clone()));

    // Build the host-control endpoint state. The Aggregator hub routes between
    // forwarder upstream sessions and the connected Tauri shell.
    let ipc_token = {
        let s = settings.read().await;
        s.system.tauri_ipc_token.clone().unwrap_or_default()
    };
    let endpoint_state = Arc::new(
        host_control::endpoint::EndpointState::new(
            Arc::clone(&host_control_hub),
            ipc_token,
            tauri_bridge.tauri_login_token.clone(),
        )
        .with_tauri_is_admin(Arc::clone(&tauri_bridge.tauri_is_admin)),
    );

    // The legacy mpsc security_approval_sender is no longer in use — Step 3 moved
    // approvals onto HostControlHub. Drop it explicitly so the unused-field lint
    // is satisfied while Step 6 finishes deleting the bridge.
    let _ = channels.security_approval_sender;

    let route_config = ApiRouteConfig {
        settings: settings_data.clone(),
        tauri_login_token: login_token_data,
        connection_map,
        host_control_hub: web::Data::new(Some(Arc::clone(&host_control_hub))),
        service_op_sender: web::Data::new(channels.service_op_sender),
        tauri_is_admin: Some(tauri_is_admin_data),
        startup_mode: StartupMode::ServiceDaemon,
    };

    let server = HttpServer::new(move || {
        let default_path = static_file_path.clone();
        let rc = route_config.clone();
        let endpoint_state_data: web::Data<Arc<host_control::endpoint::EndpointState>> =
            web::Data::new(Arc::clone(&endpoint_state));

        App::new()
            .wrap(Logger::default())
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), secret_key.clone())
                    .cookie_secure(false)
                    .build(),
            )
            // Register signaling and IPC routes BEFORE configure_api_routes so they
            // are matched first and bypass the /api scope's reject_anonymous_users
            // middleware (which would otherwise intercept /api/desk/signaling).
            .app_data(validator_data.clone())
            .service(open_signaling_handle)
            // Host-control hub endpoints replace the legacy `TauriIpcBridge`
            // ws handler. Tauri shells connect to /ws/tauri_ipc; worker
            // forwarders connect to /ws/host_upstream.
            .app_data(endpoint_state_data)
            .route(
                "/ws/tauri_ipc",
                web::get().to(host_control::endpoint::ws_handler),
            )
            .route(
                "/ws/host_upstream",
                web::get().to(host_control::endpoint::ws_upstream_handler),
            )
            .configure(move |cfg| crate::configure_api_routes(cfg, rc.clone()))
            .service(
                actix_files::Files::new("/", static_file_path.clone())
                    .index_file("index.html")
                    .default_handler(fn_service(move |req: actix_web::dev::ServiceRequest| {
                        let (http_req, _) = req.into_parts();
                        let path = default_path.clone().join("index.html");
                        async move {
                            match actix_files::NamedFile::open(&path) {
                                Ok(file) => {
                                    let resp = file.into_response(&http_req);
                                    Ok(ServiceResponse::new(http_req, resp))
                                }
                                Err(_) => {
                                    let body = format!(
                                        "Frontend assets not found (looked for: {}).\n\
                                         Build the vite project and place the output in a \
                                         'static/' directory alongside the server binary, \
                                         then reinstall the service.",
                                        path.display()
                                    );
                                    let resp = actix_web::HttpResponse::ServiceUnavailable()
                                        .content_type("text/plain; charset=utf-8")
                                        .body(body);
                                    Ok(ServiceResponse::new(http_req, resp))
                                }
                            }
                        }
                    })),
            )
    })
    .bind(("0.0.0.0", SERVICE_API_PORT))
    .map_err(|e| format!("Failed to bind local API on port {SERVICE_API_PORT}: {e}"))?
    .run();

    // Bind succeeded — release any waiter (workers can now connect upstream).
    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }

    server.await.map_err(|e| {
        error!("Local API server error: {e}");
        Box::new(e) as Box<dyn std::error::Error>
    })
}
