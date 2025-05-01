use std::path::PathBuf;

use actix_web::{HttpResponse, get, web};

use crate::{
    desk_error::DeskError,
    model::{
        files::{FileInfo, FileListParams, FileListResponse},
        settings::SharedSettings,
    },
};

#[cfg(target_os="windows")]
pub fn get_logical_driver_list() -> Result<Vec<FileInfo>, DeskError>{
    let mut file_info_list = vec![];
    Ok(file_info_list)
}

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
    settings: web::Data<SharedSettings>,
) -> Result<HttpResponse, DeskError> {
    let path = PathBuf::from(query_list.path.as_str());

    #[cfg(target_os="windows")]
    if query_list.path.is_empty() {
        let file_info_list = get_logical_driver_list()?;
        let total_count = file_info_list.len() as i64;
        return Ok(HttpResponse::Ok().json(FileListResponse {
            file_info_list,
            total_count,
        }));
    }
    let mut file_info_list = vec![];
    let mut entries = tokio::fs::read_dir(path.as_path()).await?;
    let start_index = (query_list.page_no - 1) * query_list.page_count;
    let end_index = query_list.page_no * query_list.page_count;
    let mut total_count: i64 = 0;
    while let Some(entry) = entries.next_entry().await? {
        total_count += 1;
        if total_count <= start_index {
            continue;
        } else if total_count > end_index {
            break;
        }

        let metadata = entry.metadata().await;
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_info = FileInfo::new(file_name.as_str(), entry.path(), metadata)?;
        file_info_list.push(file_info);
    }
    Ok(HttpResponse::Ok().json(FileListResponse {
        file_info_list,
        total_count,
    }))
}
