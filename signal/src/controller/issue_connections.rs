//! Mounted inside the OSS owner-authenticated scope.

use crate::{
    error::DeskSignalError,
    issue_connections::{self, WriteError},
};
use actix_web::{HttpResponse, get, post, put, web};
use desk_signal_facade::mcp_client::{
    issue_connection::IssueConnectionProbe,
    settings::{IssueConnectionChoice, IssueConnectionsPublic, IssueConnectionsUpdate},
};
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

fn db() -> Result<&'static crate::config::connection::DatabaseConnection, DeskSignalError> {
    crate::db::try_get_db().ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "External connection settings are unavailable",
        )
    })
}

fn response<T: serde::Serialize + utoipa::ToSchema>(value: T) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::succeed_with_data(value))
}

fn failure(code: DeskErrorCode, message: &str) -> HttpResponse {
    HttpResponse::Ok()
        .insert_header(("Cache-Control", "no-store"))
        .json(RestResponse::<IssueConnectionsPublic>::failed_with_data(
            code,
            Some(message.into()),
            None,
        ))
}

#[utoipa::path(tag = "IssueConnections", responses((status = 200, body = RestResponse<IssueConnectionsPublic>)))]
#[get("/admin/system/issue-connections")]
pub async fn get_issue_connections() -> Result<HttpResponse, DeskSignalError> {
    Ok(response(issue_connections::read(db()?).await?.public()))
}

#[utoipa::path(tag = "IssueConnections", request_body = IssueConnectionsUpdate, responses((status = 200, body = RestResponse<IssueConnectionsPublic>)))]
#[put("/admin/system/issue-connections")]
pub async fn update_issue_connections(
    body: web::Json<IssueConnectionsUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    Ok(match issue_connections::update(db()?, &body).await {
        Ok(config) => response(config.public()),
        Err(WriteError::Conflict) => failure(
            DeskErrorCode::REVISION_CONFLICT,
            "External connection settings changed; reload before saving",
        ),
        Err(WriteError::Invalid(message)) => failure(DeskErrorCode::INVALID_PARAMS, message),
        Err(WriteError::Backend(_)) => failure(
            DeskErrorCode::SYSTEM_ERROR,
            "External connection settings could not be saved",
        ),
    })
}

#[utoipa::path(tag = "IssueConnections", params(("connection_ref" = String, Path)), responses((status = 200, body = RestResponse<IssueConnectionProbe>)))]
#[post("/admin/system/issue-connections/{connection_ref}/test")]
pub async fn test_issue_connection(
    connection_ref: web::Path<String>,
) -> Result<HttpResponse, DeskSignalError> {
    let config = issue_connections::read(db()?).await?;
    let Some(connection) = config
        .connections
        .into_iter()
        .find(|connection| connection.connection_ref == connection_ref.as_str())
    else {
        return Ok(failure(
            DeskErrorCode::PRECONDITION_FAILED,
            "External connection is unavailable",
        ));
    };
    let client = tokio::task::spawn_blocking(move || connection.client()).await;
    let result = match client {
        Ok(Ok(client)) => client.test_connection().await,
        _ => Err(desk_signal_facade::mcp_client::McpClientError::InvalidConfiguration),
    };
    Ok(match result {
        Ok(probe) => response(probe),
        Err(error) => failure(DeskErrorCode::PRECONDITION_FAILED, &error.to_string()),
    })
}

#[utoipa::path(tag = "AiAssistantSession", responses((status = 200, body = RestResponse<Vec<IssueConnectionChoice>>)))]
#[get("/my/ai-assistant-session/issue-connections")]
pub async fn get_my_issue_connections() -> Result<HttpResponse, DeskSignalError> {
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    Ok(response(
        issue_connections::read(db()?).await?.choices(&actor),
    ))
}
