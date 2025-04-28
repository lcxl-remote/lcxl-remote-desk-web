pub mod desk_error;
pub mod model;

use std::env;

use actix_server::Server;
use actix_session::{SessionMiddleware, storage::CookieSessionStore};
use actix_web::{
    App, HttpResponse, HttpServer,
    cookie::Key,
    error,
    middleware::Logger,
    web::{self},
};
use desk_error::DeskError;
use log::{info, warn};
use model::common::{ErrorCode, RestResponse};
use utoipa_actix_web::AppExt;
use utoipa_rapidoc::RapiDoc;
use utoipa_redoc::{Redoc, Servable as _};
use utoipa_scalar::{Scalar, Servable as _};
use utoipa_swagger_ui::SwaggerUi;

pub async fn run() -> Result<Server, DeskError> {
    // Get server execution file path
    let exec_file_path = env::current_exe()?;
    info!("Server execution file path: {:?}", exec_file_path);

    // Create a path to the static files directory, which is assumed to be in the same directory as the executable.
    let mut static_file_path = exec_file_path.clone();
    static_file_path.pop();
    static_file_path.push("static");
    info!("Server static file path: {:?}", static_file_path);
    let secret_key = Key::generate();
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
    Ok(http_server.run())
}
