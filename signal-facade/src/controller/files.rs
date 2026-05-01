use actix_web::{HttpResponse, delete, get, web};
use desk_utils::error::DeskErrorCode;

use crate::error::DeskSignalFacadeError;
use crate::model::connection::SharedConnectionMap;
use crate::model::files::{DeleteFileRequest, FileListParams, FileListResponse};
use crate::model::signal::{ForwardSignalingSender, SignalingType};

#[utoipa::path(
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

    let response = {
        let connection_map = connection_map.read().await;
        if let Some(connection) = connection_map.get(&connection_id) {
            connection
                .request_peer_with_callback(
                    SignalingType::ManagerFileList,
                    Some(&query_list.into_inner()),
                    None,
                )
                .await?
        } else {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Connection {} not found", connection_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state
        && response_state.error_code != 0 {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }

    let file_list_response: FileListResponse = response.get_data()?;
    Ok(HttpResponse::Ok().json(file_list_response))
}

#[utoipa::path(
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

    let response = {
        let connection_map = connection_map.read().await;
        if let Some(connection) = connection_map.get(&connection_id) {
            connection
                .request_peer_with_callback(
                    SignalingType::ManagerFileDelete,
                    Some(&delete_file_request),
                    None,
                )
                .await?
        } else {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Connection {} not found", connection_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state
        && response_state.error_code != 0 {
            return DeskSignalFacadeError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }

    Ok(HttpResponse::Ok().finish())
}
