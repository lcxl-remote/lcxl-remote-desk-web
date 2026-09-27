//! Shared central-only task management transport, independent of device presence.
use super::ControlFrameOutcome;

/// Shared wire DTOs are explicitly included in both generated client schemas.
#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(
    desk_agent_protocol::schedule::management::ScheduleManagementRequest,
    desk_agent_protocol::schedule::management::ScheduleManagementResponse,
    ScheduleRetentionExceeded,
)))]
pub struct ScheduleManagementSchemas;
use crate::model::{
    auth_context::{AuthContext, AuthKind},
    connection::ConnectionState,
    signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
};
use desk_agent_protocol::schedule::management::ScheduleManagementResponse;
use desk_utils::error::DeskErrorCode;

pub fn cookie_owner(auth: &AuthContext) -> Option<i32> {
    (auth.auth_kind == AuthKind::CookieAuth && auth.remote_desk_type == RemoteDeskTypeEnum::Browser)
        .then_some(auth.user_id)
        .flatten()
        .filter(|id| *id > 0)
}

/// A failed task management request. `data` carries structured detail for
/// codes that need it, e.g. the accepted maximum delay for
/// `SCHEDULE_EXCEEDS_SESSION_RETENTION`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleManagementFailure {
    pub code: DeskErrorCode,
    pub message: &'static str,
    pub data: Option<serde_json::Value>,
}

impl From<(DeskErrorCode, &'static str)> for ScheduleManagementFailure {
    fn from((code, message): (DeskErrorCode, &'static str)) -> Self {
        Self {
            code,
            message,
            data: None,
        }
    }
}

/// Error detail of `SCHEDULE_EXCEEDS_SESSION_RETENTION`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRetentionExceeded {
    /// Longest delay from now, in seconds, a conversation timer may use.
    pub max_delay_seconds: u64,
}

impl ScheduleManagementFailure {
    pub fn exceeds_session_retention(max_delay_seconds: u64) -> Self {
        Self {
            code: DeskErrorCode::SCHEDULE_EXCEEDS_SESSION_RETENTION,
            message: "the continuation would run after its conversation expires",
            data: serde_json::to_value(ScheduleRetentionExceeded { max_delay_seconds }).ok(),
        }
    }
}

pub async fn reply(
    actor: &ConnectionState,
    request: &SignalingModel,
    result: Result<ScheduleManagementResponse, ScheduleManagementFailure>,
) -> ControlFrameOutcome {
    let frame = match result {
        Ok(data) => SignalingModel::success_response(
            &request.request_id,
            SignalingType::ScheduledTasksManaged,
            None,
            Some(actor.model.connection_id.clone()),
            Some(&data),
        ),
        Err(failure) => SignalingModel::new_response(
            &request.request_id,
            SignalingType::ScheduledTasksManaged,
            None,
            Some(actor.model.connection_id.clone()),
            failure.data.as_ref(),
            crate::model::signal::SignalingResponseState {
                error_code: failure.code.code(),
                message: Some(failure.message.to_string()),
            },
        ),
    };
    if let Ok(frame) = frame
        && let Ok(text) = serde_json::to_string(&frame)
    {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            actor.session.write().await.text(text).await
        })
        .await;
    }
    ControlFrameOutcome::Handled
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_positive_cookie_browser_identity_can_manage_tasks() {
        assert_eq!(
            cookie_owner(&AuthContext::cookie(1, RemoteDeskTypeEnum::Browser)),
            Some(1)
        );
        for auth in [
            AuthContext::cookie(0, RemoteDeskTypeEnum::Browser),
            AuthContext::cookie(1, RemoteDeskTypeEnum::Server),
            AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Browser),
            AuthContext::anonymous(RemoteDeskTypeEnum::Browser),
        ] {
            assert_eq!(cookie_owner(&auth), None);
        }
    }
}
