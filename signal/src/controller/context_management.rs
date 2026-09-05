//! Mounted under the same owner-authenticated scope as model settings.
use crate::{
    context_management_config::{self, WriteError},
    error::DeskSignalError,
};
use actix_web::{HttpResponse, get, put, web};
use desk_signal_facade::context_management::{
    ContextManagementDto, UpdateContextManagementRequest,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static sea_orm::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central context configuration is unavailable",
        )
    })
}
fn response(config: desk_diagnose_core::model_context::PlatformContextPolicy) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(ContextManagementDto {
            revision: config.revision,
            strategy: config.strategy.into(),
        }))
}
#[utoipa::path(tag = "ContextManagementAdmin", responses((status = 200, body = RestResponse<ContextManagementDto>)))]
#[get("/admin/system/ai-context-management")]
pub async fn get_context_management() -> Result<HttpResponse, DeskSignalError> {
    let config = context_management_config::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "context policy is unavailable",
        )
    })?;
    Ok(response(config))
}
#[utoipa::path(tag = "ContextManagementAdmin", request_body = UpdateContextManagementRequest, responses((status = 200, body = RestResponse<ContextManagementDto>)))]
#[put("/admin/system/ai-context-management")]
pub async fn update_context_management(
    body: web::Json<UpdateContextManagementRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match context_management_config::update(db()?, &body).await {
            Ok(config) => response(config),
            Err(error) => {
                let code = match error {
                    WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                    WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                    WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
                };
                HttpResponse::Ok()
                    .insert_header(("Cache-Control", "no-store"))
                    .json(RestResponse::<ContextManagementDto>::failed_with_data(
                        code,
                        Some(
                            "Context configuration could not be saved; reload before retrying"
                                .into(),
                        ),
                        None,
                    ))
            }
        },
    )
}
