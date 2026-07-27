use crate::{
    ApiRouteConfig, ApiSurfaceOpts, api_json_config, configure_api_surface,
    daemon::tauri_ipc::TauriIpcBridge,
    host_control,
    model::settings::{SharedSettings, StartupMode},
    service::signaling::LocalNodeTokenValidator,
};
use actix_files;
use actix_service::fn_service;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{App, HttpServer, cookie::Key, dev::ServiceResponse, middleware::Logger, web};
use desk_signal::model::SharedConnectionMap;
use desk_signal_facade::service::NodeTokenValidator;
use log::{error, info};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::oneshot;
use utoipa_actix_web::AppExt as _;

pub const SERVICE_API_PORT: u16 = 8082;

/// Run the daemon's local HTTP API.
///
/// `host_control_hub` is the Aggregator-mode hub shared between this server and
/// the rest of the daemon. It owns the `/ws/tauri_ipc` (Tauri shell) and
/// `/ws/host_upstream` (worker forwarder) endpoints.
///
/// `ready_tx`, when supplied, is fired exactly once after the HTTP server has
/// successfully bound its listening socket. Workers should not be spawned until
/// the signal arrives so their forwarder ws clients connect on the first try.
#[allow(clippy::too_many_arguments)]
pub async fn run_local_api(
    settings: Arc<SharedSettings>,
    settings_coordinator: Arc<crate::model::settings_coordinator::SettingsCoordinator>,
    tauri_bridge: Arc<TauriIpcBridge>,
    host_control_hub: Arc<host_control::HostControlHub>,
    manager_link_state: Arc<super::manager_link_state::ManagerLinkState>,
    support_link_state: Arc<super::support_link_state::SupportLinkState>,
    manager_link_gate: Arc<super::manager_link_gate::ManagerLinkGate>,
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
    let settings_coordinator_data = web::Data::from(settings_coordinator);
    let connection_map = web::Data::new(SharedConnectionMap::from(BTreeMap::new()));
    let tauri_is_admin_data = web::Data::new(Arc::clone(&tauri_bridge.tauri_is_admin));

    let validator: Arc<dyn NodeTokenValidator> = Arc::new(LocalNodeTokenValidator {
        settings: settings_data.clone(),
    });
    let validator_data = web::Data::new(validator);
    let manager_link_state_data = web::Data::new(manager_link_state);
    let support_link_state_data = web::Data::new(support_link_state);
    let manager_link_gate_data = web::Data::new(manager_link_gate);

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
        .with_settings(settings_data.clone())
        .with_settings_coordinator(settings_coordinator_data.clone().into_inner())
        .with_tauri_is_admin(Arc::clone(&tauri_bridge.tauri_is_admin)),
    );

    let route_config = ApiRouteConfig {
        settings: settings_data.clone(),
        tauri_login_token: login_token_data,
        connection_map,
        host_control_hub: web::Data::new(Some(Arc::clone(&host_control_hub))),
        tauri_is_admin: Some(tauri_is_admin_data),
    };

    let server = HttpServer::new(move || {
        let default_path = static_file_path.clone();
        let rc = route_config.clone();
        let endpoint_state_for_routes = Arc::clone(&endpoint_state);
        let tauri_is_admin = rc.tauri_is_admin.clone();

        App::new()
            .into_utoipa_app()
            .map(|app| app.wrap(Logger::default()))
            // App-level `app_data` (previously mounted inside `configure_api_routes`).
            .app_data(rc.settings.clone())
            .app_data(settings_coordinator_data.clone())
            .app_data(rc.tauri_login_token.clone())
            .app_data(rc.connection_map.clone())
            .app_data(rc.host_control_hub.clone())
            .app_data(validator_data.clone())
            .app_data(manager_link_state_data.clone())
            .app_data(support_link_state_data.clone())
            .app_data(manager_link_gate_data.clone())
            // The daemon hosts no TURN runtime, and says so rather than leaving
            // the runtime endpoints without the state they extract.
            .app_data(actix_web::web::Data::new(
                desk_turn::runtime::TurnRuntimeView::unsupported(),
            ))
            .app_data(api_json_config())
            .configure(move |cfg| {
                if let Some(admin) = tauri_is_admin {
                    cfg.app_data(admin);
                }
            })
            // Host-control `/ws/*` are plain actix (non-utoipa) and depend on
            // runtime endpoint state, so they stay here. Bridge into the inner
            // plain `ServiceConfig` via `.map` (closure must return `inner`).
            .configure(move |cfg| {
                cfg.map(|inner| {
                    host_control::endpoint::register_routes(inner, endpoint_state_for_routes);
                    inner
                });
            })
            // Single source of truth for the HTTP API surface. The daemon serves
            // signaling + file/device-code unconditionally. The signal DB is
            // opened by the daemon bootstrap before this API comes up, so the
            // DB-backed views (including the TURN usage history) are served. The
            // TURN runtime endpoints are registered too, answering "this mode
            // does not relay" from the unsupported runtime view mounted below.
            .configure(|cfg| {
                configure_api_surface(
                    cfg,
                    ApiSurfaceOpts {
                        include_signaling: true,
                        include_device_code: true,
                        has_signal_db: crate::startup_mode_has_signal_db(
                            &StartupMode::ServiceDaemon,
                        ),
                        include_model_usage: true,
                    },
                )
            })
            .into_app()
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), secret_key.clone())
                    .cookie_secure(false)
                    // Encrypt (not just sign) the session cookie and keep it out of
                    // page JavaScript, matching the portable / daemon HTTP apps.
                    .cookie_content_security(actix_session::config::CookieContentSecurity::Private)
                    .cookie_http_only(true)
                    .build(),
            )
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
