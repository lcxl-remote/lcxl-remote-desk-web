use actix_web::{HttpResponse, get, http::header, web};
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::settings::SharedSettings;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BrowserExtensionPairing {
    pub bridge_url: String,
    /// Strong owner-only secret entered once in the locally installed extension.
    pub pairing_code: String,
    pub extension_version: String,
}

#[utoipa::path(
    tag = "BrowserExtension",
    summary = "Get the local Chrome extension pairing configuration",
    responses(
        (status = 200, description = "Pairing configuration", body = RestResponse<BrowserExtensionPairing>),
        (status = 500, description = "Extension bridge is not initialized"),
    ),
)]
#[get("/browser-extension/pairing")]
pub async fn get_browser_extension_pairing(
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, actix_web::Error> {
    let data_root = settings.read().await.paths().data_root().to_path_buf();
    let pairing_code =
        crate::worker::agent::browser_extension_bridge::read_pairing_token(&data_root)
            .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(BrowserExtensionPairing {
            bridge_url: format!(
                "ws://127.0.0.1:{}/browser-extension/v1",
                crate::worker::agent::browser_extension_bridge::BROWSER_EXTENSION_BRIDGE_PORT
            ),
            pairing_code,
            extension_version:
                crate::worker::agent::browser_extension_bridge::BROWSER_EXTENSION_VERSION.into(),
        })))
}
