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

#[derive(Serialize, ToSchema, Debug)]
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

    pub fn append_error(&mut self, err_msg: String) {
        if self.err_msg == None {
            self.err_msg = Some(err_msg)
        } else {
            let origin_err_msg = self.err_msg.clone().unwrap();
            self.err_msg = Some(origin_err_msg +"\n"+ &err_msg);
        }
    }

    pub fn new(
        path: PathBuf
    ) -> Result<Self, DeskError> {
        let file_name = if let Some(file_name) = path.file_name() {
            file_name.to_string_lossy().to_string()
        } else {
            "".to_string()
        };
        
        let metadata = path.metadata();
        let mut file_info = Self {
            name: file_name,
            path: path.to_string_lossy().to_string(),
            size: 0,
            permissions: 0,
            accessed: Local.timestamp_opt(0, 0).unwrap(),
            created: Local.timestamp_opt(0, 0).unwrap(),
            modified: Local.timestamp_opt(0, 0).unwrap(),
            err_msg: None,
        };
        match metadata {
            Ok(metadata) => {
                file_info.size = metadata.len();
                match metadata.accessed() {
                    Ok(accessed) => file_info.accessed = DateTime::<Local>::from(accessed),
                    Err(err) => file_info.append_error(format!("{:?}", err)),
                }
                match metadata.created() {
                    Ok(created) => file_info.created = DateTime::<Local>::from(created),
                    Err(err) => file_info.append_error(format!("{:?}", err)),
                }
                match metadata.modified() {
                    Ok(modified) => file_info.modified = DateTime::<Local>::from(modified),
                    Err(err) => file_info.append_error(format!("{:?}", err)),
                }
            }
            Err(err) => {
                file_info.err_msg = Some(format!("{:?}", err));
            }
        };
        Ok(file_info)
    }
}

#[derive(Serialize, ToSchema)]
pub struct FileListResponse {
    pub file_info_list: Vec<FileInfo>,
    pub total_count: i64,
}
