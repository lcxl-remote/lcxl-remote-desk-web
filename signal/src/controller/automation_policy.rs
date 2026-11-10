//! Mounted under the same administrator scope as the other assistant policies.
use crate::{
    automation_policy::{self, WriteError},
    error::DeskSignalError,
};
use actix_web::{HttpResponse, get, put, web};
use desk_agent_protocol::ai_assistant::automation_policy::{
    AutomationPolicy, UpdateAutomationPolicy,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central automation configuration is unavailable",
        )
    })
}

fn response(config: AutomationPolicy) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config))
}

#[utoipa::path(tag = "AutomationAdmin", responses((status = 200, body = RestResponse<AutomationPolicy>)))]
#[get("/admin/system/automation-policy")]
pub async fn get_automation_policy() -> Result<HttpResponse, DeskSignalError> {
    Ok(response(automation_policy::read(db()?).await))
}

#[utoipa::path(tag = "AutomationAdmin", request_body = UpdateAutomationPolicy, responses((status = 200, body = RestResponse<AutomationPolicy>)))]
#[put("/admin/system/automation-policy")]
pub async fn update_automation_policy(
    body: web::Json<UpdateAutomationPolicy>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match automation_policy::update(db()?, &body).await {
        Ok(config) => response(config),
        Err(error) => {
            let code = match error {
                WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
            };
            HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::<AutomationPolicy>::failed_with_data(
                    code,
                    Some(
                        "Automation configuration could not be saved; reload before retrying"
                            .into(),
                    ),
                    None,
                ))
        }
    })
}
