pub mod controller;
pub mod desk_error;
pub mod model;
pub mod service;
pub mod utils;

use std::env;

use actix_server::Server;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{
    App, HttpResponse, HttpServer,
    cookie::Key,
    error::InternalError,
    middleware::{Logger, from_fn},
    web::{self},
};
use clap::Parser as _;
use controller::{
    files::{delete_file, list_files},
    login::{change_password, get_captcha, login_account, logout_account},
    settings::{query_settings, update_settings},
    turn::{
        delete_turn_session, get_turn_info, get_turn_metrics, get_turn_session,
        get_turn_session_statistics, startup_turn_server,
    },
    user::{get_current_user, get_notices, reject_anonymous_users},
};
use desk_error::DeskError;
use log::{info, warn};
use model::{
    common::{ErrorCode, RestResponse},
    settings::{Args, Settings, SharedSettings, UserSettings},
};
use utils::network::check_ipv6_available;
use utoipa_actix_web::AppExt;
use utoipa_rapidoc::RapiDoc;
use utoipa_redoc::{Redoc, Servable as _};
use utoipa_scalar::{Scalar, Servable as _};
use utoipa_swagger_ui::SwaggerUi;
use uuid::Uuid;

pub async fn run() -> Result<Server, DeskError> {
    let args = Args::parse();
    let settings = Settings::new(&args)?;

    env_logger::init_from_env(
        env_logger::Env::new().default_filter_or(settings.system.log_level.as_str()),
    );
    info!("Server args: {:?}", args);
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
    let shared_settings = web::Data::new(SharedSettings::from(settings.clone()));

    // check user and passwd
    let passwd = {
        let settings = shared_settings.lock().await;
        settings.user.login_password.clone()
    };
    if passwd == UserSettings::default().login_password {
        warn!("Password need to change");
        let random_uuid = Uuid::new_v4();
        let uuid_str = String::from(random_uuid);
        let new_password = &uuid_str[..6];
        let mut settings = shared_settings.lock().await;
        settings.user.login_password = String::from(new_password);
        info!("New random password: {}", new_password);
        settings.save()?;
    }

    //start turn server
    let turn_api_server = web::Data::new(startup_turn_server(&settings).await?);

    // Start the Actix web server
    let mut http_server = HttpServer::new(move || {
        App::new()
            .into_utoipa_app()
            .map(|app| app.wrap(Logger::default()))
            .app_data(shared_settings.clone())
            .app_data(turn_api_server.clone())
            .app_data(
                web::JsonConfig::default()
                    .limit(4096 * 1024 << 2)
                    .error_handler(|err, req| {
                        // <- create custom error response
                        warn!("progress request {} err: {}", req.path(), err);
                        let err_message = err.to_string();
                        return InternalError::from_response(
                            err,
                            HttpResponse::BadRequest()
                                .json(RestResponse::failed(ErrorCode::SYSTEM_ERROR, err_message)),
                        )
                        .into();
                    }),
            ) // <- limit size of the payload (global configuration)
            // no need to login for these routes
            .service(login_account)
            .service(logout_account)
            .service(get_current_user)
            .service(get_notices)
            .service(get_captcha)
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
                            .service(delete_file)
                            .service(list_files),
                    )
                    .service(
                        utoipa_actix_web::scope("/turn")
                            .service(get_turn_info)
                            .service(get_turn_session)
                            .service(get_turn_session_statistics)
                            .service(delete_turn_session)
                            .service(get_turn_metrics),
                    ),
            )
            .openapi_service(|api| SwaggerUi::new("/swagger-ui/{_:.*}").url("/openapi.json", api))
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
                actix_files::Files::new("/", static_file_path.clone()).index_file("index.html"),
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
    Ok(http_server.run())
}
