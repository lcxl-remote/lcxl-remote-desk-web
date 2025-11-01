use std::path::PathBuf;

use actix_web::{HttpResponse, delete, get, web};
use log::{debug, info, warn};
use tokio::fs;

use crate::{
    desk_error::DeskError,
    model::{
        common::{ErrorCode, RestResponse},
        files::{DeleteFileRequest, FileInfo, FileListParams, FileListResponse},
    },
};

#[cfg(target_os = "windows")]
pub fn get_logical_driver_list() -> Result<Vec<FileInfo>, DeskError> {
    let mut file_info_list = vec![];

    use log::info;
    use windows::Win32::Storage::FileSystem::GetLogicalDriveStringsW;

    let lp_buffer: Vec<u16> = unsafe {
        let str_len = GetLogicalDriveStringsW(None);
        if str_len == 0 {
            // something wrong, get last error
            return DeskError::windows_error();
        }

        let mut lp_buffer = vec![0u16; str_len as usize];
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
    for item in lp_buffer.iter() {
        if *item == 0 {
            if end_index > start_index {
                let driver = String::from_utf16_lossy(&lp_buffer[start_index..end_index]);
                info!("driver: {}", driver);
                let driver_path_buf = PathBuf::from(driver.as_str());
                file_info_list.push(FileInfo::new(driver_path_buf)?);
                start_index = end_index + 1;
            }
        }
        end_index += 1;
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
    #[cfg(target_os = "windows")]
    if query_list.path.is_empty() {
        let file_info_list = get_logical_driver_list()?;
        let total_count = file_info_list.len() as i64;
        return Ok(HttpResponse::Ok().json(FileListResponse {
            file_info_list,
            total_count,
        }));
    }

    // path_str need to be mut in linux/macos platform
    #[allow(unused_mut)]
    let mut path_str = query_list.path.as_str();
    #[cfg(not(target_os = "windows"))]
    if query_list.path.is_empty() {
        path_str = "/";
    }
    let path = PathBuf::from(path_str);

    let mut file_info_list = vec![];
    if query_list.page_no == 1 {
        //get parent dir
        match path.parent() {
            Some(parent_dir) => {
                let mut parent_file_info = FileInfo::new(parent_dir.to_path_buf())?;
                parent_file_info.name = "..".to_string();
                // add parent dir to list
                file_info_list.push(parent_file_info);
            }
            None => {
                #[cfg(target_os = "windows")]
                {
                    use chrono::{Local, TimeZone};
                    let fake_root_dir = FileInfo {
                        name: "..".to_string(),
                        path: "".to_string(),
                        size: 0,
                        is_dir: true,
                        is_file: false,
                        is_symlink: false,
                        permissions: 0,
                        accessed: Local.timestamp_opt(0, 0).unwrap(),
                        created: Local.timestamp_opt(0, 0).unwrap(),
                        modified: Local.timestamp_opt(0, 0).unwrap(),
                        err_msg: None,
                    };
                    file_info_list.push(fake_root_dir);
                }
            }
        }
    }

    let mut entries = tokio::fs::read_dir(path.as_path()).await?;
    let start_index = (query_list.page_no - 1) * query_list.page_count;
    let mut total_count = 1i64;
    while let Some(entry) = entries.next_entry().await? {
        total_count += 1;
        if total_count <= start_index {
            continue;
        } else if (file_info_list.len() as i64) < query_list.page_count {
            let file_info = FileInfo::new(entry.path())?;
            debug!("file_info={:?}", file_info);
            file_info_list.push(file_info);
        }
    }
    info!("List path: {}, total count: {}", path_str, total_count);
    Ok(HttpResponse::Ok().json(FileListResponse {
        file_info_list,
        total_count,
    }))
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
) -> Result<HttpResponse, DeskError> {
    let delete_file_request = requst_json.into_inner();

    let file = PathBuf::from(delete_file_request.file_path.as_str());
    if !file.exists() {
        // remove file from db

        return Ok(
            HttpResponse::Ok().json(RestResponse::succeed_with_message(format!(
                "File {} is not exist",
                file.display()
            ))),
        );
    }

    // remove file
    let delete_permanently = delete_file_request.delete_permanently.unwrap_or(false);
    if delete_permanently {
        warn!("Delete file {} permanently", delete_file_request.file_path);
        fs::remove_file(delete_file_request.file_path.as_str()).await?;
    } else {
        // move to trash dir
        info!("Move file {} to trash dir", delete_file_request.file_path);
        #[cfg(target_os = "windows")]
        {
            use log::error;
            use std::ffi::c_void;
            use windows::Win32::Foundation::HWND;
            use windows::Win32::UI::Shell::{
                FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
                SHFILEOPSTRUCTW, SHFileOperationW,
            };

            use windows::core::{BOOL, HSTRING, PCWSTR};

            let mut fileop = SHFILEOPSTRUCTW {
                hwnd: HWND::default(),
                wFunc: FO_DELETE,
                pFrom: PCWSTR(HSTRING::from(delete_file_request.file_path.as_str()).as_ptr()),
                pTo: PCWSTR::null(),
                fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
                fAnyOperationsAborted: BOOL::from(false),
                hNameMappings: 0 as *mut c_void,
                lpszProgressTitle: PCWSTR::null(),
            };

            // invoke SHFileOperationW to move file to trash
            let opr_code = unsafe { SHFileOperationW(&mut fileop) };
            if opr_code != 0 {
                error!(
                    "Failed to delete file: {}, code: {}",
                    delete_file_request.file_path, opr_code
                );
                return Ok(HttpResponse::Ok().json(RestResponse::failed(
                    ErrorCode::WINDOWS_ERROR,
                    format!(
                        "Failed to delete file: {}, code: {}",
                        delete_file_request.file_path, opr_code
                    ),
                )));
            }

            info!(
                "Moved file {} to trash successfully",
                delete_file_request.file_path
            )
        }

        #[cfg(target_os = "linux")]
        {
            // Linux specific code to move file to trash
            return Ok(HttpResponse::Ok().json(RestResponse::failed(
                ErrorCode::SYSTEM_ERROR,
                "Need implementation".to_string(),
            )));
        }

        #[cfg(target_os = "macos")]
        {
            // Linux specific code to move file to trash
            return Ok(HttpResponse::Ok().json(RestResponse::failed(
                ErrorCode::SYSTEM_ERROR,
                "Need implementation".to_string(),
            )));
        }
    }

    info!("Delete file {} successfully", delete_file_request.file_path);
    Ok(HttpResponse::Ok().finish())
}

#[cfg(test)]
mod tests {
    use crate::utils::logs::init_logs;

    use super::*;
    use actix_web::{App, test};

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
        info!("req={:?}", req);
        let resp = test::call_and_read_body(&app, req).await;
        info!("resp={:?}", resp);

        // blank path

        #[cfg(not(target_os = "windows"))]
        let uri_path = "/file/list?path=&page_no=1&page_count=200";
        #[cfg(target_os = "windows")]
        let uri_path = "/file/list?path=&page_no=1&page_count=200";

        let req = test::TestRequest::get().uri(uri_path).to_request();
        info!("req={:?}", req);
        let resp = test::call_and_read_body(&app, req).await;
        info!("resp={:?}", resp);
    }
}
