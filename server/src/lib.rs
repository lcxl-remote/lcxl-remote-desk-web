pub mod controller;
pub mod daemon;
pub mod diagnose;
pub mod error;
pub mod exec;
pub mod host_control;
pub mod mcp;
pub mod model;
pub mod openapi;
pub mod service;
pub mod telemetry;
pub mod version;
pub mod worker;

use std::{
    collections::BTreeMap,
    env,
    fs::{File, TryLockError},
    io::ErrorKind,
    path::Path,
    sync::Arc,
};

use crate::{
    controller::{
        info::{query_backend_info, query_server_info, query_sysinfo},
        init::init_system,
        login::{change_password, get_captcha, login_account, login_tauri, logout_account},
        service_mgmt::{install_service, uninstall_service},
        settings::{
            ack_security_approval, query_ai_model_settings, query_log_settings,
            query_security_settings, query_settings, query_telemetry_status,
            query_turn_client_settings, query_turn_settings, regenerate_turn_secret,
            submit_security_approval, update_ai_model_settings, update_log_settings,
            update_security_settings, update_settings, update_telemetry_consent,
            update_turn_client_settings, update_turn_settings,
        },
        turn::{
            delete_turn_session, get_turn_info, get_turn_metrics, get_turn_session,
            get_turn_session_statistics,
        },
        user::{get_current_user, reject_anonymous_users},
        virtual_display::{
            install_driver as install_virtual_display_driver,
            query_driver_status as query_virtual_display_driver_status,
            query_virtual_display_settings, uninstall_driver as uninstall_virtual_display_driver,
            update_virtual_display_settings,
        },
    },
    model::turn::TurnAuthHandler,
};
use actix_server::Server;
use actix_service::fn_service;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{
    App, HttpResponse, HttpServer,
    cookie::Key,
    dev::Service as _,
    dev::{ServiceRequest, ServiceResponse},
    error::InternalError,
    middleware::{Logger, from_fn},
    web::{self},
};
use clap::Parser as _;
use desk_signal::{
    controller::{
        connection::list_connections,
        device_code::{
            batch_delete_device_codes, create_device_code, delete_device_code, list_device_codes,
            update_device_code,
        },
        files::{delete_file, list_files},
        signaling::open_signaling_handle,
        terminal::{list_terminal, open_terminal_session},
    },
    model::SharedConnectionMap,
};
use desk_turn::service::startup_turn_server;
use desk_utils::{error::DeskErrorCode, network::check_ipv6_available, rest::RestResponse};
use error::DeskError;
use log::{error, info, warn};
use model::settings::{Args, Settings, SharedSettings, StartupMode};
use tracing_appender::non_blocking::WorkerGuard;

use utoipa::OpenApi;
use utoipa_actix_web::AppExt;
use utoipa_rapidoc::RapiDoc;
use utoipa_redoc::{Redoc, Servable as _};
use utoipa_scalar::{Scalar, Servable as _};
use utoipa_swagger_ui::SwaggerUi;

rust_i18n::i18n!("locales");

/// Shared override for the `is_admin` field in `/api/server_info` and `/api/desk/sysinfo`.
/// In ServiceDaemon mode the process runs as SYSTEM so `is_admin()` is always true.
/// The Tauri shell reports its own admin status via the IPC WebSocket; that value is stored
/// here and read by the info controllers so the frontend receives the correct elevation status.
/// `None` = use platform `is_admin()` directly (portable / non-daemon modes).
pub type TauriIsAdminOverride = std::sync::Arc<std::sync::Mutex<Option<bool>>>;

/// Parameters for registering the core HTTP API routes.
/// All fields are `Clone` so this struct can be captured by `HttpServer::new` closures.
#[derive(Clone)]
pub struct ApiRouteConfig {
    pub settings: web::Data<SharedSettings>,
    pub tauri_login_token: web::Data<Option<TauriLoginToken>>,
    pub connection_map: web::Data<desk_signal::model::SharedConnectionMap>,
    /// Optional unified host-control hub. Wired into the security-approval submit
    /// endpoint and (in Aggregator mode) the host-control ws routes.
    pub host_control_hub: web::Data<Option<Arc<host_control::HostControlHub>>>,
    pub tauri_is_admin: Option<web::Data<TauriIsAdminOverride>>,
}

/// Register the core API routes onto `cfg` using plain actix-web (no utoipa).
/// Used by the ServiceDaemon local API. The embedded portable server keeps its
/// own utoipa-wrapped registration for OpenAPI doc generation in
/// `run_with_hub` — keep both registrations in sync when adding endpoints.
///
/// Excluded in all modes: signaling WS, TURN management.
/// (Signaling WS is registered separately by the caller before this fn;
/// TURN endpoints stay in the embedded portable server only.)
pub fn configure_api_routes(cfg: &mut web::ServiceConfig, config: ApiRouteConfig) {
    use crate::controller::{
        info::{query_backend_info, query_server_info, query_sysinfo},
        init::init_system,
        login::{change_password, get_captcha, login_account, login_tauri, logout_account},
        service_mgmt::{install_service, uninstall_service},
        settings::{
            ack_security_approval, query_ai_model_settings, query_log_settings,
            query_security_settings, query_settings, query_telemetry_status,
            query_turn_client_settings, query_turn_settings, regenerate_turn_secret,
            submit_security_approval, update_ai_model_settings, update_log_settings,
            update_security_settings, update_settings, update_telemetry_consent,
            update_turn_client_settings, update_turn_settings,
        },
        user::{get_current_user, reject_anonymous_users},
        virtual_display::{
            install_driver as install_virtual_display_driver, query_driver_status,
            query_virtual_display_settings, uninstall_driver as uninstall_virtual_display_driver,
            update_virtual_display_settings,
        },
    };
    use desk_signal::controller::{
        connection::list_connections,
        device_code::{
            batch_delete_device_codes, create_device_code, delete_device_code, list_device_codes,
            update_device_code,
        },
        terminal::{list_terminal, open_terminal_session},
    };
    use desk_signal_facade::controller::files::{delete_file, list_files};

    let ApiRouteConfig {
        settings,
        tauri_login_token,
        connection_map,
        host_control_hub,
        tauri_is_admin,
    } = config;

    cfg.app_data(settings)
        .app_data(tauri_login_token)
        .app_data(connection_map)
        .app_data(host_control_hub)
        .app_data(
            web::JsonConfig::default()
                .limit((4096 * 1024) << 2)
                .error_handler(|err, req| {
                    warn!("request {} json error: {}", req.path(), err);
                    let msg = err.to_string();
                    InternalError::from_response(
                        err,
                        HttpResponse::BadRequest().json(desk_utils::rest::RestResponse::failed(
                            desk_utils::error::DeskErrorCode::SYSTEM_ERROR,
                            msg,
                        )),
                    )
                    .into()
                }),
        )
        .service(login_account)
        .service(login_tauri)
        .service(logout_account)
        .service(get_current_user)
        .service(get_captcha)
        .service(query_server_info)
        .service(install_service)
        .service(uninstall_service)
        .service(init_system)
        .service(
            web::scope("/api")
                .wrap(actix_web::middleware::from_fn(reject_anonymous_users))
                .service(query_driver_status)
                .service(install_virtual_display_driver)
                .service(uninstall_virtual_display_driver)
                .service(
                    web::scope("/desk")
                        .service(change_password)
                        .service(query_settings)
                        .service(update_settings)
                        .service(query_ai_model_settings)
                        .service(update_ai_model_settings)
                        .service(query_turn_settings)
                        .service(update_turn_settings)
                        .service(query_turn_client_settings)
                        .service(update_turn_client_settings)
                        .service(query_log_settings)
                        .service(update_log_settings)
                        .service(query_security_settings)
                        .service(update_security_settings)
                        .service(submit_security_approval)
                        .service(ack_security_approval)
                        .service(regenerate_turn_secret)
                        .service(query_telemetry_status)
                        .service(update_telemetry_consent)
                        .service(list_connections)
                        .service(query_sysinfo)
                        .service(query_backend_info)
                        .service(query_virtual_display_settings)
                        .service(update_virtual_display_settings)
                        // Remote terminal + file management. The browser
                        // hits these on the daemon's 8082 port; the
                        // controllers go through the local
                        // `connection_map` (populated by the daemon's own
                        // `signaling_proxy` self-loop client) and the
                        // request rides typed `ServiceToWorker::*` IPC
                        // to the user-session worker. Mirrors the
                        // utoipa registration in `run_with_hub` so
                        // portable + ServiceDaemon expose the same
                        // surface.
                        .service(list_terminal)
                        .service(open_terminal_session)
                        .service(list_files)
                        .service(delete_file)
                        // Device-code admin (CRUD over `/device_codes`).
                        // Safe in ServiceDaemon mode because
                        // `daemon::run_service_daemon_inner` initialises
                        // the same Sea-ORM signal database the portable
                        // server uses (see `desk_signal::db::init_db`),
                        // and the handlers only touch that DB plus the
                        // already-registered `SharedConnectionMap` for
                        // online-state lookups. No additional state is
                        // required.
                        .service(list_device_codes)
                        .service(create_device_code)
                        .service(update_device_code)
                        .service(delete_device_code)
                        .service(batch_delete_device_codes),
                ),
        )
        .configure(move |inner| {
            if let Some(ref override_data) = tauri_is_admin {
                inner.app_data(override_data.clone());
            }
        });
}

/// Service management operations that can be requested by the embedded HTTP
/// server and fulfilled by the Tauri host (which has UAC elevation ability).
///
/// `install_idd_driver` rides along on the `Install` variant: when `true`,
/// the elevated sidecar will stage the LcxlVirtualDisplay IDD driver as
/// part of the install. The flag is intentionally part of the wire-level
/// command (rather than a separate REST op) so the user only has to
/// approve the UAC prompt once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceOp {
    Install {
        install_path: String,
        install_idd_driver: bool,
    },
    Uninstall,
}

use std::sync::Mutex;

/// One-time token for Tauri auto-login.
/// The token is consumed after the first successful use.
///
/// The inner Mutex is Arc-wrapped so clones share the same state.
/// This allows the daemon IPC bridge and the HTTP server to share one token
/// instance and keep it in sync across Tauri reconnects.
#[derive(Clone)]
pub struct TauriLoginToken(Arc<Mutex<Option<String>>>);

impl TauriLoginToken {
    pub fn new(token: String) -> Self {
        TauriLoginToken(Arc::new(Mutex::new(Some(token))))
    }

    /// Construct in the empty (unset) state. Used when the Host Control Hub will
    /// later push a fresh token over `/ws/tauri_ipc` on every Tauri reconnect.
    pub fn empty() -> Self {
        TauriLoginToken(Arc::new(Mutex::new(None)))
    }

    /// Verify and consume the token. Returns true only once for the correct token.
    pub fn verify_and_consume(&self, candidate: &str) -> bool {
        let mut guard = self.0.lock().unwrap();
        if let Some(ref stored) = *guard {
            // Constant-time comparison to prevent timing attacks
            if constant_time_eq(stored.as_bytes(), candidate.as_bytes()) {
                *guard = None; // Consume: one-time use
                return true;
            }
        }
        false
    }

    /// Replace the stored token (used by daemon IPC bridge on Tauri reconnect).
    pub fn refresh(&self, token: String) {
        *self.0.lock().unwrap() = Some(token);
    }
}

/// Constant-time byte comparison
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Returned alongside the running [`Server`]. The caller MUST keep this guard
/// alive until `server.await` completes — dropping it earlier shuts down the
/// `tracing_appender` non-blocking writer thread, after which every log line
/// the running server emits is silently discarded.
pub type ServerHandle = (Server, Option<WorkerGuard>);

pub async fn run() -> Result<ServerHandle, DeskError> {
    let args = Args::parse();
    let settings = Settings::new(&args)?;
    run_with_hub(&settings, None).await
}

/// Run the embedded server with an optional caller-supplied
/// [`host_control::HostControlHub`].
///
/// Pass `Some(hub)` from the Tauri portable shell so business code, the embedded
/// `/ws/tauri_ipc` endpoint, and the Tauri ws client all share a single hub
/// instance. Pass `None` for headless / desk-server / signaling modes — a
/// `Local` hub is constructed internally and approval prompts deny-fast when no
/// Tauri shell is connected.
pub async fn run_with_hub(
    settings: &Settings,
    host_control_hub: Option<Arc<host_control::HostControlHub>>,
) -> Result<ServerHandle, DeskError> {
    // Create a lock file to prevent multiple instances of the server from running simultaneously.
    let lock_file_path = env::temp_dir().join("lcxl_remote_desk_server.lock");
    let lock_file = File::create(lock_file_path)?;
    if let Err(TryLockError::WouldBlock) = lock_file.try_lock() {
        error!("Failed to lock file, is another instance running?");
        return Err(DeskError::from(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "Failed to lock file, is another instance running?",
        )));
    }

    // Install default crypto provider
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    if settings.log.traceback {
        // Set RUST_BACKTRACE environment variable to 1 to enable backtraces for errors. This is useful for debugging.
        unsafe { env::set_var("RUST_BACKTRACE", "1") };
    }
    // Initialize settings
    let shared_settings = Arc::new(SharedSettings::from(settings.clone()));

    // determine startup mode
    let startup_mode = settings.args.startup_mode.clone();

    // Initialize telemetry. The returned `WorkerGuard` is propagated back to
    // the caller through `ServerHandle` so it lives as long as the running
    // server. Holding it inside this function would drop it on `Ok(server)`,
    // which closes the non-blocking writer thread before any request-time
    // logs make it to disk.
    let telemetry_guard = telemetry::init_telemetry(shared_settings.clone(), &startup_mode).await?;

    // init desk_signal db
    if startup_mode == StartupMode::Default || startup_mode == StartupMode::Signaling {
        let settings_dir = Path::new(&settings.args.config_file_path)
            .parent()
            .unwrap_or(Path::new("."))
            .to_string_lossy()
            .to_string();

        desk_signal::db::init_db(&settings_dir).await?;
    }

    info!("Server settings: {:?}", settings);
    // Get server execution file path
    let exec_file_path = env::current_exe()?;
    info!("Server execution file path: {:?}", exec_file_path);

    // Create a path to the static files directory, which is assumed to be in the same directory as the executable.
    let mut static_file_path = exec_file_path.clone();
    static_file_path.pop();
    static_file_path.push("static");
    info!("Server static file path: {:?}", static_file_path);
    let secret_key = Key::generate();
    let shared_settings_data = web::Data::from(shared_settings.clone());

    // The TauriLoginToken is shared between the HTTP `/login_tauri` route (which
    // verifies + consumes) and the host-control `/ws/tauri_ipc` endpoint (which
    // refreshes the token on every Tauri reconnect). Cloning the struct shares
    // the inner `Arc<Mutex<>>`. We always start empty — the hub endpoint pushes
    // a fresh token via the ws Ready first frame on every Tauri reconnect.
    let shared_tauri_login_token: TauriLoginToken = TauriLoginToken::empty();
    let tauri_login_token: web::Data<Option<TauriLoginToken>> =
        web::Data::new(Some(shared_tauri_login_token.clone()));

    // Build host-control endpoint state once, so portable + daemon can mount
    // identical routes. The state is registered as actix Data inside
    // `register_routes` for both the inline portable App below and any
    // future caller of `configure_api_routes`.
    let host_control_endpoint_state: Option<Arc<host_control::endpoint::EndpointState>> =
        if let Some(hub) = host_control_hub.clone() {
            let ipc_token = shared_settings_data
                .read()
                .await
                .system
                .tauri_ipc_token
                .clone()
                .unwrap_or_default();
            if ipc_token.is_empty() {
                warn!("tauri_ipc_token is empty; /ws/tauri_ipc will reject all connections");
            }
            Some(Arc::new(host_control::endpoint::EndpointState::new(
                hub,
                ipc_token,
                shared_tauri_login_token.clone(),
            )))
        } else {
            None
        };

    let connection_map = web::Data::new(SharedConnectionMap::from(BTreeMap::new()));

    //start turn server if mode is Default or Signaling
    let turn_api_state =
        if startup_mode == StartupMode::Default || startup_mode == StartupMode::Signaling {
            log::info!("Starting turn server");
            let turn_settings = {
                let settings = shared_settings.read().await;
                settings.turn.clone()
            };

            let auth_handler = Arc::new(TurnAuthHandler::new(
                turn_settings.clone(),
                connection_map.clone(),
            ));
            match startup_turn_server(turn_settings, auth_handler).await {
                Ok(s) => Some(web::Data::from(s)),
                Err(e) => {
                    error!("Failed to start turn server: {}", e);
                    None
                }
            }
        } else {
            None
        };

    // For Default / DeskServer modes that don't yet have a hub injected, fall back
    // to a Local hub so business code never sees a None. Approvals deny-fast when
    // no Tauri shell is connected (intended fallback for headless DeskServer).
    let host_control_hub_arc: Arc<host_control::HostControlHub> = match host_control_hub.clone() {
        Some(h) => h,
        None => Arc::new(host_control::HostControlHub::new_local()),
    };
    let host_control_hub_data: web::Data<Option<Arc<host_control::HostControlHub>>> =
        web::Data::new(Some(host_control_hub_arc.clone()));

    // If this instance runs signaling, ensure local_signaling_token is generated and persisted
    if startup_mode == StartupMode::Default || startup_mode == StartupMode::Signaling {
        let mut s = shared_settings_data.write().await;
        if s.system.local_signaling_token.is_none() {
            let token = uuid::Uuid::new_v4().to_string();
            info!("Generated new local_signaling_token: {}", token);
            s.system.local_signaling_token = Some(token);
            if let Err(e) = s.save() {
                error!("Failed to save local_signaling_token: {}", e);
            }
        }
    }

    let validator: Arc<dyn desk_signal_facade::service::NodeTokenValidator> =
        Arc::new(crate::service::signaling::LocalNodeTokenValidator {
            settings: shared_settings_data.clone(),
        });
    let validator_data = web::Data::new(validator);

    // Start the desk pipeline.
    //
    // Both **Default (portable)** and **DeskServer (headless)** route
    // through the in-process daemon-worker pipeline so the WebRTC
    // PeerConnection lives in the daemon-side code path identical to
    // ServiceDaemon mode. The signaling proxy's `local_handle` skips
    // itself in DeskServer mode (only Default + ServiceDaemon expose a
    // local signaling endpoint), so DeskServer naturally runs only the
    // remote signaling + remote manager WS clients — matching the
    // headless "connect to remote signal server" role.
    match startup_mode {
        StartupMode::Default | StartupMode::DeskServer => {
            info!("Starting desk pipeline (in-process daemon, mode={startup_mode:?})");
            let settings_clone = shared_settings_data.clone();
            let session_hub = host_control_hub_arc.clone();
            let args_clone = settings.args.clone();
            actix_web::rt::spawn(async move {
                if let Err(e) =
                    daemon::start_inprocess_daemon(args_clone, settings_clone, session_hub).await
                {
                    error!("In-process daemon failed to start: {e}");
                }
            });
        }
        _ => {}
    }

    // Start the Actix web server
    let mut http_server = HttpServer::new(move || {
        let default_static_file_path = static_file_path.clone();

        let turn_api_state = turn_api_state.clone();
        let startup_mode = startup_mode.clone();
        let tauri_login_token = tauri_login_token.clone();
        let host_control_hub_data = host_control_hub_data.clone();
        let validator_data = validator_data.clone();
        let host_control_endpoint_state = host_control_endpoint_state.clone();
        App::new()
            .into_utoipa_app()
            .map(|app| app.wrap(Logger::default()))
            .map(|app| {
                app.wrap_fn(|req, srv| {
                    let method = req.method().clone();
                    let uri = req.uri().to_string();
                    let peer = req.connection_info().realip_remote_addr().map(str::to_owned);
                    let fut = srv.call(req);

                    async move {
                        let res = fut.await;
                        match &res {
                            Err(err) => {
                                error!(
                                    "HTTP request failed: method={}, uri={}, peer={:?}, error={}, debug={:?}",
                                    method, uri, peer, err, err
                                );
                            }
                            Ok(resp) if resp.status().is_server_error() => {
                                error!(
                                    "HTTP request returned server error: method={}, uri={}, peer={:?}, status={}",
                                    method,
                                    uri,
                                    peer,
                                    resp.status()
                                );
                            }
                            _ => {}
                        }
                        res
                    }
                })
            })
            .app_data(shared_settings_data.clone())
            .app_data(tauri_login_token.clone())
            .app_data(connection_map.clone())
            .app_data(host_control_hub_data.clone())
            .app_data(validator_data.clone())
            .configure(|cfg| {
                if let Some(turn_api_state) = &turn_api_state {
                    cfg.app_data(turn_api_state.clone());
                }
            })
            .app_data(
                web::JsonConfig::default()
                    .limit((4096 * 1024) << 2)
                    .error_handler(|err, req| {
                        // <- create custom error response
                        warn!("progress request {} err: {}", req.path(), err);
                        let err_message = err.to_string();
                        InternalError::from_response(
                            err,
                            HttpResponse::BadRequest().json(RestResponse::failed(
                                DeskErrorCode::SYSTEM_ERROR,
                                err_message,
                            )),
                        )
                        .into()
                    }),
            ) // <- limit size of the payload (global configuration)
            // no need to login for these routes
            .service(login_account)
            .service(login_tauri)
            .service(logout_account)
            .service(get_current_user)
            .service(get_captcha)
            .service(query_server_info)
            .service(install_service)
            .service(uninstall_service)
            .service(init_system)
            .configure({
                let startup_mode = startup_mode.clone();
                move |cfg| {
                    if startup_mode == StartupMode::Default
                        || startup_mode == StartupMode::Signaling
                    {
                        log::info!("Registering signaling route at /api/desk/signaling");
                        cfg.service(open_signaling_handle);
                    }
                }
            })
            .configure(|cfg| {
                if let Some(state) = host_control_endpoint_state.clone() {
                    log::info!(
                        "Registering host control routes (mode={:?})",
                        state.hub.mode()
                    );
                    // The portable App is built on `utoipa_actix_web`, whose
                    // `ServiceConfig` is a distinct type from
                    // `actix_web::web::ServiceConfig`, so we can't delegate
                    // to `host_control::endpoint::register_routes` here.
                    // Mirror its body verbatim instead — `Data::from(Arc<T>)`
                    // yields `Data<T>`, matching the handler signature
                    // `state: web::Data<EndpointState>`.
                    cfg.app_data(web::Data::from(state))
                        .route(
                            "/ws/tauri_ipc",
                            web::get().to(host_control::endpoint::ws_handler),
                        )
                        .route(
                            "/ws/host_upstream",
                            web::get().to(host_control::endpoint::ws_upstream_handler),
                        );
                }
            })
            // TODO need to login for these routes
            .service(
                // need to login for these routes
                utoipa_actix_web::scope("/api")
                    .wrap(from_fn(reject_anonymous_users))
                    .service(query_virtual_display_driver_status)
                    .service(install_virtual_display_driver)
                    .service(uninstall_virtual_display_driver)
                    .service(
                        utoipa_actix_web::scope("/desk")
                            .service(change_password)
                            .service(query_settings)
                            .service(update_settings)
                            .service(query_ai_model_settings)
                            .service(update_ai_model_settings)
                            .service(query_turn_settings)
                            .service(update_turn_settings)
                            .service(query_turn_client_settings)
                            .service(update_turn_client_settings)
                            .service(query_log_settings)
                            .service(update_log_settings)
                            .service(query_security_settings)
                            .service(update_security_settings)
                            .service(submit_security_approval)
                            .service(ack_security_approval)
                            .service(regenerate_turn_secret)
                            .service(query_telemetry_status)
                            .service(update_telemetry_consent)
                            .service(list_connections)
                            .service(list_terminal)
                            .service(open_terminal_session)
                            .service(query_sysinfo)
                            .service(query_backend_info)
                            .service(query_virtual_display_settings)
                            .service(update_virtual_display_settings)
                            .configure({
                                let startup_mode = startup_mode.clone();
                                move |cfg| {
                                    if startup_mode == StartupMode::Default
                                        || startup_mode == StartupMode::Signaling
                                    {
                                        cfg.service(delete_file)
                                            .service(list_files)
                                            .service(create_device_code)
                                            .service(list_device_codes)
                                            .service(update_device_code)
                                            .service(delete_device_code)
                                            .service(batch_delete_device_codes);
                                    }
                                }
                            }),
                    )
                    .configure(|cfg| {
                        if turn_api_state.is_some() {
                            cfg.service(
                                utoipa_actix_web::scope("/turn")
                                    .service(get_turn_info)
                                    .service(get_turn_session)
                                    .service(get_turn_session_statistics)
                                    .service(delete_turn_session)
                                    .service(get_turn_metrics),
                            );
                        }
                    }),
            )
            .openapi_service(|mut api| {
                api.merge(openapi::ExtraSchemas::openapi());
                SwaggerUi::new("/swagger-ui/{_:.*}").url("/openapi.json", api)
            })
            .openapi_service(|api| Redoc::with_url("/redoc", api))
            .openapi_service(|api| RapiDoc::with_url("/rapidoc", "/openapi.json", api))
            .openapi_service(|api| Scalar::with_url("/scalar", api))
            .into_app()
            .wrap(
                SessionMiddleware::builder(CookieSessionStore::default(), secret_key.clone())
                    .cookie_secure(false)
                    .build(),
            )
            .service(
                actix_files::Files::new("/", static_file_path.clone())
                    .index_file("index.html")
                    .default_handler(fn_service(move |req: ServiceRequest| {
                        // support html5 history mode
                        let (http_req, _payload) = req.into_parts();
                        let path = default_static_file_path.clone().join("index.html");
                        log::debug!(
                            "Default handler hit for path: {}, serving index.html",
                            http_req.path()
                        );
                        async {
                            let response =
                                actix_files::NamedFile::open(path)?.into_response(&http_req);
                            Ok(ServiceResponse::new(http_req, response))
                        }
                    })),
            )
    });
    if settings.system.enable_ipv6 && check_ipv6_available() {
        let addr = format!(
            "{}:{}",
            settings.system.listen_addr_ipv6, settings.system.port
        );
        http_server = http_server.bind(addr.as_str())?;
        info!("Server started at http://{}", addr);
    } else {
        http_server = http_server.bind((
            settings.system.listen_addr_ipv4.as_str(),
            settings.system.port,
        ))?;
        info!(
            "Server started at http://{}:{}",
            settings.system.listen_addr_ipv4, settings.system.port
        );
    }
    if embedded_should_disable_signals(host_control_hub.is_some()) {
        // The Tauri shell owns the process lifecycle. Letting actix trap SIGINT
        // would stop only the HTTP server while the GUI keeps running, so the
        // process never exits on Ctrl+C — see `embedded_should_disable_signals`.
        http_server = http_server.disable_signals();
    }
    let server = http_server.run();
    Ok((server, telemetry_guard))
}

/// Whether the embedded HTTP server should suppress actix's default
/// SIGINT/SIGTERM handling.
///
/// True only when a shared hub is supplied, i.e. the server is embedded in the
/// Tauri shell, which owns the process lifecycle: if actix traps SIGINT there it
/// gracefully stops only the HTTP server while the GUI event loop and the IPC
/// reconnect loop keep running, so Ctrl+C never terminates the app. Standalone /
/// headless runs (no shared hub) keep actix's graceful shutdown.
fn embedded_should_disable_signals(has_shared_hub: bool) -> bool {
    has_shared_hub
}

#[cfg(test)]
mod tests {
    use super::*;

    // Constant-time eq is correct on equal slices.
    #[test]
    fn constant_time_eq_equal() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    // Embedded (Tauri) runs pass a shared hub and must suppress actix signal
    // handling so Ctrl+C terminates the whole process.
    #[test]
    fn embedded_disables_signals_when_hub_present() {
        assert!(embedded_should_disable_signals(true));
    }

    // Standalone / headless runs keep actix's graceful SIGINT shutdown.
    #[test]
    fn standalone_keeps_signals_without_hub() {
        assert!(!embedded_should_disable_signals(false));
    }

    #[test]
    fn constant_time_eq_unequal() {
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hellos")); // length mismatch
    }

    // TauriLoginToken::empty constructs in the unset state — verify always
    // fails until a refresh sets a token.
    #[test]
    fn tauri_login_token_empty_never_validates() {
        let token = TauriLoginToken::empty();
        assert!(!token.verify_and_consume(""));
        assert!(!token.verify_and_consume("anything"));
    }

    // After refresh, the empty token validates exactly once with the new value.
    #[test]
    fn tauri_login_token_empty_refresh_then_consume() {
        let token = TauriLoginToken::empty();
        token.refresh("new-tok".to_string());
        assert!(!token.verify_and_consume("wrong"));
        assert!(token.verify_and_consume("new-tok"));
        // Already consumed.
        assert!(!token.verify_and_consume("new-tok"));
    }

    // The shared inner Arc<Mutex> means cloning the struct gives a view onto
    // the same state — required for the HTTP route + ws endpoint to stay in sync.
    #[test]
    fn tauri_login_token_clone_shares_state() {
        let a = TauriLoginToken::empty();
        let b = a.clone();
        a.refresh("via-a".to_string());
        assert!(b.verify_and_consume("via-a"));
        // After consuming via b, a sees the consumed (empty) state too.
        assert!(!a.verify_and_consume("via-a"));
    }

    // Lock down the `ServerHandle` contract: the second tuple slot MUST be
    // `Option<WorkerGuard>` so callers can keep the `tracing_appender`
    // non-blocking writer alive for the entire `server.await` lifetime. The
    // earlier shape was `Result<Server, _>`, which let `run_with_hub` drop
    // its locally-bound guard at function return — closing the writer thread
    // and silently discarding every subsequent log line emitted by the
    // running server. Regressing the type (e.g. removing the guard slot or
    // returning a bare `Server`) breaks this assignment at compile time.
    #[test]
    fn server_handle_alias_carries_telemetry_worker_guard() {
        fn _shape_check(handle: ServerHandle) -> (Server, Option<WorkerGuard>) {
            handle
        }
        let _coerce: fn(ServerHandle) -> (Server, Option<WorkerGuard>) = _shape_check;
    }

    /// Regression: the four browser-facing endpoints for the remote terminal
    /// and remote file management features must be registered by
    /// `configure_api_routes` so the daemon's 8082 HTTP App exposes them. An
    /// earlier revision intentionally excluded these in ServiceDaemon mode,
    /// which left users with `404` from
    /// `GET /api/desk/file/list?connection_id=...` and an empty shell list
    /// from `GET /api/desk/terminals/{id}` even though the typed-IPC chain
    /// to the worker was fully wired. We do not check authentication here —
    /// the request lacks a session cookie and the `reject_anonymous_users`
    /// middleware will return `401 Unauthorized`, which is sufficient to
    /// prove the route matched (vs the `404 Not Found` failure mode).
    #[actix_web::test]
    async fn configure_api_routes_registers_terminal_and_file_endpoints() {
        use crate::model::settings::Settings;
        use actix_web::test;
        use desk_signal::model::SharedConnectionMap;

        let settings = Arc::new(crate::model::settings::SharedSettings::from(
            Settings::default(),
        ));
        let route_config = ApiRouteConfig {
            settings: web::Data::from(settings),
            tauri_login_token: web::Data::new(None::<TauriLoginToken>),
            connection_map: web::Data::new(SharedConnectionMap::from(BTreeMap::new())),
            host_control_hub: web::Data::new(None::<Arc<host_control::HostControlHub>>),
            tauri_is_admin: None,
        };

        let secret_key = Key::generate();
        let app = test::init_service(
            App::new()
                .wrap(
                    SessionMiddleware::builder(CookieSessionStore::default(), secret_key)
                        .cookie_secure(false)
                        .build(),
                )
                .configure(move |cfg| configure_api_routes(cfg, route_config.clone())),
        )
        .await;

        // The four routes that ServiceDaemon mode used to drop. Each one
        // should at least *match* — `reject_anonymous_users` middleware
        // will then return `Err(ErrorUnauthorized)` because no session
        // cookie is present, but emphatically NOT a `Ok(404)`. Use
        // `try_call_service` so the `Err` path doesn't panic the test —
        // an `Err` here means the route matched and the middleware ran,
        // which is exactly what we want to prove.
        let probes = [
            ("GET", "/api/desk/file/list?connection_id=test"),
            ("GET", "/api/desk/terminals/test"),
            ("GET", "/api/desk/terminal/test?command=cmd"),
            ("DELETE", "/api/desk/file"),
        ];
        for (method, uri) in probes {
            let req = match method {
                "GET" => test::TestRequest::get().uri(uri).to_request(),
                "DELETE" => test::TestRequest::delete().uri(uri).to_request(),
                _ => unreachable!(),
            };
            match test::try_call_service(&app, req).await {
                Ok(resp) => assert_ne!(
                    resp.status(),
                    actix_web::http::StatusCode::NOT_FOUND,
                    "{method} {uri} returned 404 — route must be \
                     registered by configure_api_routes (it was \
                     previously excluded in ServiceDaemon mode and \
                     broke remote terminal + file management on the \
                     daemon's 8082 port)",
                ),
                Err(_) => {
                    // Middleware-level rejection (e.g. 401 Unauthorized
                    // from `reject_anonymous_users`) means the route
                    // matched — which is the success criterion of this
                    // regression test.
                }
            }
        }
    }

    /// Regression: the five new virtual-display endpoints
    /// (`/api/virtual-display/driver/{status,install,uninstall}` and
    /// `/api/desk/settings/virtual-display` GET/POST) must be
    /// registered by `configure_api_routes` so the daemon's 8082 HTTP
    /// App exposes them. The browser hits these on the daemon side
    /// for both modes; missing them would leave the new UI broken on
    /// service-daemon installs.
    #[actix_web::test]
    async fn configure_api_routes_registers_virtual_display_endpoints() {
        use crate::model::settings::Settings;
        use actix_web::test;
        use desk_signal::model::SharedConnectionMap;

        let settings = Arc::new(crate::model::settings::SharedSettings::from(
            Settings::default(),
        ));
        let route_config = ApiRouteConfig {
            settings: web::Data::from(settings),
            tauri_login_token: web::Data::new(None::<TauriLoginToken>),
            connection_map: web::Data::new(SharedConnectionMap::from(BTreeMap::new())),
            host_control_hub: web::Data::new(None::<Arc<host_control::HostControlHub>>),
            tauri_is_admin: None,
        };

        let secret_key = Key::generate();
        let app = test::init_service(
            App::new()
                .wrap(
                    SessionMiddleware::builder(CookieSessionStore::default(), secret_key)
                        .cookie_secure(false)
                        .build(),
                )
                .configure(move |cfg| configure_api_routes(cfg, route_config.clone())),
        )
        .await;

        let probes = [
            ("GET", "/api/virtual-display/driver/status"),
            ("POST", "/api/virtual-display/driver/install"),
            ("POST", "/api/virtual-display/driver/uninstall"),
            ("GET", "/api/desk/settings/virtual-display"),
            ("POST", "/api/desk/settings/virtual-display"),
        ];
        for (method, uri) in probes {
            let req = match method {
                "GET" => test::TestRequest::get().uri(uri).to_request(),
                "POST" => test::TestRequest::post().uri(uri).to_request(),
                _ => unreachable!(),
            };
            match test::try_call_service(&app, req).await {
                Ok(resp) => assert_ne!(
                    resp.status(),
                    actix_web::http::StatusCode::NOT_FOUND,
                    "{method} {uri} returned 404 — route must be \
                     registered by configure_api_routes so the daemon's \
                     8082 port exposes the new virtual-display UI",
                ),
                Err(_) => {
                    // Middleware-level rejection (401 from
                    // `reject_anonymous_users`) means the route matched —
                    // the success criterion.
                }
            }
        }
    }

    /// Regression: the AI model settings endpoint
    /// (`/api/desk/settings/ai-model` GET/POST) must be registered by
    /// `configure_api_routes` at its real mounted path — the unit tests in
    /// `controller::settings` mount the bare service at `/settings/ai-model`,
    /// so only this smoke test proves the `/api` → `/desk` → `/settings` scope
    /// nesting (and thus the daemon's 8082 surface) actually exposes it.
    #[actix_web::test]
    async fn configure_api_routes_registers_ai_model_settings_endpoint() {
        use crate::model::settings::Settings;
        use actix_web::test;
        use desk_signal::model::SharedConnectionMap;

        let settings = Arc::new(crate::model::settings::SharedSettings::from(
            Settings::default(),
        ));
        let route_config = ApiRouteConfig {
            settings: web::Data::from(settings),
            tauri_login_token: web::Data::new(None::<TauriLoginToken>),
            connection_map: web::Data::new(SharedConnectionMap::from(BTreeMap::new())),
            host_control_hub: web::Data::new(None::<Arc<host_control::HostControlHub>>),
            tauri_is_admin: None,
        };

        let secret_key = Key::generate();
        let app = test::init_service(
            App::new()
                .wrap(
                    SessionMiddleware::builder(CookieSessionStore::default(), secret_key)
                        .cookie_secure(false)
                        .build(),
                )
                .configure(move |cfg| configure_api_routes(cfg, route_config.clone())),
        )
        .await;

        let probes = [
            ("GET", "/api/desk/settings/ai-model"),
            ("POST", "/api/desk/settings/ai-model"),
        ];
        for (method, uri) in probes {
            let req = match method {
                "GET" => test::TestRequest::get().uri(uri).to_request(),
                "POST" => test::TestRequest::post().uri(uri).to_request(),
                _ => unreachable!(),
            };
            match test::try_call_service(&app, req).await {
                Ok(resp) => assert_ne!(
                    resp.status(),
                    actix_web::http::StatusCode::NOT_FOUND,
                    "{method} {uri} returned 404 — the AI model settings route must \
                     be registered by configure_api_routes at the /api/desk/settings \
                     scope so the daemon's 8082 port exposes it",
                ),
                Err(_) => {
                    // A middleware-level rejection (401 from
                    // `reject_anonymous_users`) means the route matched — the
                    // success criterion, same as the sibling smoke tests.
                }
            }
        }
    }

    /// Regression: the five device-code admin endpoints
    /// (`/api/desk/device_codes` CRUD + `/api/desk/device_codes/batch_delete`)
    /// must be registered by `configure_api_routes` so the daemon's 8082
    /// HTTP App exposes them on the manager UI. Earlier revisions
    /// intentionally restricted device-code admin to portable
    /// (`Default | Signaling`) modes, leaving daemon installations with
    /// `404` whenever an operator opened the device-code page. The
    /// daemon does initialise the same Sea-ORM signal database as the
    /// portable server (see `desk_signal::db::init_db` invocation in
    /// `daemon::run_service_daemon_inner`), so re-using the same
    /// handlers is safe. We do not authenticate here — the request
    /// lacks a session cookie and `reject_anonymous_users` will reject
    /// before the handler runs (and before any `get_db()` call), which
    /// is sufficient to prove the route matched (vs a `404 Not Found`).
    #[actix_web::test]
    async fn configure_api_routes_registers_device_code_endpoints() {
        use crate::model::settings::Settings;
        use actix_web::test;
        use desk_signal::model::SharedConnectionMap;

        let settings = Arc::new(crate::model::settings::SharedSettings::from(
            Settings::default(),
        ));
        let route_config = ApiRouteConfig {
            settings: web::Data::from(settings),
            tauri_login_token: web::Data::new(None::<TauriLoginToken>),
            connection_map: web::Data::new(SharedConnectionMap::from(BTreeMap::new())),
            host_control_hub: web::Data::new(None::<Arc<host_control::HostControlHub>>),
            tauri_is_admin: None,
        };

        let secret_key = Key::generate();
        let app = test::init_service(
            App::new()
                .wrap(
                    SessionMiddleware::builder(CookieSessionStore::default(), secret_key)
                        .cookie_secure(false)
                        .build(),
                )
                .configure(move |cfg| configure_api_routes(cfg, route_config.clone())),
        )
        .await;

        let probes = [
            ("GET", "/api/desk/device_codes"),
            ("POST", "/api/desk/device_codes"),
            ("PUT", "/api/desk/device_codes/1"),
            ("DELETE", "/api/desk/device_codes/1"),
            ("POST", "/api/desk/device_codes/batch_delete"),
        ];
        for (method, uri) in probes {
            let req = match method {
                "GET" => test::TestRequest::get().uri(uri).to_request(),
                "POST" => test::TestRequest::post().uri(uri).to_request(),
                "PUT" => test::TestRequest::put().uri(uri).to_request(),
                "DELETE" => test::TestRequest::delete().uri(uri).to_request(),
                _ => unreachable!(),
            };
            match test::try_call_service(&app, req).await {
                Ok(resp) => assert_ne!(
                    resp.status(),
                    actix_web::http::StatusCode::NOT_FOUND,
                    "{method} {uri} returned 404 — route must be \
                     registered by configure_api_routes (it was \
                     previously restricted to portable modes and broke \
                     device-code admin on the daemon's 8082 port)",
                ),
                Err(_) => {
                    // Same convention as
                    // `configure_api_routes_registers_terminal_and_file_endpoints`:
                    // a middleware-level rejection means the route matched.
                }
            }
        }
    }
}
