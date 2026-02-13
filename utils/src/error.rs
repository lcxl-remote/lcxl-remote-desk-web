use std::fmt::{self, Display, Formatter};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeskErrorCode(pub i32);

impl DeskErrorCode {
    pub const SUCCESS: DeskErrorCode = DeskErrorCode(0);
    pub const SYSTEM_ERROR: DeskErrorCode = DeskErrorCode(1);
    pub const INVALID_STATE: DeskErrorCode = DeskErrorCode(2);
    pub const NOT_IMPLEMENTED_YET: DeskErrorCode = DeskErrorCode(3);
    pub const PERMISSION_ERROR: DeskErrorCode = DeskErrorCode(4);

    pub const FILE_PATH_NOT_FOUND: DeskErrorCode = DeskErrorCode(11);
    pub const NOT_ALLOW_DELETE_FILE: DeskErrorCode = DeskErrorCode(21);
    pub const FILE_CHANGED: DeskErrorCode = DeskErrorCode(22);

    pub const INVALID_PARAMS: DeskErrorCode = DeskErrorCode(5);
    pub const UNKNOWN_SIGNALING_TYPE: DeskErrorCode = DeskErrorCode(6);
    pub const REMOTE_DESK_OFFLINE: DeskErrorCode = DeskErrorCode(10003);
    pub const TIMEOUT: DeskErrorCode = DeskErrorCode(10004);

    pub const ACTION_NEED_RETRY: DeskErrorCode = DeskErrorCode(1001);

    pub const GENERATE_LOCAL_DESCRIPTION_FAILED: DeskErrorCode = DeskErrorCode(10001);
    pub const BLANK_SIGNALING_DATA: DeskErrorCode = DeskErrorCode(10002);
    pub fn new(code: i32) -> Self {
        DeskErrorCode(code)
    }
}

impl Display for DeskErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code_str = format!("{}", self.0);
        f.write_str(code_str.as_str())
    }
}

#[derive(Debug)]
pub struct CustomDeskError {
    pub error_code: DeskErrorCode,
    pub message: String,
}

impl fmt::Display for CustomDeskError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.write_str("Custom desk error")?;

        write!(fmt, "({}): {}", self.error_code, self.message)?;

        Ok(())
    }
}

impl CustomDeskError {
    pub fn new(error_code: DeskErrorCode, message: &str) -> CustomDeskError {
        CustomDeskError {
            error_code,
            message: message.to_string(),
        }
    }
}

#[derive(Debug)]
pub enum DeskUtilsError {
    /// A log parse level error occurred.
    ParseLevelError(log::ParseLevelError),
    /// A log set logger error occurred.
    SetLoggerError(log::SetLoggerError),
}

impl Display for DeskUtilsError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeskUtilsError::ParseLevelError(err) => err.fmt(f),
            DeskUtilsError::SetLoggerError(err) => err.fmt(f),
        }
    }
}

impl From<log::ParseLevelError> for DeskUtilsError {
    fn from(err: log::ParseLevelError) -> Self {
        DeskUtilsError::ParseLevelError(err)
    }
}

impl From<log::SetLoggerError> for DeskUtilsError {
    fn from(err: log::SetLoggerError) -> Self {
        DeskUtilsError::SetLoggerError(err)
    }
}
