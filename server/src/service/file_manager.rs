use crate::error::DeskError;
use crate::service::signaling::DeskSession;
use desk_signal_facade::model::files::{
    CreateDirectoryRequest, DeleteFileRequest, FileInfo, FileListParams, FileListResponse,
};
use desk_signal_facade::model::signal::{PeerSignalingSender, SignalingModel, SignalingType};
use desk_utils::error::DeskErrorCode;

use crate::model::security_approval::SecurityPermissionType;

use log::{debug, info, warn};
use std::path::PathBuf;
use tokio::fs;

pub use access::ListPolicy;

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

/// List a directory. With a `policy`, also report what the host user may do
/// with each entry and with the directory itself.
pub async fn list_files(
    query_list: FileListParams,
    policy: Option<ListPolicy>,
) -> Result<FileListResponse, DeskError> {
    let home = if query_list.prefer_user_home && query_list.path.is_empty() {
        tokio::task::spawn_blocking(crate::user_home::current)
            .await
            .unwrap_or(None)
    } else {
        None
    };
    list_files_with_home(query_list, home, policy).await
}

async fn list_files_with_home(
    query_list: FileListParams,
    home: Option<String>,
    policy: Option<ListPolicy>,
) -> Result<FileListResponse, DeskError> {
    if query_list.prefer_user_home
        && query_list.path.is_empty()
        && let Some(home) = home
    {
        let mut preferred = query_list.clone();
        preferred.path = home;
        match list_files_at_path(preferred, policy).await {
            Ok(response) => return Ok(response),
            Err(error) => {
                warn!("Initial user home listing failed; falling back to root: {error}");
            }
        }
    }
    list_files_at_path(query_list, policy).await
}

async fn list_files_at_path(
    query_list: FileListParams,
    policy: Option<ListPolicy>,
) -> Result<FileListResponse, DeskError> {
    if query_list.directories_only {
        return directory_listing::list_directories(&query_list).await;
    }
    #[cfg(target_os = "windows")]
    if query_list.path.is_empty() {
        let file_info_list = get_logical_driver_list()?;
        let total_count = file_info_list.len() as i64;
        // Drive roots are never deleted and the drive list takes no writes,
        // so every flag stays off.
        return Ok(FileListResponse {
            path: query_list.path,
            file_info_list,
            total_count,
            can_write: false,
            trash_available: false,
        });
    }

    let path_str = if cfg!(not(target_os = "windows")) && query_list.path.is_empty() {
        "/".to_string()
    } else {
        query_list.path.clone()
    };
    let path = PathBuf::from(&path_str);

    let mut file_info_list = vec![];
    // Exact path of each entry, kept beside its lossy wire string.
    let mut entry_paths = vec![];
    if query_list.page_no == 1 {
        //get parent dir
        if let Some(parent_dir) = path.parent() {
            let mut parent_file_info = FileInfo::new(parent_dir.to_path_buf())?;
            parent_file_info.name = "..".to_string();
            // add parent dir to list
            file_info_list.push(parent_file_info);
            entry_paths.push(parent_dir.to_path_buf());
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
            entry_paths.push(entry.path());
        }
    }
    info!("List path: {}, total count: {}", path_str, total_count);
    let mut response = FileListResponse {
        path: query_list.path,
        file_info_list,
        total_count,
        can_write: false,
        trash_available: false,
    };
    if let Some(policy) = policy {
        response = tokio::task::spawn_blocking(move || {
            access::fill(&mut response, &entry_paths, Some(&path), policy);
            response
        })
        .await
        .map_err(|error| {
            DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?;
    }
    Ok(response)
}

mod access;
mod create_directory;
mod delete;
mod directory_listing;
#[cfg(test)]
mod initial_directory_tests;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

pub async fn delete_file(delete_file_request: DeleteFileRequest) -> Result<(), DeskError> {
    let path = PathBuf::from(delete_file_request.file_path);
    let permanently = delete_file_request.delete_permanently.unwrap_or(false);
    tokio::task::spawn_blocking(move || delete::delete_entry(&path, permanently))
        .await
        .map_err(|error| {
            DeskError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.to_string())
        })?
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

    let policy = ListPolicy {
        delete_allowed: desk_session
            .capability_not_denied(&from_connection_id, SecurityPermissionType::FileDelete)
            .await,
        transfer_allowed: desk_session.file_transfer.is_some()
            && desk_session
                .capability_not_denied(&from_connection_id, SecurityPermissionType::FileTransfer)
                .await,
    };
    match list_files(params, Some(policy)).await {
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

pub async fn handle_manager_directory_create(
    desk_session: &mut DeskSession,
    signaling_model: &SignalingModel,
) -> Result<(), DeskError> {
    let Some(from_connection_id) = signaling_model.from_connection_id.clone() else {
        log::warn!("CreateDirectory without a controller connection was dropped");
        return Ok(());
    };
    let params = signaling_model.get_data::<CreateDirectoryRequest>()?;
    // Order: name syntax, browse, transfer, file system. A malformed name is
    // refused before anyone is asked to approve it.
    let denied = if let Err(error) = create_directory::validate_name(&params.name) {
        Some((error.to_error_code(), error.to_string()))
    } else if !desk_session
        .file_browse_permission(&from_connection_id)
        .await
    {
        Some((
            DeskErrorCode::PERMISSION_ERROR,
            "File browse access denied".to_string(),
        ))
    } else {
        let transfer = match desk_session.file_transfer.as_ref() {
            Some(dispatcher) => dispatcher.transfer_permission(&from_connection_id).await,
            None => false,
        };
        (!transfer).then(|| {
            (
                DeskErrorCode::PERMISSION_ERROR,
                "File transfer access denied".to_string(),
            )
        })
    };
    if let Some((code, message)) = denied {
        desk_session
            .session
            .send_error(
                &signaling_model.request_id,
                SignalingType::DirectoryCreated,
                Some(from_connection_id),
                code,
                &message,
            )
            .await?;
        return Ok(());
    }
    desk_session.mark_file_manager_opened(&from_connection_id);

    match create_directory::create_directory(&params).await {
        Ok(()) => {
            desk_session
                .session
                .send_response(
                    &signaling_model.request_id,
                    SignalingType::DirectoryCreated,
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
                    SignalingType::DirectoryCreated,
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

        let result = list_files(params, None).await;
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

    fn listing_params(dir: &std::path::Path) -> FileListParams {
        FileListParams {
            path: dir.to_string_lossy().to_string(),
            page_no: 1,
            page_count: 100,
            ..Default::default()
        }
    }

    fn entry<'a>(response: &'a FileListResponse, name: &str) -> &'a FileInfo {
        response
            .file_info_list
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("{name} not listed"))
    }

    const ALLOW_ALL: ListPolicy = ListPolicy {
        delete_allowed: true,
        transfer_allowed: true,
    };

    #[tokio::test]
    async fn listing_reports_what_the_host_user_can_do() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("file.txt")).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let response = list_files(listing_params(dir.path()), Some(ALLOW_ALL))
            .await
            .unwrap();
        let file = entry(&response, "file.txt");
        assert!(file.can_read && file.can_delete && !file.can_open);
        let sub = entry(&response, "sub");
        assert!(!sub.can_read && sub.can_delete && sub.can_open);
        let parent = entry(&response, "..");
        assert!(!parent.can_read && !parent.can_delete);
        assert!(response.can_write);

        // Without a policy (the directory picker and plain listings) every
        // action flag stays off.
        let plain = list_files(listing_params(dir.path()), None).await.unwrap();
        assert!(!entry(&plain, "file.txt").can_read && !plain.can_write);
    }

    #[tokio::test]
    async fn denied_policies_switch_their_actions_off() {
        let dir = tempdir().unwrap();
        File::create(dir.path().join("file.txt")).unwrap();
        let no_delete = ListPolicy {
            delete_allowed: false,
            transfer_allowed: true,
        };
        let response = list_files(listing_params(dir.path()), Some(no_delete))
            .await
            .unwrap();
        assert!(entry(&response, "file.txt").can_read);
        assert!(!entry(&response, "file.txt").can_delete);

        let no_transfer = ListPolicy {
            delete_allowed: true,
            transfer_allowed: false,
        };
        let response = list_files(listing_params(dir.path()), Some(no_transfer))
            .await
            .unwrap();
        assert!(!entry(&response, "file.txt").can_read);
        assert!(entry(&response, "file.txt").can_delete);
        assert!(!response.can_write);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_system_permissions_switch_actions_off() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root bypasses permission bits
        }
        let dir = tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let secret = locked.join("secret.txt");
        File::create(&secret).unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let response = list_files(listing_params(&locked), Some(ALLOW_ALL)).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let response = response.unwrap();
        let secret = entry(&response, "secret.txt");
        assert!(!secret.can_read, "unreadable file offers no download");
        assert!(!secret.can_delete, "read-only parent offers no delete");
        assert!(!response.can_write, "read-only directory offers no writes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sticky_directories_only_offer_deleting_own_entries() {
        // /tmp-style sticky directories hold other users' files; a fresh
        // sticky directory owned by us still lets us delete our own file.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        File::create(dir.path().join("mine.txt")).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o1777)).unwrap();
        let response = list_files(listing_params(dir.path()), Some(ALLOW_ALL))
            .await
            .unwrap();
        assert!(entry(&response, "mine.txt").can_delete);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn lossy_names_offer_no_action() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempdir().unwrap();
        let lossy = dir.path().join(std::ffi::OsStr::from_bytes(b"bad\xff"));
        std::fs::create_dir(&lossy).unwrap();
        // A real directory whose name is the lossy rendering of the first.
        std::fs::create_dir(dir.path().join("bad\u{FFFD}")).unwrap();
        let response = list_files(listing_params(dir.path()), Some(ALLOW_ALL))
            .await
            .unwrap();
        let same_name: Vec<_> = response
            .file_info_list
            .iter()
            .filter(|f| f.name == "bad\u{FFFD}")
            .collect();
        assert_eq!(same_name.len(), 2);
        assert_eq!(
            same_name
                .iter()
                .filter(|f| f.can_open && f.can_delete)
                .count(),
            1
        );
        assert_eq!(
            same_name
                .iter()
                .filter(|f| !f.can_open && !f.can_delete)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn test_delete_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("file_to_delete.txt");
        File::create(&file_path).unwrap();

        let req = DeleteFileRequest {
            file_path: file_path.to_string_lossy().to_string(),
            delete_permanently: Some(true),
        };

        let result = delete_file(req).await;
        assert!(result.is_ok());
        assert!(!file_path.exists());
    }
}
