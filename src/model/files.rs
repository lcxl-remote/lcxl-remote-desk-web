use std::{fs::Metadata, io, path::PathBuf};

use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::desk_error::DeskError;

#[derive(Deserialize, ToSchema, IntoParams)]
pub struct FileListParams {
    pub path: String,
    pub page_no: i64,
    pub page_count: i64,
}

#[derive(Serialize, ToSchema)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
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

    #[cfg(target_os = "windows")]
    pub fn get_permissions(metadata: &Metadata) -> u32 {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes()
    }

    pub fn new(name: &str, path: PathBuf, metadata: io::Result<Metadata>) -> Result<Self, DeskError> {
        let file_info = match metadata {
            Ok(metadata) => Self {
                name: String::from(name),
                path: path.to_string_lossy().to_string(),
                size: metadata.len(),
                permissions: FileInfo::get_permissions(&metadata),
                accessed: DateTime::<Local>::from(metadata.accessed()?),
                created: DateTime::<Local>::from(metadata.created()?),
                modified: DateTime::<Local>::from(metadata.modified()?),
                err_msg: None,
            },    
            Err(err) => {
                Self {
                    name: String::from(name),
                    path: path.to_string_lossy().to_string(),
                    size: 0,
                    permissions:0,
                    accessed:Local.timestamp_opt(0, 0).unwrap(),
                    created: Local.timestamp_opt(0, 0).unwrap(),
                    modified: Local.timestamp_opt(0, 0).unwrap(),
                    err_msg: Some(format!("{:?}", err)),
                }
            },
        };
        Ok(file_info)
    }
}

#[derive(Serialize, ToSchema)]
pub struct FileListResponse {
    pub file_info_list: Vec<FileInfo>,
    pub total_count: i64,
}
