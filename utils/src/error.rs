use std::fmt::{Display, Formatter};

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
