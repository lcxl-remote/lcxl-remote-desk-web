//! Identical owner HTTP request shapes for OSS and Manager.
use desk_agent_protocol::file_recovery::FileRecoveryRequest;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Download errors remain typed JSON, never an empty or mislabeled ZIP.
pub fn download_failure(
    reason: desk_agent_protocol::file_recovery::FileRecoveryFailure,
) -> actix_web::HttpResponse {
    actix_web::HttpResponse::Ok()
        .insert_header((actix_web::http::header::CACHE_CONTROL, "no-store"))
        .insert_header(("x-content-type-options", "nosniff"))
        .json(desk_utils::rest::RestResponse::failed_with_data(
            desk_utils::error::DeskErrorCode::PRECONDITION_FAILED,
            Some("File backup export failed".into()),
            Some(reason),
        ))
}

/// Preserve transport failure categories without exposing connection internals.
pub fn download_request_failure(
    error: crate::service::file_recovery::RecoveryRequestError,
) -> actix_web::HttpResponse {
    use crate::service::file_recovery::RecoveryRequestError;
    use desk_agent_protocol::file_recovery::FileRecoveryFailure;
    download_failure(match error {
        RecoveryRequestError::Offline | RecoveryRequestError::Timeout => {
            FileRecoveryFailure::WorkerUnavailable
        }
        RecoveryRequestError::Capacity => FileRecoveryFailure::Busy,
        RecoveryRequestError::InvalidRequest => FileRecoveryFailure::InvalidRequest,
        RecoveryRequestError::InvalidReply => FileRecoveryFailure::StorageUnavailable,
    })
}

#[cfg(test)]
mod tests {
    #[actix_web::test]
    async fn download_transport_errors_are_typed_and_never_zip() {
        use crate::service::file_recovery::RecoveryRequestError::*;
        for (error, expected) in [
            (Offline, "worker_unavailable"),
            (Timeout, "worker_unavailable"),
            (Capacity, "busy"),
            (InvalidRequest, "invalid_request"),
            (InvalidReply, "storage_unavailable"),
        ] {
            let response = super::download_request_failure(error);
            assert!(response.headers().get("content-disposition").is_none());
            let bytes = actix_web::body::to_bytes(response.into_body())
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["success"], false);
            assert_eq!(body["data"], expected);
        }
    }
    #[actix_web::test]
    async fn download_failure_preserves_category_without_zip_headers() {
        let response = super::download_failure(
            desk_agent_protocol::file_recovery::FileRecoveryFailure::ClockChanged,
        );
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json"
        );
        assert!(response.headers().get("content-disposition").is_none());
        let bytes = actix_web::body::to_bytes(response.into_body())
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["success"], false);
        assert_eq!(body["data"], "clock_changed");
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryManagementBody {
    pub connection: String,
    /// Manager public device handle. OSS resolves the authenticated connection.
    pub device_id: Option<String>,
    pub request: FileRecoveryRequest,
}
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryDownloadBody {
    pub connection: String,
    pub device_id: Option<String>,
    pub conversation_id: String,
    pub recovery_id: String,
    pub expected_authority: Option<String>,
    pub expected_os_user: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryCleanupQuery {
    pub after: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct FileRecoveryCleanupStatus {
    pub conversation_id: String,
    pub created_at_unix_ms: i64,
    pub next_attempt_at_unix_ms: i64,
    pub attempts: i64,
    /// A fixed, user-readable category; never a remote raw error or filesystem path.
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct FileRecoveryCleanupPage {
    pub records: Vec<FileRecoveryCleanupStatus>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FileRecoveryCleanupRetryBody {
    pub conversation_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct FileRecoveryCleanupRetryResult {
    /// The queue was made due. This is not confirmation of filesystem cleanup.
    pub scheduled: bool,
}

pub fn cleanup_reason(value: Option<&str>) -> String {
    match value {
        Some("offline" | "timeout" | "identity_changed" | "cleanup_pending" | "unavailable") => {
            value.unwrap().to_owned()
        }
        _ => "waiting".to_owned(),
    }
}
