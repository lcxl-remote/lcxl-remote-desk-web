//! Mounted under the same owner-authenticated scope as model settings.
use crate::{
    error::DeskSignalError,
    goal_budget_policy::{self, WriteError},
};
use actix_web::{HttpResponse, get, put, web};
use desk_agent_protocol::ai_assistant::goal_budget::{GoalBudgetPolicy, UpdateGoalBudgetPolicy};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static sea_orm::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central goal budget configuration is unavailable",
        )
    })
}
fn response(config: GoalBudgetPolicy) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config))
}
#[utoipa::path(tag = "GoalBudgetAdmin", responses((status = 200, body = RestResponse<GoalBudgetPolicy>)))]
#[get("/admin/system/goal-budget-policy")]
pub async fn get_goal_budget_policy() -> Result<HttpResponse, DeskSignalError> {
    let config = goal_budget_policy::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "goal budget policy is unavailable",
        )
    })?;
    Ok(response(config))
}

#[utoipa::path(tag = "AiAssistant", responses((status = 200, body = RestResponse<GoalBudgetPolicy>)))]
#[get("/my/ai-assistant-session/goal/budget-policy")]
pub async fn get_my_goal_budget_policy() -> Result<HttpResponse, DeskSignalError> {
    let config = goal_budget_policy::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "goal budget policy is unavailable",
        )
    })?;
    Ok(response(config))
}
#[utoipa::path(tag = "GoalBudgetAdmin", request_body = UpdateGoalBudgetPolicy, responses((status = 200, body = RestResponse<GoalBudgetPolicy>)))]
#[put("/admin/system/goal-budget-policy")]
pub async fn update_goal_budget_policy(
    body: web::Json<UpdateGoalBudgetPolicy>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match goal_budget_policy::update(db()?, &body).await {
        Ok(config) => response(config),
        Err(error) => {
            let code = match error {
                WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
            };
            HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::<GoalBudgetPolicy>::failed_with_data(
                    code,
                    Some(
                        "Goal budget configuration could not be saved; reload before retrying"
                            .into(),
                    ),
                    None,
                ))
        }
    })
}
