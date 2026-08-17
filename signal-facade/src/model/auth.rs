use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::startup_mode::StartupMode;

/// Marker used only to describe an envelope whose `data` member is null.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EmptyResponseDto {}

/// Account credentials submitted to the current service.
///
/// Concrete identifier rules are documented by each service's login operation.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LoginRequest {
    /// Account identifier interpreted by the receiving service.
    pub username: String,
    pub password: String,
    /// Optional human-verification token. Services without CAPTCHA ignore it.
    pub captcha_token: Option<String>,
}

/// Login metadata shared by manager and standalone-server control clients.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LoginOutcomeDto {
    pub captcha_required: Option<bool>,
    pub retry_after_sec: Option<u64>,
    pub api_version: Option<i32>,
    pub startup_mode: Option<StartupMode>,
    /// Masked address the verification mail was sent to, carried only when the
    /// login was rejected because the account is still awaiting email
    /// verification. Services without email verification always leave it null,
    /// and clients must also tolerate the member being absent entirely.
    #[serde(default)]
    pub email_masked: Option<String>,
}

/// Verify the current credentials and optionally replace either credential.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateCredentialsRequest {
    pub current_username: String,
    pub current_password: String,
    pub new_username: Option<String>,
    pub new_password: Option<String>,
}

/// Public current-user projection. Session and signaling identities stay
/// private to their owning services.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CurrentUserDto {
    pub user_id: Option<i32>,
    pub name: String,
    pub avatar: Option<String>,
    pub email: Option<String>,
    pub access: Option<String>,
    pub target_connection_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_wire_fields_are_snake_case_and_nullable() {
        let encoded = serde_json::to_value(LoginOutcomeDto {
            captcha_required: None,
            retry_after_sec: None,
            api_version: Some(7),
            startup_mode: None,
            email_masked: None,
        })
        .unwrap();

        assert_eq!(encoded["captcha_required"], serde_json::Value::Null);
        assert_eq!(encoded["retry_after_sec"], serde_json::Value::Null);
        assert_eq!(encoded["startup_mode"], serde_json::Value::Null);
        assert_eq!(encoded["email_masked"], serde_json::Value::Null);
        assert!(encoded.get("captchaRequired").is_none());
        assert!(encoded.get("retryAfterSec").is_none());
        assert!(encoded.get("emailMasked").is_none());
    }

    #[test]
    fn login_outcome_decodes_without_service_specific_members() {
        // A control client talks to both the manager and the standalone signaling
        // server; members only one of them emits must decode as absent, not fail.
        let decoded: LoginOutcomeDto = serde_json::from_str("{}").unwrap();

        assert!(decoded.captcha_required.is_none());
        assert!(decoded.retry_after_sec.is_none());
        assert!(decoded.api_version.is_none());
        assert!(decoded.startup_mode.is_none());
        assert!(decoded.email_masked.is_none());
    }

    #[test]
    fn login_outcome_carries_masked_email_verbatim() {
        let decoded: LoginOutcomeDto =
            serde_json::from_str(r#"{"email_masked":"a***@ex****.com"}"#).unwrap();

        assert_eq!(decoded.email_masked.as_deref(), Some("a***@ex****.com"));
    }

    #[test]
    fn current_user_wire_does_not_expose_template_fields() {
        let encoded = serde_json::to_value(CurrentUserDto {
            user_id: None,
            name: "admin".to_string(),
            avatar: None,
            email: None,
            access: Some("admin".to_string()),
            target_connection_id: None,
        })
        .unwrap();

        assert_eq!(encoded["user_id"], serde_json::Value::Null);
        assert!(encoded.get("userid").is_none());
        assert!(encoded.get("notifyCount").is_none());
        assert!(encoded.get("unreadCount").is_none());
    }
}
