//! Mounted under the same owner-authenticated scope as model settings.
use crate::{
    device_code_config::{self, WriteError},
    error::DeskSignalError,
};
use actix_web::{HttpResponse, get, put, web};
use desk_signal_facade::device_code::{DeviceCodeSettings, UpdateDeviceCodeSettings};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central device-code configuration is unavailable",
        )
    })
}
fn response(config: DeviceCodeSettings) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config))
}
#[utoipa::path(tag = "DeviceCodeSettings", responses((status = 200, body = RestResponse<DeviceCodeSettings>)))]
#[get("/admin/system/device-code-settings")]
pub async fn get_device_code_settings() -> Result<HttpResponse, DeskSignalError> {
    let config = device_code_config::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "device-code policy is unavailable",
        )
    })?;
    Ok(response(config))
}
#[utoipa::path(tag = "DeviceCodeSettings", request_body = UpdateDeviceCodeSettings, responses((status = 200, body = RestResponse<DeviceCodeSettings>)))]
#[put("/admin/system/device-code-settings")]
pub async fn update_device_code_settings(
    body: web::Json<UpdateDeviceCodeSettings>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match device_code_config::update(db()?, &body).await {
        Ok(config) => response(config),
        Err(error) => {
            let code = match error {
                WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
            };
            HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::<DeviceCodeSettings>::failed_with_data(
                    code,
                    Some(
                        "Device-code configuration could not be saved; reload before retrying"
                            .into(),
                    ),
                    None,
                ))
        }
    })
}
