use actix_web::{HttpResponse, delete, get, web};
use desk_utils::error::DeskErrorCode;

use crate::error::DeskSignalError;
use crate::model::SharedSessionMap;
use desk_signal_facade::model::files::{DeleteFileRequest, FileListParams, FileListResponse};
use desk_signal_facade::model::signal::{ForwardSignalingSender, SignalingType};

#[utoipa::path(
    summary = "List files",
    params(FileListParams),
    responses(
        (status = 200, description = "The list of file info", body=FileListResponse)
    ),
)]
#[get("/file/list")]
pub async fn list_files(
    query_list: web::Query<FileListParams>,
    session_map: web::Data<SharedSessionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let session_id = if let Some(id) = &query_list.session_id {
        id.clone()
    } else {
        return DeskSignalError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "session_id is required",
        );
    };

    let response = {
        let session_map = session_map.read().await;
        if let Some(session) = session_map.get(&session_id) {
            session
                .request_peer_with_callback(
                    SignalingType::ManagerFileList,
                    &query_list.into_inner(),
                    None,
                )
                .await?
        } else {
            return DeskSignalError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Session {} not found", session_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state {
        if response_state.error_code != 0 {
            return DeskSignalError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }
    }

    let file_list_response: FileListResponse = response.get_data()?;
    Ok(HttpResponse::Ok().json(file_list_response))
}

#[utoipa::path(
    summary = "Delete a file",
    request_body(content = DeleteFileRequest),
    responses(
        (status = 200, description = "Delete file successfully"),
        (status = 400, description = "Bad request"),
        (status = 501, description = "Not implemented"),
    ),
)]
#[delete("/file")]
pub async fn delete_file(
    requst_json: web::Json<DeleteFileRequest>,
    session_map: web::Data<SharedSessionMap>,
) -> Result<HttpResponse, DeskSignalError> {
    let delete_file_request = requst_json.into_inner();
    let session_id = if let Some(id) = &delete_file_request.session_id {
        id.clone()
    } else {
        return DeskSignalError::custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "session_id is required",
        );
    };

    let response = {
        let session_map = session_map.read().await;
        if let Some(session) = session_map.get(&session_id) {
            session
                .request_peer_with_callback(
                    SignalingType::ManagerFileDelete,
                    &delete_file_request,
                    None,
                )
                .await?
        } else {
            return DeskSignalError::custom_error(
                DeskErrorCode::REMOTE_DESK_OFFLINE,
                &format!("Session {} not found", session_id),
            );
        }
    };

    if let Some(ref response_state) = response.response_state {
        if response_state.error_code != 0 {
            return DeskSignalError::custom_error(
                DeskErrorCode::new(response_state.error_code),
                &response_state.message.clone().unwrap_or_default(),
            );
        }
    }

    Ok(HttpResponse::Ok().finish())
}

#[cfg(test)]
mod tests {

    use super::*;
    use actix_web::{App, test};
    use desk_utils::logs::init_logs;

    #[actix_web::test]
    async fn it_works() {
        let _ = init_logs(log::LevelFilter::Debug);
        //env_logger::init_from_env(env_logger::Env::new().default_filter_or("DEBUG"));
        let app = test::init_service(App::new().service(list_files)).await;
        #[cfg(not(target_os = "windows"))]
        let uri_path = "/file/list?path=/sys&page_no=1&page_count=200";
        #[cfg(target_os = "windows")]
        let uri_path = "/file/list?path=C:\\&page_no=1&page_count=200";

        let req = test::TestRequest::get().uri(uri_path).to_request();
        log::info!("req={:?}", req);
        let resp = test::call_and_read_body(&app, req).await;
        log::info!("resp={:?}", resp);

        // blank path

        #[cfg(not(target_os = "windows"))]
        let uri_path = "/file/list?path=&page_no=1&page_count=200";
        #[cfg(target_os = "windows")]
        let uri_path = "/file/list?path=&page_no=1&page_count=200";

        let req = test::TestRequest::get().uri(uri_path).to_request();
        log::info!("req={:?}", req);
        let resp = test::call_and_read_body(&app, req).await;
        log::info!("resp={:?}", resp);
    }
}
