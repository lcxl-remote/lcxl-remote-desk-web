//! Mounted under the same owner-authenticated scope as model settings.
use crate::{
    access_code_protection_config::{self, WriteError},
    error::DeskSignalError,
};
use actix_web::{HttpResponse, get, put, web};
use desk_signal_facade::access_code_protection::{
    AccessCodeProtectionSettings, UpdateAccessCodeProtectionSettings,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central access-code configuration is unavailable",
        )
    })
}
fn response(config: AccessCodeProtectionSettings) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config))
}
#[utoipa::path(tag = "AccessCodeProtection", responses((status = 200, body = RestResponse<AccessCodeProtectionSettings>)))]
#[get("/admin/system/access-code-protection")]
pub async fn get_access_code_protection() -> Result<HttpResponse, DeskSignalError> {
    let config = access_code_protection_config::read(db()?)
        .await
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "access-code policy is unavailable",
            )
        })?;
    Ok(response(config))
}
#[utoipa::path(tag = "AccessCodeProtection", request_body = UpdateAccessCodeProtectionSettings, responses((status = 200, body = RestResponse<AccessCodeProtectionSettings>)))]
#[put("/admin/system/access-code-protection")]
pub async fn update_access_code_protection(
    body: web::Json<UpdateAccessCodeProtectionSettings>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match access_code_protection_config::update(db()?, &body).await {
            Ok(config) => response(config),
            Err(error) => {
                let code = match error {
                    WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                    WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                    WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
                };
                HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::<AccessCodeProtectionSettings>::failed_with_data(
                    code,
                    Some(
                        "Access-code configuration could not be saved; reload before retrying"
                            .into(),
                    ),
                    None,
                ))
            }
        },
    )
}
