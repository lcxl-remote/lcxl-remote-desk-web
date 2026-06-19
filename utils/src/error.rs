use std::fmt::{self, Display, Formatter};

pub fn format_backtrace<E: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    backtrace: &std::backtrace::Backtrace,
    error: &E,
) -> fmt::Result {
    if matches!(
        backtrace.status(),
        std::backtrace::BacktraceStatus::Captured
    ) {
        write!(f, "{}\n{}", error, backtrace)
    } else {
        error.fmt(f)
    }
}

pub fn format_debug_backtrace<E: fmt::Debug>(
    f: &mut fmt::Formatter<'_>,
    backtrace: &std::backtrace::Backtrace,
    error: &E,
) -> fmt::Result {
    if matches!(
        backtrace.status(),
        std::backtrace::BacktraceStatus::Captured
    ) {
        write!(f, "{:?}\n{}", error, backtrace)
    } else {
        f.write_fmt(format_args!("{:?}", error))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeskErrorCode(i32);

impl DeskErrorCode {
    pub const SUCCESS: DeskErrorCode = DeskErrorCode(0);
    pub const SYSTEM_ERROR: DeskErrorCode = DeskErrorCode(1);
    pub const INVALID_STATE: DeskErrorCode = DeskErrorCode(2);
    pub const NOT_IMPLEMENTED_YET: DeskErrorCode = DeskErrorCode(3);
    pub const PERMISSION_ERROR: DeskErrorCode = DeskErrorCode(4);
    pub const INVALID_PARAMS: DeskErrorCode = DeskErrorCode(5);
    pub const UNKNOWN_SIGNALING_TYPE: DeskErrorCode = DeskErrorCode(6);
    /// The requested feature/backend is structurally unavailable in the
    /// current process or desktop context (e.g. Windows.Graphics.Capture
    /// under the SYSTEM token / Winlogon desktop, where RuntimeBroker is
    /// not running). Callers may transparently fall back to an
    /// alternative implementation instead of surfacing this as a hard
    /// error.
    pub const FEATURE_UNAVAILABLE: DeskErrorCode = DeskErrorCode(7);
    /// The request is structurally valid but rejected because a
    /// hard precondition is unmet (e.g. the caller asked the daemon
    /// to enable the virtual display but the IDD driver is not
    /// staged). Use this when the right resolution is "make the
    /// precondition true and retry," not "fix the request body."
    pub const PRECONDITION_FAILED: DeskErrorCode = DeskErrorCode(8);

    pub const FILE_PATH_NOT_FOUND: DeskErrorCode = DeskErrorCode(11);
    pub const CLIENT_ID_NOT_FOUND: DeskErrorCode = DeskErrorCode(12);
    /// Optimistic-concurrency conflict: a write supplied an `expected_revision`
    /// that no longer matches the current persisted revision (another writer or
    /// instance committed in between). The caller should re-read the current
    /// revision/value — returned in the response payload — and retry. This is a
    /// business-level outcome carried in the `RestResponse.code`, never an HTTP
    /// status code.
    pub const REVISION_CONFLICT: DeskErrorCode = DeskErrorCode(13);
    /// A fleet (multi-device) request resolved to zero diagnosable targets after
    /// applying the caller's policy visibility. Returned uniformly whether the
    /// selector matched nothing or every match was policy-invisible, so it leaks
    /// no information about devices the caller cannot see. Carried in
    /// `RestResponse.code`, never an HTTP status.
    pub const NO_VISIBLE_TARGETS: DeskErrorCode = DeskErrorCode(14);

    pub const NOT_ALLOW_DELETE_FILE: DeskErrorCode = DeskErrorCode(21);
    pub const FILE_CHANGED: DeskErrorCode = DeskErrorCode(22);

    pub const ACTION_NEED_RETRY: DeskErrorCode = DeskErrorCode(1001);

    pub const REMOTE_DESK_OFFLINE: DeskErrorCode = DeskErrorCode(10003);
    pub const TIMEOUT: DeskErrorCode = DeskErrorCode(10004);
    pub const SESSION_NOT_FOUND: DeskErrorCode = DeskErrorCode(10005);

    pub const GENERATE_LOCAL_DESCRIPTION_FAILED: DeskErrorCode = DeskErrorCode(10001);
    pub const BLANK_SIGNALING_DATA: DeskErrorCode = DeskErrorCode(10002);
    pub const AUTO_START_ERROR: DeskErrorCode = DeskErrorCode(10006);

    // for windows platform
    /// windows error code
    pub const WINDOWS_ERROR: DeskErrorCode = DeskErrorCode(100001);

    // for linux platform
    /// linux error code
    pub const LINUX_ERROR: DeskErrorCode = DeskErrorCode(200001);

    // for mac platform
    /// mac error code
    pub const MAC_ERROR: DeskErrorCode = DeskErrorCode(300001);

    pub fn new(code: i32) -> Self {
        DeskErrorCode(code)
    }

    pub fn code(&self) -> i32 {
        self.0
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

#[cfg(test)]
mod tests {
    use super::DeskErrorCode;

    /// Lock the numeric wire value of `REVISION_CONFLICT`. Clients and the
    /// manager both branch on this exact code, so the value is a contract and
    /// must not drift.
    #[test]
    fn revision_conflict_code_is_stable() {
        assert_eq!(DeskErrorCode::REVISION_CONFLICT.code(), 13);
    }

    /// The new code must not collide with any existing assignment around it.
    #[test]
    fn revision_conflict_code_is_distinct() {
        let others = [
            DeskErrorCode::PRECONDITION_FAILED.code(),
            DeskErrorCode::FILE_PATH_NOT_FOUND.code(),
            DeskErrorCode::CLIENT_ID_NOT_FOUND.code(),
            DeskErrorCode::NOT_ALLOW_DELETE_FILE.code(),
        ];
        assert!(!others.contains(&DeskErrorCode::REVISION_CONFLICT.code()));
    }

    /// Lock the numeric wire value of `NO_VISIBLE_TARGETS`. The manager returns it
    /// and the console branches on it, so the value is a contract.
    #[test]
    fn no_visible_targets_code_is_stable_and_distinct() {
        assert_eq!(DeskErrorCode::NO_VISIBLE_TARGETS.code(), 14);
        let others = [
            DeskErrorCode::REVISION_CONFLICT.code(),
            DeskErrorCode::PRECONDITION_FAILED.code(),
            DeskErrorCode::FILE_PATH_NOT_FOUND.code(),
            DeskErrorCode::CLIENT_ID_NOT_FOUND.code(),
        ];
        assert!(!others.contains(&DeskErrorCode::NO_VISIBLE_TARGETS.code()));
    }
}
