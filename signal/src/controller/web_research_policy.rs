//! Mounted under the same administrator scope as the other assistant policies.
use crate::{
    error::DeskSignalError,
    web_research_policy::{self, WriteError},
};
use actix_web::{HttpResponse, get, put, web};
use desk_agent_protocol::ai_assistant::web_research_policy::{
    UpdateWebResearchPolicy, WebResearchPolicy,
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central web research configuration is unavailable",
        )
    })
}

fn response(config: WebResearchPolicy) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config))
}

#[utoipa::path(tag = "WebResearchAdmin", responses((status = 200, body = RestResponse<WebResearchPolicy>)))]
#[get("/admin/system/web-research-policy")]
pub async fn get_web_research_policy() -> Result<HttpResponse, DeskSignalError> {
    Ok(response(web_research_policy::read(db()?).await))
}

#[utoipa::path(tag = "WebResearchAdmin", request_body = UpdateWebResearchPolicy, responses((status = 200, body = RestResponse<WebResearchPolicy>)))]
#[put("/admin/system/web-research-policy")]
pub async fn update_web_research_policy(
    body: web::Json<UpdateWebResearchPolicy>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match web_research_policy::update(db()?, &body).await {
        Ok(config) => response(config),
        Err(error) => {
            let code = match error {
                WriteError::Conflict => DeskErrorCode::REVISION_CONFLICT,
                WriteError::Invalid => DeskErrorCode::INVALID_PARAMS,
                WriteError::Db(_) => DeskErrorCode::SYSTEM_ERROR,
            };
            HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::<WebResearchPolicy>::failed_with_data(
                    code,
                    Some(
                        "Web research configuration could not be saved; reload before retrying"
                            .into(),
                    ),
                    None,
                ))
        }
    })
}
