use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// Query parameters for listing files.
#[derive(Clone, Debug, Deserialize, Serialize, IntoParams, ToSchema)]
pub struct ListSettings {
    /// Page number, start from 1
    pub page_no: i64,
    /// Page count, must be greater than 0
    pub page_count: i64,
    /// Minimum file size
    pub min_file_size: Option<i64>,
    /// Max file size
    pub max_file_size: Option<i64>,
    /// Dir path of the directory containing the file
    pub dir_path: Option<String>,
    /// File name filtering
    pub file_name: Option<String>,
    /// New field for file extension filtering
    pub file_extension: Option<String>,
    /// Optional file extension list filtering, comma(,) separated values.
    pub file_extension_list: Option<String>,
    /// MD5 hash of the file content, used for filtering files by their content.
    pub md5: Option<String>,
    /// Optional time range filter for file creation.
    pub start_created_time: Option<DateTime<Local>>,
    pub end_created_time: Option<DateTime<Local>>,
    /// Optional time range filter for file modification.
    pub start_modified_time: Option<DateTime<Local>>,
    pub end_modified_time: Option<DateTime<Local>>,

    /// Minimum file md5 count
    pub min_md5_count: Option<i64>,
    /// Max file md5 count
    pub max_md5_count: Option<i64>,
    /// Optional order by field.
    pub order_by: Option<String>,
    /// Optional order direction, true for ascending, false for descending. Default is descending.
    pub order_asc: Option<bool>,

    /// Optional filter for duplicate files in a specific directory path. If set, if files within this directory duplicate those outside of it, they will be displayed.
    pub filter_dup_file_by_dir_path: Option<bool>,
}

impl Default for ListSettings {
    fn default() -> Self {
        Self {
            page_no: 1,
            page_count: 20,
            min_file_size: None,
            max_file_size: None,
            dir_path: None,
            file_name: None,
            file_extension: None,
            file_extension_list: None,
            md5: None,
            start_created_time: None,
            end_created_time: None,
            start_modified_time: None,
            end_modified_time: None,
            min_md5_count: Some(2),
            max_md5_count: None,
            order_by: None,
            order_asc: None,
            filter_dup_file_by_dir_path: None,
        }
    }
}
