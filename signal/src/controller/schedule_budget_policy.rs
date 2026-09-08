//! Mounted under the same owner-authenticated scope as model settings.
use crate::{
    error::DeskSignalError,
    schedule_budget_policy::{self, WriteError},
};
use actix_web::{HttpResponse, get, put, web};
use desk_agent_protocol::schedule::policy::{ScheduleBudgetPolicy, UpdateScheduleBudgetPolicy};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static sea_orm::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central schedule budget configuration is unavailable",
        )
    })
}
fn response(config: ScheduleBudgetPolicy) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config))
}
#[utoipa::path(tag = "ScheduleBudgetAdmin", responses((status = 200, body = RestResponse<ScheduleBudgetPolicy>)))]
#[get("/admin/system/schedule-budget-policy")]
pub async fn get_schedule_budget_policy() -> Result<HttpResponse, DeskSignalError> {
    let config = schedule_budget_policy::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "schedule budget policy is unavailable",
        )
    })?;
    Ok(response(config))
}
#[utoipa::path(tag = "ScheduleBudgetAdmin", request_body = UpdateScheduleBudgetPolicy, responses((status = 200, body = RestResponse<ScheduleBudgetPolicy>)))]
#[put("/admin/system/schedule-budget-policy")]
pub async fn update_schedule_budget_policy(
    body: web::Json<UpdateScheduleBudgetPolicy>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match schedule_budget_policy::update(db()?, &body).await {
        Ok(config) => response(config),
        Err(error) => {
            let code = match error {
                WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
            };
            HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::<ScheduleBudgetPolicy>::failed_with_data(
                    code,
                    Some(
                        "Schedule budget configuration could not be saved; reload before retrying"
                            .into(),
                    ),
                    None,
                ))
        }
    })
}
