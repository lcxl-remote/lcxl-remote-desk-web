//! Mounted under the same owner-authenticated scope as model settings.
use crate::{
    error::DeskSignalError,
    subagent_policy::{self, WriteError},
};
use actix_web::{HttpResponse, get, put, web};
use desk_agent_protocol::ai_assistant::subagent_policy::{SubAgentPolicy, UpdateSubAgentPolicy};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static sea_orm::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central subagent configuration is unavailable",
        )
    })
}
fn response(config: SubAgentPolicy) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config))
}
#[utoipa::path(tag = "SubAgentAdmin", responses((status = 200, body = RestResponse<SubAgentPolicy>)))]
#[get("/admin/system/subagent-policy")]
pub async fn get_subagent_policy() -> Result<HttpResponse, DeskSignalError> {
    let config = subagent_policy::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "subagent policy is unavailable",
        )
    })?;
    Ok(response(config))
}

#[utoipa::path(tag = "AiAssistant", responses((status = 200, body = RestResponse<SubAgentPolicy>)))]
#[get("/my/ai-assistant-session/subagent-policy")]
pub async fn get_my_subagent_policy() -> Result<HttpResponse, DeskSignalError> {
    let config = subagent_policy::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "subagent policy is unavailable",
        )
    })?;
    Ok(response(config))
}
#[utoipa::path(tag = "SubAgentAdmin", request_body = UpdateSubAgentPolicy, responses((status = 200, body = RestResponse<SubAgentPolicy>)))]
#[put("/admin/system/subagent-policy")]
pub async fn update_subagent_policy(
    body: web::Json<UpdateSubAgentPolicy>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match subagent_policy::update(db()?, &body).await {
        Ok(config) => response(config),
        Err(error) => {
            let code = match error {
                WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
            };
            HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::<SubAgentPolicy>::failed_with_data(
                    code,
                    Some(
                        "Subagent configuration could not be saved; reload before retrying".into(),
                    ),
                    None,
                ))
        }
    })
}
