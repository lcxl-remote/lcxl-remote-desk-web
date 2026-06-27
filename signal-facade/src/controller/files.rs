use actix_web::{HttpResponse, delete, get, web};
use desk_utils::error::DeskErrorCode;

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::files::{DeleteFileRequest, FileListParams, FileListResponse};
use crate::model::signal::SignalingType;
use crate::service::request_on_local_connection;

pub const TAG: &str = "File";

#[utoipa::path(
    tag = TAG,
    summary = "List files on remote desk",
    params(FileListParams),
    responses(
        (status = 200, description = "The list of file info", body = FileListResponse)
    ),
)]
#[get("/file/list")]
pub async fn list_files(
    query_list: web::Query<FileListParams>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let connection_id = if let Some(id) = &query_list.connection_id {
        id.clone()
    } else {
        return DeskSignalFacadeError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "connection_id is required",
        );
    };

    let not_found = format!("Connection {connection_id} not found");
    let response = request_on_local_connection(
        &connection_map,
        &connection_id,
        SignalingType::ManagerFileList,
        Some(&query_list.into_inner()),
        &not_found,
    )
    .await?;

    let file_list_response: FileListResponse = response.get_data()?;
    Ok(HttpResponse::Ok().json(file_list_response))
}

#[utoipa::path(
    tag = TAG,
    summary = "Delete a file on remote desk",
    request_body(content = DeleteFileRequest),
    responses(
        (status = 200, description = "Delete file successfully"),
        (status = 400, description = "Bad request"),
    ),
)]
#[delete("/file")]
pub async fn delete_file(
    request_json: web::Json<DeleteFileRequest>,
    connection_map: web::Data<SharedConnectionMap>,
) -> Result<HttpResponse, DeskSignalFacadeError> {
    let delete_file_request = request_json.into_inner();
    let connection_id = if let Some(id) = &delete_file_request.connection_id {
        id.clone()
    } else {
        return DeskSignalFacadeError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "connection_id is required",
        );
    };

    let not_found = format!("Connection {connection_id} not found");
    request_on_local_connection(
        &connection_map,
        &connection_id,
        SignalingType::ManagerFileDelete,
        Some(&delete_file_request),
        &not_found,
    )
    .await?;

    Ok(HttpResponse::Ok().finish())
}
