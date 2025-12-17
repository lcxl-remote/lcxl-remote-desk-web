use std::fmt::{Display, Formatter};

use actix_web::ResponseError;
use desk_utils::rest::{ErrorCode, RestResponse};

#[derive(Debug)]
pub enum DeskTurnError {
    /// An anyhow error occurred.
    AnyhowError(anyhow::Error),
    IllegalTransport(String),
}

impl Display for DeskTurnError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DeskTurnError::AnyhowError(err) => err.fmt(f),
            DeskTurnError::IllegalTransport(illegal_transport) => {
                write!(f, "Illegal transport: {}", illegal_transport)
            }
        }
    }
}

impl From<anyhow::Error> for DeskTurnError {
    fn from(err: anyhow::Error) -> Self {
        DeskTurnError::AnyhowError(err)
    }
}

impl ResponseError for DeskTurnError {
    fn status_code(&self) -> actix_web::http::StatusCode {
        actix_web::http::StatusCode::OK
    }

    fn error_response(&self) -> actix_web::HttpResponse<actix_web::body::BoxBody> {
        let error_code = ErrorCode::SYSTEM_ERROR;

        // write as json
        let rest = RestResponse::failed(error_code, self.to_string());
        actix_web::HttpResponse::Ok()
            .status(self.status_code())
            .json(rest)
    }
}
