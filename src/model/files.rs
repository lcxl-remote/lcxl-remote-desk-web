use std::fs::Metadata;

use chrono::{DateTime, Local};
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
    pub size: u64,
    pub permissions: u32,
    pub accessed: DateTime<Local>,
    pub created: DateTime<Local>,
    pub modified: DateTime<Local>,
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

    pub fn new(metadata: &Metadata) -> Result<Self, DeskError> {
        Ok(Self {
            name: String::new(),
            size: metadata.len(),
            permissions: FileInfo::get_permissions(metadata),
            accessed: DateTime::<Local>::from(metadata.accessed()?),
            created: DateTime::<Local>::from(metadata.created()?),
            modified: DateTime::<Local>::from(metadata.modified()?),
        })
    }
}

#[derive(Serialize, ToSchema)]
pub struct FileListResponse {
    pub file_info_list: Vec<FileInfo>,
    pub total_count: i64,
}
