//! Mounted under the same owner-authenticated scope as model settings.
use crate::{
    error::DeskSignalError,
    terminal_completion_config::{self, WriteError},
};
use actix_web::{HttpResponse, get, put, web};
use desk_signal_facade::terminal_completion::{
    TerminalCompletionDto, UpdateTerminalCompletionRequest,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central completion configuration is unavailable",
        )
    })
}
fn response(
    config: desk_diagnose_core::terminal_completion_policy::TerminalCompletionPolicy,
) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(TerminalCompletionDto {
            revision: config.revision,
            max_output_tokens: config.max_output_tokens,
        }))
}
#[utoipa::path(tag = "TerminalCompletionAdmin", responses((status = 200, body = RestResponse<TerminalCompletionDto>)))]
#[get("/admin/system/terminal-completion")]
pub async fn get_terminal_completion() -> Result<HttpResponse, DeskSignalError> {
    let config = terminal_completion_config::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "completion policy is unavailable",
        )
    })?;
    Ok(response(config))
}
#[utoipa::path(tag = "TerminalCompletionAdmin", request_body = UpdateTerminalCompletionRequest, responses((status = 200, body = RestResponse<TerminalCompletionDto>)))]
#[put("/admin/system/terminal-completion")]
pub async fn update_terminal_completion(
    body: web::Json<UpdateTerminalCompletionRequest>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(
        match terminal_completion_config::update(db()?, &body).await {
            Ok(config) => response(config),
            Err(error) => {
                let code = match error {
                    WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                    WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                    WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
                };
                HttpResponse::Ok()
                    .insert_header(("Cache-Control", "no-store"))
                    .json(RestResponse::<TerminalCompletionDto>::failed_with_data(
                        code,
                        Some(
                            "Completion configuration could not be saved; reload before retrying"
                                .into(),
                        ),
                        None,
                    ))
            }
        },
    )
}
