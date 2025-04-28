use std::fmt::{self, Display};

use actix_web::ResponseError;

use crate::model::common::{ErrorCode, RestResponse};

#[derive(Debug)]
pub struct CustomDeskError {
    pub error_code: ErrorCode,
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
    pub fn new(error_code: ErrorCode, message: String) -> CustomDeskError {
        CustomDeskError {
            error_code,
            message,
        }
    }
}

/// Custom error type for the application.
#[derive(Debug)]
pub enum DeskError {
    /// An I/O error occurred.
    IoError(std::io::Error),
    /// A JSON serialization/deserialization error occurred.
    JsonError(serde_json::Error),
    /// A Rusqlite database error occurred.
    RusqliteError(rusqlite::Error),
    /// A configuration error occurred.
    ConfigError(config::ConfigError),
    /// A TOML edit error occurred.
    TomlEditError(toml_edit::TomlError),
    /// A TOML ser error occurred.
    TomlError(toml::ser::Error),
    /// A connection pool error occurred.
    R2d2Error(r2d2::Error),
    /// Desk custom error
    CustomError(CustomDeskError),
}

impl DeskError {
    pub fn custom_error<T>(error_code: ErrorCode, message: String) -> Result<T, DeskError> {
        Err(DeskError::CustomError(CustomDeskError::new(
            error_code, message,
        )))
    }
}

impl Display for DeskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeskError::IoError(error) => error.fmt(f),
            DeskError::JsonError(error) => error.fmt(f),
            DeskError::RusqliteError(error) => error.fmt(f),
            DeskError::ConfigError(error) => error.fmt(f),
            DeskError::TomlEditError(error) => error.fmt(f),
            DeskError::TomlError(error) => error.fmt(f),
            DeskError::R2d2Error(error) => error.fmt(f),
            DeskError::CustomError(error) => error.fmt(f),
        }
    }
}

impl From<std::io::Error> for DeskError {
    fn from(err: std::io::Error) -> Self {
        DeskError::IoError(err)
    }
}

impl From<serde_json::Error> for DeskError {
    fn from(err: serde_json::Error) -> Self {
        DeskError::JsonError(err)
    }
}

impl From<rusqlite::Error> for DeskError {
    fn from(err: rusqlite::Error) -> Self {
        DeskError::RusqliteError(err)
    }
}

impl From<config::ConfigError> for DeskError {
    fn from(err: config::ConfigError) -> Self {
        DeskError::ConfigError(err)
    }
}

impl From<toml_edit::TomlError> for DeskError {
    fn from(err: toml_edit::TomlError) -> Self {
        DeskError::TomlEditError(err)
    }
}

impl From<toml::ser::Error> for DeskError {
    fn from(err: toml::ser::Error) -> Self {
        DeskError::TomlError(err)
    }
}

impl From<r2d2::Error> for DeskError {
    fn from(err: r2d2::Error) -> Self {
        DeskError::R2d2Error(err)
    }
}

impl ResponseError for DeskError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        actix_web::http::StatusCode::OK
    }

    fn error_response(&self) -> actix_web::HttpResponse<actix_web::body::BoxBody> {
        let error_code = match self {
            DeskError::CustomError(error) => error.error_code,
            _ => ErrorCode::SYSTEM_ERROR,
        };
        // write as json
        let rest = RestResponse::failed(error_code, format!("{}", self));
        actix_web::HttpResponse::Ok()
            .status(self.status_code())
            .json(rest)
    }
}
