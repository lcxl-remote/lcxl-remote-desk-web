//! Mounted only inside the OSS owner-authenticated management scope.

use crate::{
    error::DeskSignalError,
    web_search_config::{self, WriteError},
};
use actix_web::{HttpResponse, get, post, put, web};
use desk_signal_facade::web_search::{SearchConfigPublic, SearchConfigUpdate, SearchTestResult};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

const TAG: &str = "WebSearch";

fn db() -> Result<&'static sea_orm::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "central search configuration is unavailable",
        )
    })
}

fn failure(code: DeskErrorCode, message: &str) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<SearchConfigPublic>::failed_with_data(
            code,
            Some(message.to_owned()),
            None,
        ))
}

#[utoipa::path(tag = TAG, responses((status = 200, body = RestResponse<SearchConfigPublic>)))]
#[get("/admin/system/web-search")]
pub async fn get_web_search() -> Result<HttpResponse, DeskSignalError> {
    let config = web_search_config::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Web Search configuration is unavailable",
        )
    })?;
    Ok(HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(config.public())))
}

#[utoipa::path(tag = TAG, request_body = SearchConfigUpdate, responses((status = 200, body = RestResponse<SearchConfigPublic>)))]
#[put("/admin/system/web-search")]
pub async fn update_web_search(
    body: web::Json<SearchConfigUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match web_search_config::update(db()?, &body).await {
        Ok(config) => HttpResponse::Ok()
            .insert_header(("Cache-Control", "no-store"))
            .json(RestResponse::succeed_with_data(config.public())),
        Err(WriteError::Conflict(_)) => failure(
            DeskErrorCode::REVISION_CONFLICT,
            "Web Search configuration changed; reload before saving",
        ),
        Err(WriteError::Invalid(message)) => failure(DeskErrorCode::INVALID_PARAMS, message),
        Err(WriteError::Db(_)) => failure(
            DeskErrorCode::SYSTEM_ERROR,
            "Web Search configuration could not be saved",
        ),
    })
}

#[utoipa::path(tag = TAG, request_body = SearchConfigUpdate, responses((status = 200, body = RestResponse<SearchTestResult>)))]
#[post("/admin/system/web-search/test")]
pub async fn test_web_search(
    body: web::Json<SearchConfigUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    let current = web_search_config::read(db()?).await.map_err(|_| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Web Search configuration is unavailable",
        )
    })?;
    if current.revision != body.expected_revision {
        return Ok(failure(
            DeskErrorCode::REVISION_CONFLICT,
            "Web Search configuration changed; reload before testing",
        ));
    }
    let candidate = match current.candidate(&body) {
        Ok(value) => value,
        Err(message) => return Ok(failure(DeskErrorCode::INVALID_PARAMS, message)),
    };
    Ok(
        match desk_signal_facade::web_search::test_connection(&candidate).await {
            Ok(result) => HttpResponse::Ok()
                .insert_header(("Cache-Control", "no-store"))
                .json(RestResponse::succeed_with_data(result)),
            Err(error) => failure(DeskErrorCode::PRECONDITION_FAILED, &error.message),
        },
    )
}
