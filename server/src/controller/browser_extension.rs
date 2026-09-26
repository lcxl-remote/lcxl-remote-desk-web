use actix_web::{HttpRequest, HttpResponse, http::header, post, web};
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

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalPairingProof {
    pub local_proof: String,
}

#[utoipa::path(
    tag = "BrowserExtension",
    summary = "Redeem a local OS-user proof for Chrome extension pairing",
    request_body = LocalPairingProof,
    responses(
        (status = 200, description = "Pairing configuration", body = RestResponse<BrowserExtensionPairing>),
        (status = 500, description = "Extension bridge is not initialized"),
    ),
)]
#[post("/browser-extension/pairing")]
pub async fn create_browser_extension_pairing(
    request: HttpRequest,
    settings: web::Data<SharedSettings>,
    proof: web::Json<LocalPairingProof>,
) -> Result<HttpResponse, actix_web::Error> {
    super::host_readiness::validate_local_mutation(&request)?;
    let settings = settings.read().await;
    let data_root = settings.paths().data_root().to_path_buf();
    let device_id = settings
        .system
        .get_client_id()
        .map_err(actix_web::error::ErrorInternalServerError)?;
    // Resolve the response before spending the one-use OS proof. A bridge
    // initialization/storage failure must not consume a valid local attempt.
    let pairing_code =
        crate::worker::agent::browser_extension_bridge::read_pairing_token(&data_root)
            .map_err(actix_web::error::ErrorInternalServerError)?;
    let bridge_url =
        crate::worker::agent::browser_extension_bridge::pairing_endpoint(&data_root, &device_id)
            .map_err(actix_web::error::ErrorInternalServerError)?;
    crate::worker::agent::browser_extension_bridge::pairing::consume(
        &data_root,
        &proof.local_proof,
    )
    .map_err(|_| {
        actix_web::error::ErrorForbidden("A fresh local OS-user pairing proof is required")
    })?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(RestResponse::succeed_with_data(BrowserExtensionPairing {
            bridge_url,
            pairing_code,
            extension_version:
                crate::worker::agent::browser_extension_bridge::BROWSER_EXTENSION_VERSION.into(),
        })))
}
