use std::fmt::Display;

use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
pub struct RestResponse<T: ToSchema> {
    success: bool,
    code: i32,
    message: Option<String>,
    data: Option<T>,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErrorCode(pub i32);

impl ErrorCode {
    pub const SUCCESS: ErrorCode = ErrorCode(0);
    pub const SYSTEM_ERROR: ErrorCode = ErrorCode(1);
    pub const INVALID_STATE: ErrorCode = ErrorCode(2);
    pub const NOT_IMPLEMENTED_YET: ErrorCode = ErrorCode(3);

    pub const FILE_PATH_NOT_FOUND: ErrorCode = ErrorCode(11);
    pub const NOT_ALLOW_DELETE_FILE: ErrorCode = ErrorCode(21);
    pub const FILE_CHANGED: ErrorCode = ErrorCode(22);

    pub const WINDOWS_ERROR: ErrorCode = ErrorCode(1000);

    pub const ACTION_NEED_RETRY: ErrorCode = ErrorCode(1001);

    pub const GENERATE_LOCAL_DESCRIPTION_FAILED: ErrorCode = ErrorCode(10001);
    pub const BLANK_SIGNALING_DATA: ErrorCode = ErrorCode(10002);
    pub fn new(code: i32) -> Self {
        ErrorCode(code)
    }
}

impl Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code_str = format!("{}", self.0);
        f.write_str(code_str.as_str())
    }
}

impl RestResponse<()> {
    pub fn succeed() -> Self {
        RestResponse {
            success: true,
            code: ErrorCode::SUCCESS.0,
            message: None,
            data: None,
        }
    }

    pub fn succeed_with_message(message: String) -> Self {
        RestResponse {
            success: true,
            code: ErrorCode::SUCCESS.0,
            message: Some(message),
            data: None,
        }
    }

    pub fn failed(error_code: ErrorCode, message: String) -> Self {
        RestResponse {
            success: false,
            code: error_code.0,
            message: Some(message),
            data: None,
        }
    }
}

impl<T: ToSchema> RestResponse<T> {
    pub fn succeed_with_data(data: T) -> Self {
        RestResponse {
            success: true,
            code: ErrorCode::SUCCESS.0,
            message: None,
            data: Some(data),
        }
    }

    pub fn failed_with_data(
        error_code: ErrorCode,
        message: Option<String>,
        data: Option<T>,
    ) -> Self {
        RestResponse {
            success: false,
            code: error_code.0,
            message,
            data,
        }
    }
}
