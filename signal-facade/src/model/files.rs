use std::{fs::Metadata, path::PathBuf};

use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::error::DeskSignalFacadeError;

#[derive(Deserialize, Serialize, ToSchema, IntoParams, Default)]
pub struct FileListParams {
    pub path: String,
    pub page_no: i64,
    pub page_count: i64,

    /// Minimum file size
    pub min_file_size: Option<i64>,
    /// Max file size
    pub max_file_size: Option<i64>,
    /// File name filtering
    pub file_name: Option<String>,
    /// New field for file extension filtering
    pub file_extension: Option<String>,
    /// Optional file extension list filtering, comma(,) separated values.
    pub file_extension_list: Option<String>,
    /// Optional time range filter for file creation.
    pub start_created_time: Option<DateTime<Local>>,
    pub end_created_time: Option<DateTime<Local>>,
    /// Optional time range filter for file modification.
    pub start_modified_time: Option<DateTime<Local>>,
    pub end_modified_time: Option<DateTime<Local>>,
    /// Connection ID for remote desk
    pub connection_id: Option<String>,
}

#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub permissions: u32,
    pub accessed: DateTime<Local>,
    pub created: DateTime<Local>,
    pub modified: DateTime<Local>,
    pub err_msg: Option<String>,
}

impl FileInfo {
    #[cfg(target_os = "linux")]
    pub fn get_permissions(metadata: &Metadata) -> u32 {
        use std::os::linux::fs::MetadataExt;
        metadata.st_mode()
    }

    #[cfg(target_os = "macos")]
    pub fn get_permissions(metadata: &Metadata) -> u32 {
        use std::os::macos::fs::MetadataExt;
        metadata.st_mode()
    }

    #[cfg(target_os = "windows")]
    pub fn get_permissions(metadata: &Metadata) -> u32 {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
    }

    pub fn add_err_msg(&mut self, err_msg: String) {
        if self.err_msg.is_none() {
            self.err_msg = Some(err_msg)
        } else {
            let origin_err_msg = self.err_msg.clone().unwrap();
            self.err_msg = Some(origin_err_msg + "\n" + &err_msg);
        }
    }

    pub fn new(path: PathBuf) -> Result<Self, DeskSignalFacadeError> {
        let file_name = if let Some(file_name) = path.file_name() {
            file_name.to_string_lossy().to_string()
        } else {
            // For paths like "C:\" where file_name() returns None,
            // use the full path string as the display name
            path.to_string_lossy().to_string()
        };

        let metadata = path.metadata();
        let mut file_info = Self {
            name: file_name,
            path: path.to_string_lossy().to_string(),
            size: 0,
            is_dir: false,
            is_file: false,
            is_symlink: false,
            permissions: 0,
            accessed: Local.timestamp_opt(0, 0).unwrap(),
            created: Local.timestamp_opt(0, 0).unwrap(),
            modified: Local.timestamp_opt(0, 0).unwrap(),
            err_msg: None,
        };
        match metadata {
            Ok(metadata) => {
                file_info.size = metadata.len();
                file_info.is_dir = metadata.is_dir();
                file_info.is_file = metadata.is_file();
                file_info.is_symlink = metadata.is_symlink();
                file_info.permissions = FileInfo::get_permissions(&metadata);
                use chrono::{DateTime, Local};
                match metadata.accessed() {
                    Ok(accessed) => file_info.accessed = DateTime::<Local>::from(accessed),
                    Err(err) => {
                        file_info.add_err_msg(format!("Failed to get file accessed time: {}", err))
                    }
                }
                match metadata.created() {
                    Ok(created) => file_info.created = DateTime::<Local>::from(created),
                    Err(err) => {
                        file_info.add_err_msg(format!("Failed to get file created time: {}", err))
                    }
                }
                match metadata.modified() {
                    Ok(modified) => file_info.modified = DateTime::<Local>::from(modified),
                    Err(err) => {
                        file_info.add_err_msg(format!("Failed to get file modified time: {}", err))
                    }
                }
            }
            Err(err) => {
                file_info.err_msg = Some(format!("Failed to get file metadata: {}", err));
            }
        };
        Ok(file_info)
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct FileListResponse {
    pub file_info_list: Vec<FileInfo>,
    pub total_count: i64,
}

/// Request body for deleting a file.
#[derive(Deserialize, Serialize, ToSchema, Clone, Debug, Default)]
pub struct DeleteFileRequest {
    /// The path of file to be deleted
    pub file_path: String,
    /// Whether to delete permanently or move to trash
    pub delete_permanently: Option<bool>,
    pub connection_id: Option<String>,
}
