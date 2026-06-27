use actix_web::{HttpResponse, get, post, web};
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;

use crate::error::DeskSignalError;
use crate::model_dial::configured_ssrf_mode;
use crate::model_provider::{self, ModelProviderPublic, ModelProviderUpdate};

pub const TAG: &str = "ModelProvider";

#[utoipa::path(
    tag = TAG,
    summary = "Query the masked model-provider configuration",
    responses(
        (status = 200, description = "Masked provider config (never carries the api_key)", body = RestResponse<ModelProviderPublic>),
    ),
)]
#[get("/provider")]
pub async fn get_model_provider() -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let config = model_provider::load(db).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config.public_view())))
}

#[utoipa::path(
    tag = TAG,
    summary = "Update the model-provider configuration",
    request_body = ModelProviderUpdate,
    responses(
        (status = 200, description = "Updated masked provider config", body = RestResponse<ModelProviderPublic>),
    ),
)]
#[post("/provider")]
pub async fn update_model_provider(
    body: web::Json<ModelProviderUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let mut config = model_provider::load(db).await?;
    config.apply_update(body.into_inner());
    // Write-time SSRF check: reject a base_url whose scheme or IP-literal host is
    // not permitted by the active mode. Domain hosts pass here and are re-checked
    // authoritatively at dial time by the connect-time resolver. An unset base_url
    // is allowed (the seam fails closed at dial time when it is missing).
    if let Some(base_url) = config.base_url.as_deref()
        && !base_url.trim().is_empty()
    {
        desk_utils::ssrf::check_provider_url(base_url, configured_ssrf_mode()).map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "base_url is not permitted by the server's provider policy",
            )
        })?;
    }
    model_provider::save(db, config.clone()).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config.public_view())))
}
