use std::path::PathBuf;

use actix_web::{HttpResponse, get, web};
use log::debug;

use crate::{
    desk_error::DeskError,
    model::{
        files::{FileInfo, FileListParams, FileListResponse},
        settings::SharedSettings,
    },
};

#[cfg(target_os = "windows")]
pub fn get_logical_driver_list() -> Result<Vec<FileInfo>, DeskError> {
    let mut file_info_list = vec![];

    use log::info;
    use windows::Win32::{Foundation::{GetLastError, ERROR_INVALID_INDEX, STATUS_RTPM_INVALID_CONTEXT}, Storage::FileSystem::GetLogicalDriveStringsW};

    let lp_buffer: Vec<u16> = unsafe {
        let str_len = GetLogicalDriveStringsW(None);
        if str_len == 0 {
            // something wrong, get last error
            return DeskError::windows_error();
        }

        let mut lp_buffer  = vec![0u16; str_len as usize];
        let str_len = GetLogicalDriveStringsW(Some(&mut lp_buffer));
        // double check
        if str_len == 0 {
            // something wrong, get last error
            return DeskError::windows_error();
        }
        lp_buffer.into_iter().take(str_len as usize).collect()
    };
    let mut start_index = 0;
    let mut end_index = 0;
    for item in  lp_buffer.iter() {
        if *item == 0 {
            if end_index > start_index {
                let driver = String::from_utf16_lossy(&lp_buffer[start_index..end_index]);
                info!("driver: {}", driver);
                let driver_path_buf = PathBuf::from(driver.as_str());
                file_info_list.push(FileInfo::new(driver_path_buf)?);
                start_index = end_index;
            }
        }
        end_index+=1;
    }
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
pub async fn list_files(query_list: web::Query<FileListParams>) -> Result<HttpResponse, DeskError> {
    let path = PathBuf::from(query_list.path.as_str());

    #[cfg(target_os = "windows")]
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

        let file_info = FileInfo::new(entry.path())?;
        debug!("file_info={:?}", file_info);
        file_info_list.push(file_info);
    }
    Ok(HttpResponse::Ok().json(FileListResponse {
        file_info_list,
        total_count,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, http::header::ContentType, test};
    use log::{error, info};

    #[actix_web::test]
    async fn it_works() {
        env_logger::init_from_env(env_logger::Env::new().default_filter_or("DEBUG"));
        let app = test::init_service(App::new().service(list_files)).await;
        #[cfg(target_os = "linux")]
        let uri_path = "/file/list?path=/sys&page_no=1&page_count=200";
        #[cfg(target_os = "windows")]
        let uri_path = "/file/list?path=&page_no=1&page_count=200";

        let req = test::TestRequest::get().uri(uri_path).to_request();
        error!("req={:?}", req);
        let resp = test::call_and_read_body(&app, req).await;
        error!("resp={:?}", resp);
    }
}
