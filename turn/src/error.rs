use std::fmt::{Display, Formatter};

use actix_web::ResponseError;
use desk_utils::{error::DeskErrorCode, rest::RestResponse};

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

impl DeskTurnError {
    /// Business code carried in the response envelope.
    ///
    /// Per variant rather than one blanket value: a transport or address the
    /// caller wrote that does not parse is the caller's to fix, and telling them
    /// the server failed sends them looking in the wrong place.
    pub fn error_code(&self) -> DeskErrorCode {
        match self {
            DeskTurnError::AnyhowError(_) => DeskErrorCode::SYSTEM_ERROR,
            DeskTurnError::IllegalTransport(_) => DeskErrorCode::INVALID_PARAMS,
        }
    }
}

impl ResponseError for DeskTurnError {
    /// Always `200`: the HTTP status carries transport semantics only, and the
    /// business outcome is in the envelope's `code`.
    fn status_code(&self) -> actix_web::http::StatusCode {
        actix_web::http::StatusCode::OK
    }

    fn error_response(&self) -> actix_web::HttpResponse<actix_web::body::BoxBody> {
        // write as json
        let rest = RestResponse::failed(self.error_code(), self.to_string());
        actix_web::HttpResponse::Ok()
            .status(self.status_code())
            .json(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the caller typed is the caller's problem to fix; anything else is
    /// the server's. Reporting both as a server failure sends an operator
    /// looking at logs for a typo.
    #[test]
    fn an_unparseable_input_is_a_parameter_error_not_a_server_failure() {
        assert_eq!(
            DeskTurnError::IllegalTransport("sctp".into()).error_code(),
            DeskErrorCode::INVALID_PARAMS
        );
        assert_eq!(
            DeskTurnError::AnyhowError(anyhow::anyhow!("socket is gone")).error_code(),
            DeskErrorCode::SYSTEM_ERROR
        );
    }

    /// The status code says nothing about the business outcome, whichever
    /// failure it is.
    #[test]
    fn every_failure_answers_with_http_200() {
        assert_eq!(
            DeskTurnError::IllegalTransport("sctp".into()).status_code(),
            actix_web::http::StatusCode::OK
        );
        assert_eq!(
            DeskTurnError::AnyhowError(anyhow::anyhow!("boom")).status_code(),
            actix_web::http::StatusCode::OK
        );
    }
}
