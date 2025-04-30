pub mod controller;
pub mod desk_error;
pub mod model;
pub mod utils;

use std::env;

use actix_server::Server;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{
    App, HttpResponse, HttpServer,
    cookie::Key,
    error,
    middleware::{Logger, from_fn},
    web::{self},
};
use clap::Parser as _;
use controller::{
    login::{change_password, get_captcha, login_account, logout_account},
    settings::query_settings,
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

    // Start the Actix web server here
    let mut http_server = HttpServer::new(move || {
        App::new()
            .into_utoipa_app()
            .map(|app| app.wrap(Logger::default()))
            .app_data(
                web::JsonConfig::default()
                    .limit(4096 * 1024 << 2)
                    .error_handler(|err, req| {
                        // <- create custom error response
                        warn!("progress request {} err: {}", req.path(), err);
                        let err_message = err.to_string();
                        return error::InternalError::from_response(
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
            .service(
                // need to login for these routes
                utoipa_actix_web::scope("/api/desk")
                    .wrap(from_fn(reject_anonymous_users))
                    .service(change_password)
                    .service(query_settings),
            )
            .openapi_service(|api| {
                SwaggerUi::new("/swagger-ui/{_:.*}").url("/api/openapi.json", api)
            })
            .openapi_service(|api| Redoc::with_url("/redoc", api))
            .openapi_service(|api| RapiDoc::with_url("/rapidoc", "/api/openapi.json", api))
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
