pub mod controller;
pub mod error;
pub mod model;
pub mod openapi;
pub mod service;
pub mod telemetry;
pub mod version;

use std::{
    collections::BTreeMap,
    env,
    fs::{File, TryLockError},
    io::ErrorKind,
    path::Path,
    sync::Arc,
};

use crate::controller::{
    info::{query_backend_info, query_server_info, query_sysinfo},
    init::init_system,
    login::{change_password, get_captcha, login_account, logout_account},
    settings::{
        query_log_settings, query_security_settings, query_settings, query_telemetry_status,
        regenerate_turn_secret, submit_security_approval, update_log_settings,
        update_security_settings, update_settings, update_telemetry_consent,
    },
    turn::{
        delete_turn_session, get_turn_info, get_turn_metrics, get_turn_session,
        get_turn_session_statistics,
    },
    user::{get_current_user, reject_anonymous_users},
};
use actix_server::Server;
use actix_service::fn_service;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{
    App, HttpResponse, HttpServer,
    cookie::Key,
    dev::{ServiceRequest, ServiceResponse},
    error::InternalError,
    middleware::{Logger, from_fn},
    web::{self},
};
use clap::Parser as _;
use desk_signal::{
    controller::{
        device_code::{
            batch_delete_device_codes, create_device_code, delete_device_code, list_device_codes,
            update_device_code,
        },
        files::{delete_file, list_files},
        session::list_sessions,
        signaling::open_signaling_handle,
        terminal::{list_terminal, open_terminal_session},
    },
    model::SharedSessionMap,
};
use desk_turn::service::startup_turn_server;
use desk_utils::{error::DeskErrorCode, network::check_ipv6_available, rest::RestResponse};
use error::DeskError;
use log::{error, info, warn};
use model::settings::{Args, Settings, SharedSettings, StartupMode};
use service::signaling::start_desk_session;
use crate::model::turn::TurnAuthHandler;

use utoipa::OpenApi;
use utoipa_actix_web::AppExt;
use utoipa_rapidoc::RapiDoc;
use utoipa_redoc::{Redoc, Servable as _};
use utoipa_scalar::{Scalar, Servable as _};
use utoipa_swagger_ui::SwaggerUi;

use crate::model::host_control::{HostControlEventType, PrivateScreenCommand, WhiteboardCommand};


rust_i18n::i18n!("locales");

pub struct ExternalChannels {
    pub private_screen_cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
    pub private_screen_state_receiver:
        Option<tokio::sync::mpsc::UnboundedReceiver<HostControlEventType>>,
    /// One-time token for Tauri WebView auto-login
    pub tauri_login_token: Option<String>,
    /// Command sender for whiteboard overlay (available when Tauri is present)
    pub whiteboard_cmd_sender: Option<std::sync::mpsc::Sender<WhiteboardCommand>>,
    /// Channel for sending security approval requests to Tauri dialog
    pub security_approval_sender: Option<crate::model::security_approval::SecurityApprovalSender>,
}

use std::sync::Mutex;

/// One-time token for Tauri auto-login.
/// The token is consumed after the first successful use.
pub struct TauriLoginToken(Mutex<Option<String>>);

impl TauriLoginToken {
    pub fn new(token: String) -> Self {
        TauriLoginToken(Mutex::new(Some(token)))
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
}

/// Constant-time byte comparison
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

pub async fn run() -> Result<Server, DeskError> {
    let args = Args::parse();
    let settings = Settings::new(&args)?;
    run_with_channels(
        &settings,
        ExternalChannels {
            private_screen_cmd_sender: None,
            private_screen_state_receiver: None,
            tauri_login_token: None,
            whiteboard_cmd_sender: None,
            security_approval_sender: None,
        },
    )
    .await
}

pub async fn run_with_channels(
    settings: &Settings,
    channels: ExternalChannels,
) -> Result<Server, DeskError> {
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

    // Initialize telemetry
    let _guard = telemetry::init_telemetry(shared_settings.clone()).await?;

    // determine startup mode
    let startup_mode = settings.args.startup_mode.clone();

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

    let tauri_login_token: web::Data<Option<TauriLoginToken>> =
        web::Data::new(channels.tauri_login_token.clone().map(TauriLoginToken::new));

    //start turn server if mode is Default or Signaling
    let turn_api_state =
        if startup_mode == StartupMode::Default || startup_mode == StartupMode::Signaling {
            log::info!("Starting turn server");
            let settings = {
                let settings = shared_settings.read().await;
                settings.turn.clone()
            };
            let auth_handler = Arc::new(TurnAuthHandler::new(shared_settings_data.clone()));
            match startup_turn_server(settings, auth_handler).await {
                Ok(s) => Some(web::Data::from(s)),
                Err(e) => {
                    error!("Failed to start turn server: {}", e);
                    None
                }
            }
        } else {
            None
        };

    let security_approval_sender = web::Data::new(channels.security_approval_sender.clone());

    // start desk session if mode is Default or DeskServer
    if startup_mode == StartupMode::Default || startup_mode == StartupMode::DeskServer {
        info!("Starting desk session");
        let settings_clone = shared_settings_data.clone();
        actix_web::rt::spawn(async move {
            if let Err(e) = start_desk_session(settings_clone, channels).await {
                error!("Desk session error: {}", e);
            }
        });
    }

    let session_map = web::Data::new(SharedSessionMap::from(BTreeMap::new()));

    // Start the Actix web server
    let mut http_server = HttpServer::new(move || {
        let default_static_file_path = static_file_path.clone();

        let turn_api_state = turn_api_state.clone();
        let startup_mode = startup_mode.clone();
        let tauri_login_token = tauri_login_token.clone();
        let security_approval_sender = security_approval_sender.clone();
        App::new()
            .into_utoipa_app()
            .map(|app| app.wrap(Logger::default()))
            .app_data(shared_settings_data.clone())
            .app_data(tauri_login_token.clone())
            .app_data(session_map.clone())
            .app_data(security_approval_sender.clone())
            .configure(|cfg| {
                if let Some(turn_api_state) = &turn_api_state {
                    cfg.app_data(turn_api_state.clone());
                }
            })
            .app_data(
                web::JsonConfig::default()
                    .limit(4096 * 1024 << 2)
                    .error_handler(|err, req| {
                        // <- create custom error response
                        warn!("progress request {} err: {}", req.path(), err);
                        let err_message = err.to_string();
                        return InternalError::from_response(
                            err,
                            HttpResponse::BadRequest().json(RestResponse::failed(
                                DeskErrorCode::SYSTEM_ERROR,
                                err_message,
                            )),
                        )
                        .into();
                    }),
            ) // <- limit size of the payload (global configuration)
            // no need to login for these routes
            .service(login_account)
            .service(crate::controller::login::login_tauri)
            .service(logout_account)
            .service(get_current_user)
            .service(get_captcha)
            .service(query_server_info)
            .service(init_system)
            // TODO need to login for these routes
            .service(
                // need to login for these routes
                utoipa_actix_web::scope("/api")
                    .wrap(from_fn(reject_anonymous_users))
                    .service(
                        utoipa_actix_web::scope("/desk")
                            .service(change_password)
                            .service(query_settings)
                            .service(update_settings)
                            .service(query_log_settings)
                            .service(update_log_settings)
                            .service(query_security_settings)
                            .service(update_security_settings)
                            .service(submit_security_approval)
                            .service(regenerate_turn_secret)
                            .service(query_telemetry_status)
                            .service(update_telemetry_consent)
                            .service(list_sessions)
                            .service(list_terminal)
                            .service(open_terminal_session)
                            .service(query_sysinfo)
                            .service(query_backend_info)
                            .configure(move |cfg| {
                                if startup_mode == StartupMode::Default
                                    || startup_mode == StartupMode::Signaling
                                {
                                    log::info!("Registering signaling route at /signaling");
                                    cfg.service(open_signaling_handle)
                                        .service(delete_file)
                                        .service(list_files)
                                        .service(create_device_code)
                                        .service(list_device_codes)
                                        .service(update_device_code)
                                        .service(delete_device_code)
                                        .service(batch_delete_device_codes);
                                }
                            }),
                    )
                    .configure(|cfg| {
                        if let Some(_) = &turn_api_state {
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
    let server = http_server.run();
    Ok(server)
}
