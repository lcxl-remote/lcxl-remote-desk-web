use std::path::PathBuf;

use actix_web::{HttpResponse, get, web};

use crate::{
    desk_error::DeskError,
    model::{
        files::{FileInfo, FileListParams, FileListResponse},
        settings::SharedSettings,
    },
};

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

        let metadata = entry.metadata().await?;
        let file_info = FileInfo::new(&metadata)?;
        file_info_list.push(file_info);
    }
    Ok(HttpResponse::Ok().json(FileListResponse {
        file_info_list,
        total_count,
    }))
}
