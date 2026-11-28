use std::{fs::Metadata, path::PathBuf};

use jiff::Timestamp;
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

    /// Prefer the selected worker user's home for an empty initial path.
    /// If it cannot be resolved or listed, use the normal root entry instead.
    #[serde(default)]
    pub prefer_user_home: bool,

    /// Return only directories, filtered and sorted before pagination.
    #[serde(default)]
    pub directories_only: bool,

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
    #[schema(value_type = Option<String>, format = DateTime)]
    #[param(value_type = Option<String>, format = DateTime)]
    #[serde(serialize_with = "serialize_optional_local_time")]
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub start_created_time: Option<Timestamp>,
    #[schema(value_type = Option<String>, format = DateTime)]
    #[param(value_type = Option<String>, format = DateTime)]
    #[serde(serialize_with = "serialize_optional_local_time")]
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub end_created_time: Option<Timestamp>,
    /// Optional time range filter for file modification.
    #[schema(value_type = Option<String>, format = DateTime)]
    #[param(value_type = Option<String>, format = DateTime)]
    #[serde(serialize_with = "serialize_optional_local_time")]
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub start_modified_time: Option<Timestamp>,
    #[schema(value_type = Option<String>, format = DateTime)]
    #[param(value_type = Option<String>, format = DateTime)]
    #[serde(serialize_with = "serialize_optional_local_time")]
    #[wincode(with = "Option<DateTimeLocalWincode>")]
    pub end_modified_time: Option<Timestamp>,
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
    #[schema(value_type = String, format = DateTime)]
    #[serde(serialize_with = "serialize_local_time")]
    #[wincode(with = "DateTimeLocalWincode")]
    pub accessed: Timestamp,
    #[schema(value_type = String, format = DateTime)]
    #[serde(serialize_with = "serialize_local_time")]
    #[wincode(with = "DateTimeLocalWincode")]
    pub created: Timestamp,
    #[schema(value_type = String, format = DateTime)]
    #[serde(serialize_with = "serialize_local_time")]
    #[wincode(with = "DateTimeLocalWincode")]
    pub modified: Timestamp,
    pub err_msg: Option<String>,
    /// The entry is a directory whose path round-trips losslessly, so a
    /// controller may navigate into it.
    #[serde(default)]
    pub can_open: bool,
    /// The file can be downloaded: readable by the host user and file
    /// transfer is not denied for this connection. Filled by file-manager
    /// listings only.
    #[serde(default)]
    pub can_read: bool,
    /// The entry itself can be deleted by the host user and deletion is not
    /// denied for this connection. Filled by file-manager listings only.
    #[serde(default)]
    pub can_delete: bool,
}

fn local_wire_time(time: Timestamp) -> String {
    let offset = jiff::tz::TimeZone::system().to_offset(time);
    let text = desk_utils::datetime::rfc3339_at(time, offset);
    if let Some(utc) = text.strip_suffix("+00:00") {
        format!("{utc}Z")
    } else {
        text
    }
}

fn serialize_local_time<S: serde::Serializer>(
    time: &Timestamp,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&local_wire_time(*time))
}

fn serialize_optional_local_time<S: serde::Serializer>(
    time: &Option<Timestamp>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    time.map(local_wire_time).serialize(serializer)
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
            accessed: Timestamp::UNIX_EPOCH,
            created: Timestamp::UNIX_EPOCH,
            modified: Timestamp::UNIX_EPOCH,
            err_msg: None,
            can_open: false,
            can_read: false,
            can_delete: false,
        };
        match metadata {
            Ok(metadata) => {
                file_info.size = metadata.len();
                file_info.is_dir = metadata.is_dir();
                file_info.is_file = metadata.is_file();
                file_info.is_symlink = metadata.is_symlink();
                // A lossy path could name a different entry, so only a
                // directory whose path converts exactly may be opened.
                file_info.can_open = file_info.is_dir && path.to_str().is_some();
                file_info.permissions = FileInfo::get_permissions(&metadata);
                match metadata
                    .accessed()
                    .and_then(|time| Timestamp::try_from(time).map_err(std::io::Error::other))
                {
                    Ok(accessed) => file_info.accessed = accessed,
                    Err(err) => {
                        file_info.add_err_msg(format!("Failed to get file accessed time: {}", err))
                    }
                }
                match metadata
                    .created()
                    .and_then(|time| Timestamp::try_from(time).map_err(std::io::Error::other))
                {
                    Ok(created) => file_info.created = created,
                    Err(err) => {
                        file_info.add_err_msg(format!("Failed to get file created time: {}", err))
                    }
                }
                match metadata
                    .modified()
                    .and_then(|time| Timestamp::try_from(time).map_err(std::io::Error::other))
                {
                    Ok(modified) => file_info.modified = modified,
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
    /// Effective path used for this listing; empty denotes the root entry.
    pub path: String,
    pub file_info_list: Vec<FileInfo>,
    pub total_count: i64,
    /// The listed directory accepts uploads and new folders: writable by the
    /// host user and file transfer is not denied for this connection.
    #[serde(default)]
    pub can_write: bool,
    /// Deleting an entry of this directory can probably move it to the trash.
    /// `false` only when the host has clear evidence that it cannot.
    #[serde(default)]
    pub trash_available: bool,
}

/// Request body for creating one directory.
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
pub struct CreateDirectoryRequest {
    /// Directory that receives the new entry, as returned by a listing.
    pub parent_path: String,
    /// Name of the new directory; a single path component.
    pub name: String,
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
}

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    /// `FileListParams` carries 4 `Option<Timestamp>` fields
    /// that ride `DateTimeLocalWincode` via `#[wincode(with =
    /// "Option<...>")]`. Construct a payload with mixed `Some` /
    /// `None` plus distinct timestamps on each field, so a wrong
    /// adapter wiring (e.g. all four routed to the same instant)
    /// would surface as a value mismatch on decode.
    #[test]
    fn directory_filter_defaults_off_and_round_trips_json() {
        let value = serde_json::json!({"path":"/","page_no":1,"page_count":100});
        let mut params: FileListParams = serde_json::from_value(value).unwrap();
        assert!(!params.directories_only);
        assert!(!params.prefer_user_home);
        params.directories_only = true;
        params.prefer_user_home = true;
        let restored: FileListParams =
            serde_json::from_str(&serde_json::to_string(&params).unwrap()).unwrap();
        assert!(restored.directories_only);
        assert!(restored.prefer_user_home);
    }

    #[test]
    fn file_time_filters_keep_rfc3339_precision_and_openapi_format() {
        let instant: Timestamp = "2026-01-15T00:00:00.100Z".parse().unwrap();
        let params = FileListParams {
            start_created_time: Some(instant),
            ..Default::default()
        };
        let json = serde_json::to_value(params).unwrap();
        let time = json["start_created_time"].as_str().unwrap();
        assert!(time.contains(".100"));
        assert_eq!(desk_utils::datetime::parse_rfc3339(time).unwrap(), instant);
        assert!(json["end_created_time"].is_null());
        let schema =
            serde_json::to_value(<FileListParams as utoipa::PartialSchema>::schema()).unwrap();
        assert_eq!(
            schema["properties"]["start_created_time"]["format"],
            "date-time"
        );
    }

    #[test]
    fn file_list_params_round_trips_wincode_with_mixed_datetime_fields() {
        let original = FileListParams {
            path: r"C:\Users".to_string(),
            page_no: 2,
            page_count: 50,
            prefer_user_home: true,
            directories_only: true,
            min_file_size: Some(1024),
            max_file_size: None,
            file_name: Some("readme".to_string()),
            file_extension: Some("md".to_string()),
            file_extension_list: Some("md,txt".to_string()),
            start_created_time: Some("2024-01-01T00:00:00Z".parse::<Timestamp>().unwrap()),
            end_created_time: None,
            start_modified_time: Some("2025-06-15T12:30:00Z".parse::<Timestamp>().unwrap()),
            end_modified_time: Some("2026-05-10T23:59:59Z".parse::<Timestamp>().unwrap()),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: FileListParams = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.path, original.path);
        assert!(back.prefer_user_home);
        assert_eq!(back.page_no, original.page_no);
        assert_eq!(back.page_count, original.page_count);
        assert_eq!(back.directories_only, original.directories_only);
        assert_eq!(back.min_file_size, original.min_file_size);
        assert_eq!(back.max_file_size, original.max_file_size);
        assert_eq!(back.file_name, original.file_name);
        assert_eq!(back.start_created_time, original.start_created_time);
        assert_eq!(back.end_created_time, original.end_created_time);
        assert_eq!(back.start_modified_time, original.start_modified_time);
        assert_eq!(back.end_modified_time, original.end_modified_time);
    }

    /// `FileInfo` has 3 bare `Timestamp` fields. Use distinct
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
            accessed: "2026-05-10T09:00:00Z".parse::<Timestamp>().unwrap(),
            created: "2026-01-15T14:22:33Z".parse::<Timestamp>().unwrap(),
            modified: "2026-05-09T18:45:00Z".parse::<Timestamp>().unwrap(),
            err_msg: None,
            can_open: false,
            can_read: true,
            can_delete: true,
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
        assert!(!back.can_open);
        assert!(back.can_read);
        assert!(back.can_delete);
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
            accessed: "2026-05-10T00:00:00Z".parse::<Timestamp>().unwrap(),
            created: "2026-05-10T00:00:00Z".parse::<Timestamp>().unwrap(),
            modified: "2026-05-10T00:00:00Z".parse::<Timestamp>().unwrap(),
            err_msg: None,
            can_open: false,
            can_read: true,
            can_delete: false,
        };
        let original = FileListResponse {
            path: "/home/user".to_string(),
            file_info_list: vec![make_info("a.txt", 100), make_info("b.txt", 200)],
            total_count: 2,
            can_write: true,
            trash_available: false,
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: FileListResponse = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.total_count, 2);
        assert_eq!(back.path, original.path);
        assert_eq!(back.file_info_list.len(), 2);
        assert_eq!(back.file_info_list[0].name, "a.txt");
        assert_eq!(back.file_info_list[1].size, 200);
        assert!(back.can_write);
        assert!(!back.trash_available);
    }

    #[test]
    fn capability_flags_default_off_when_absent_from_json() {
        let info: FileInfo = serde_json::from_value(serde_json::json!({
            "name": "a", "path": "/a", "size": 0, "is_dir": true, "is_file": false,
            "is_symlink": false, "permissions": 0, "accessed": "2026-01-01T00:00:00Z",
            "created": "2026-01-01T00:00:00Z", "modified": "2026-01-01T00:00:00Z",
            "err_msg": null
        }))
        .unwrap();
        assert!(!info.can_open && !info.can_read && !info.can_delete);
        let list: FileListResponse = serde_json::from_value(
            serde_json::json!({"path": "", "file_info_list": [], "total_count": 0}),
        )
        .unwrap();
        assert!(!list.can_write && !list.trash_available);
    }

    #[test]
    fn new_marks_only_lossless_directories_openable() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(FileInfo::new(dir.path().to_path_buf()).unwrap().can_open);
        assert!(!FileInfo::new(file).unwrap().can_open);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let lossy = dir.path().join(std::ffi::OsStr::from_bytes(b"bad\xff"));
            if std::fs::create_dir(&lossy).is_ok() {
                let info = FileInfo::new(lossy).unwrap();
                assert!(info.is_dir);
                assert!(!info.can_open);
            }
        }
    }

    #[test]
    fn create_directory_request_round_trips_wincode() {
        let original = CreateDirectoryRequest {
            parent_path: "/srv".to_string(),
            name: "new".to_string(),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: CreateDirectoryRequest =
            wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.parent_path, original.parent_path);
        assert_eq!(back.name, original.name);
    }

    #[test]
    fn delete_file_request_round_trips_wincode() {
        let original = DeleteFileRequest {
            file_path: r"C:\tmp\junk.txt".to_string(),
            delete_permanently: Some(true),
        };
        let config = unbounded_config();
        let bytes = wincode::config::serialize(&original, config).expect("encode");
        let back: DeleteFileRequest = wincode::config::deserialize(&bytes, config).expect("decode");
        assert_eq!(back.file_path, original.file_path);
        assert_eq!(back.delete_permanently, original.delete_permanently);
    }
}
