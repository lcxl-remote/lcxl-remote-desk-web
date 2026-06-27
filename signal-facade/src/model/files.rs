use std::{fs::Metadata, path::PathBuf};

use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::error::DeskSignalFacadeError;
use crate::wincode_adapters::DateTimeLocalWincode;

#[derive(
    Deserialize,
    Serialize,
    ToSchema,
    IntoParams,
    Default,
    Clone,
    Debug,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
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
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub start_created_time: Option<DateTime<Local>>,
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub end_created_time: Option<DateTime<Local>>,
    /// Optional time range filter for file modification.
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub start_modified_time: Option<DateTime<Local>>,
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub end_modified_time: Option<DateTime<Local>>,
    /// Connection ID for remote desk
    pub connection_id: Option<String>,
    /// Target device primary key (manager multi-instance addressing). The manager
    /// routes by this; the OSS single-instance signal leaves it `None` and routes
    /// by `connection_id` (dual-target wire model).
    pub device_id: Option<String>,
}

#[derive(
    Serialize, Deserialize, ToSchema, Debug, Clone, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct FileInfo {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub permissions: u32,
    #[wincode(with = "DateTimeLocalWincode")]
    pub accessed: DateTime<Local>,
    #[wincode(with = "DateTimeLocalWincode")]
    pub created: DateTime<Local>,
    #[wincode(with = "DateTimeLocalWincode")]
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

#[derive(
    Serialize, Deserialize, ToSchema, Debug, Clone, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct FileListResponse {
    pub file_info_list: Vec<FileInfo>,
    pub total_count: i64,
}

/// Request body for deleting a file.
#[derive(
    Deserialize,
    Serialize,
    ToSchema,
    Clone,
    Debug,
    Default,
    wincode::SchemaWrite,
    wincode::SchemaRead,
)]
pub struct DeleteFileRequest {
    /// The path of file to be deleted
    pub file_path: String,
    /// Whether to delete permanently or move to trash
    pub delete_permanently: Option<bool>,
    pub connection_id: Option<String>,
    /// Target device primary key (manager multi-instance addressing). See
    /// [`FileListParams::device_id`] for the dual-target rationale.
    pub device_id: Option<String>,
}

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    /// `FileListParams` carries 4 `Option<DateTime<Local>>` fields
    /// that ride `DateTimeLocalWincode` via `#[wincode(with =
    /// "Option<...>")]`. Construct a payload with mixed `Some` /
    /// `None` plus distinct timestamps on each field, so a wrong
    /// adapter wiring (e.g. all four routed to the same instant)
    /// would surface as a value mismatch on decode.
    #[test]
    fn file_list_params_round_trips_wincode_with_mixed_datetime_fields() {
        let original = FileListParams {
            path: r"C:\Users".to_string(),
            page_no: 2,
            page_count: 50,
            min_file_size: Some(1024),
            max_file_size: None,
            file_name: Some("readme".to_string()),
            file_extension: Some("md".to_string()),
            file_extension_list: Some("md,txt".to_string()),
            start_created_time: Some(
                Local
                    .with_ymd_and_hms(2024, 1, 1, 0, 0, 0)
                    .single()
                    .expect("valid local time"),
            ),
            end_created_time: None,
            start_modified_time: Some(
                Local
                    .with_ymd_and_hms(2025, 6, 15, 12, 30, 0)
                    .single()
                    .expect("valid local time"),
            ),
            end_modified_time: Some(
                Local
                    .with_ymd_and_hms(2026, 5, 10, 23, 59, 59)
                    .single()
                    .expect("valid local time"),
            ),
            connection_id: Some("conn-fl".to_string()),
            device_id: Some("42".to_string()),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: FileListParams = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.path, original.path);
        assert_eq!(back.page_no, original.page_no);
        assert_eq!(back.page_count, original.page_count);
        assert_eq!(back.min_file_size, original.min_file_size);
        assert_eq!(back.max_file_size, original.max_file_size);
        assert_eq!(back.file_name, original.file_name);
        assert_eq!(back.start_created_time, original.start_created_time);
        assert_eq!(back.end_created_time, original.end_created_time);
        assert_eq!(back.start_modified_time, original.start_modified_time);
        assert_eq!(back.end_modified_time, original.end_modified_time);
        assert_eq!(back.connection_id, original.connection_id);
        assert_eq!(back.device_id, original.device_id);
    }

    /// `FileInfo` has 3 bare `DateTime<Local>` fields. Use distinct
    /// instants so a swapped adapter wiring shows up.
    #[test]
    fn file_info_round_trips_wincode_with_distinct_datetime_fields() {
        let original = FileInfo {
            name: "report.txt".to_string(),
            path: r"C:\Users\alice\report.txt".to_string(),
            size: 12_345,
            is_dir: false,
            is_file: true,
            is_symlink: false,
            permissions: 0o644,
            accessed: Local
                .with_ymd_and_hms(2026, 5, 10, 9, 0, 0)
                .single()
                .expect("valid local time"),
            created: Local
                .with_ymd_and_hms(2026, 1, 15, 14, 22, 33)
                .single()
                .expect("valid local time"),
            modified: Local
                .with_ymd_and_hms(2026, 5, 9, 18, 45, 0)
                .single()
                .expect("valid local time"),
            err_msg: None,
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: FileInfo = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.name, original.name);
        assert_eq!(back.size, original.size);
        assert_eq!(back.is_file, original.is_file);
        assert_eq!(back.accessed, original.accessed);
        assert_eq!(back.created, original.created);
        assert_eq!(back.modified, original.modified);
    }

    #[test]
    fn file_list_response_round_trips_wincode_with_two_entries() {
        let make_info = |name: &str, size: u64| FileInfo {
            name: name.to_string(),
            path: format!(r"C:\test\{}", name),
            size,
            is_dir: false,
            is_file: true,
            is_symlink: false,
            permissions: 0o644,
            accessed: Local
                .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
                .single()
                .expect("valid local time"),
            created: Local
                .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
                .single()
                .expect("valid local time"),
            modified: Local
                .with_ymd_and_hms(2026, 5, 10, 0, 0, 0)
                .single()
                .expect("valid local time"),
            err_msg: None,
        };
        let original = FileListResponse {
            file_info_list: vec![make_info("a.txt", 100), make_info("b.txt", 200)],
            total_count: 2,
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: FileListResponse = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.total_count, 2);
        assert_eq!(back.file_info_list.len(), 2);
        assert_eq!(back.file_info_list[0].name, "a.txt");
        assert_eq!(back.file_info_list[1].size, 200);
    }

    #[test]
    fn delete_file_request_round_trips_wincode() {
        let original = DeleteFileRequest {
            file_path: r"C:\tmp\junk.txt".to_string(),
            delete_permanently: Some(true),
            connection_id: Some("conn-del".to_string()),
            device_id: Some("42".to_string()),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: DeleteFileRequest = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.file_path, original.file_path);
        assert_eq!(back.delete_permanently, original.delete_permanently);
        assert_eq!(back.connection_id, original.connection_id);
        assert_eq!(back.device_id, original.device_id);
    }
}
