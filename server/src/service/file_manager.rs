use crate::error::DeskError;
use crate::service::signaling::DeskSession;
use desk_signal_facade::model::files::{
    DeleteFileRequest, FileInfo, FileListParams, FileListResponse,
};
use desk_signal_facade::model::signal::{PeerSignalingSender, SignalingModel, SignalingType};
use desk_utils::error::DeskErrorCode;

use log::{debug, info, warn};
use std::path::PathBuf;
use tokio::fs;

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
        if *item == 0 && end_index > start_index {
            let driver = String::from_utf16_lossy(&lp_buffer[start_index..end_index]);
            info!("driver: {}", driver);
            let driver_path_buf = PathBuf::from(driver.as_str());
            file_info_list.push(FileInfo::new(driver_path_buf)?);
            start_index = end_index + 1;
        }
        end_index += 1;
    }
    Ok(file_info_list)
}

pub async fn list_files(query_list: FileListParams) -> Result<FileListResponse, DeskError> {
    #[cfg(target_os = "windows")]
    if query_list.path.is_empty() {
        let file_info_list = get_logical_driver_list()?;
        let total_count = file_info_list.len() as i64;
        return Ok(FileListResponse {
            file_info_list,
            total_count,
        });
    }

    let path_str = if cfg!(not(target_os = "windows")) && query_list.path.is_empty() {
        "/".to_string()
    } else {
        query_list.path.clone()
    };
    let path = PathBuf::from(&path_str);

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
    Ok(FileListResponse {
        file_info_list,
        total_count,
    })
}

pub async fn delete_file(delete_file_request: DeleteFileRequest) -> Result<(), DeskError> {
    let file_path = PathBuf::from(delete_file_request.file_path.as_str());
    if !file_path.exists() {
        return Ok(());
    }

    // remove file
    let delete_permanently = delete_file_request.delete_permanently.unwrap_or(false);
    if delete_permanently {
        warn!("Delete file {} permanently", delete_file_request.file_path);
        fs::remove_file(&file_path).await?;
    } else {
        // move to trash dir
        info!("Move file {} to trash dir", delete_file_request.file_path);
        if let Err(e) = trash::delete(&file_path) {
            use desk_utils::error::DeskErrorCode;
            log::error!(
                "Failed to delete file to trash: {}, error: {}",
                delete_file_request.file_path,
                e
            );
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("Failed to move to trash: {}", e),
            );
        }
    }

    info!("Delete file {} successfully", delete_file_request.file_path);
    Ok(())
}

pub async fn handle_manager_file_list(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = match signaling_model.from_connection_id.clone() {
        Some(connection_id) => connection_id,
        None => {
            log::warn!("ListFiles without a controller connection was dropped");
            return Ok(());
        }
    };
    let params = signaling_model.get_data::<FileListParams>()?;
    if !desk_session
        .file_browse_permission(&from_connection_id)
        .await
    {
        desk_session
            .session
            .send_error(
                &signaling_model.request_id,
                SignalingType::FilesListed,
                Some(from_connection_id),
                DeskErrorCode::PERMISSION_ERROR,
                "File browse access denied",
            )
            .await?;
        return Ok(());
    }
    desk_session.mark_file_manager_opened(&from_connection_id);

    match list_files(params).await {
        Ok(response) => {
            desk_session
                .session
                .send_response(
                    &signaling_model.request_id,
                    SignalingType::FilesListed,
                    Some(from_connection_id),
                    &response,
                )
                .await?;
        }
        Err(e) => {
            desk_session
                .session
                .send_error(
                    &signaling_model.request_id,
                    SignalingType::FilesListed,
                    Some(from_connection_id),
                    e.to_error_code(),
                    &e.to_string(),
                )
                .await?;
        }
    }
    Ok(())
}

pub async fn handle_manager_file_delete(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = match signaling_model.from_connection_id.clone() {
        Some(connection_id) => connection_id,
        None => {
            log::warn!("DeleteFile without a controller connection was dropped");
            return Ok(());
        }
    };
    let params = signaling_model.get_data::<DeleteFileRequest>()?;

    if !desk_session
        .file_browse_permission(&from_connection_id)
        .await
    {
        desk_session
            .session
            .send_error(
                &signaling_model.request_id,
                SignalingType::FileDeleted,
                Some(from_connection_id),
                DeskErrorCode::PERMISSION_ERROR,
                "File browse access denied",
            )
            .await?;
        return Ok(());
    }
    if !desk_session
        .file_delete_permission(&from_connection_id)
        .await
    {
        desk_session
            .session
            .send_error(
                &signaling_model.request_id,
                SignalingType::FileDeleted,
                Some(from_connection_id),
                DeskErrorCode::PERMISSION_ERROR,
                "File delete access denied",
            )
            .await?;
        return Ok(());
    }
    desk_session.mark_file_manager_opened(&from_connection_id);

    match delete_file(params).await {
        Ok(()) => {
            desk_session
                .session
                .send_response(
                    &signaling_model.request_id,
                    SignalingType::FileDeleted,
                    Some(from_connection_id),
                    &serde_json::json!({}),
                )
                .await?;
        }
        Err(e) => {
            desk_session
                .session
                .send_error(
                    &signaling_model.request_id,
                    SignalingType::FileDeleted,
                    Some(from_connection_id),
                    e.to_error_code(),
                    &e.to_string(),
                )
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_list_files() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file.txt");
        File::create(&file_path).unwrap();

        let params = FileListParams {
            path: dir.path().to_string_lossy().to_string(),
            page_no: 1,
            page_count: 10,
            ..Default::default()
        };

        let result = list_files(params).await;
        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(!response.file_info_list.is_empty());
        // Verify that the created file is in the list
        let found = response
            .file_info_list
            .iter()
            .any(|f| f.name == "test_file.txt");
        assert!(found);
    }

    #[tokio::test]
    async fn test_delete_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("file_to_delete.txt");
        File::create(&file_path).unwrap();

        let req = DeleteFileRequest {
            file_path: file_path.to_string_lossy().to_string(),
            delete_permanently: Some(true),
            ..Default::default()
        };

        let result = delete_file(req).await;
        assert!(result.is_ok());
        assert!(!file_path.exists());
    }
}
