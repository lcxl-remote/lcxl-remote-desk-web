use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Login params
#[derive(Serialize, Deserialize, ToSchema)]
pub struct LoginParams {
    pub username: String,
    pub password: String,
    #[serde(rename = "autoLogin")]
    #[schema(rename = "autoLogin")]
    pub auto_login: bool,
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub login_type: String,
}

#[derive(Deserialize, ToSchema)]
pub struct FakeCaptchaParams {
    pub phone: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct FakeCaptcha {
    pub code: Option<u32>,
    pub status: Option<String>,
}

/// Login result
#[derive(Serialize, Deserialize, Clone, Debug, ToSchema)]
pub struct LoginResult {
    pub status: String,
    #[serde(rename = "type")]
    #[schema(rename = "type")]
    pub login_type: String,
    #[serde(rename = "currentAuthority")]
    #[schema(rename = "currentAuthority")]
    pub current_authority: String,
    /// return api version of signal/desk/manage server
    pub api_version: i32,
}

/// Password params
#[derive(Deserialize, ToSchema)]
pub struct PasswordParams {
    /// Old username
    pub username: String,
    /// Old password
    pub password: String,
    /// New username (optional)
    pub new_username: Option<String>,
    /// New password (optional)
    pub new_password: Option<String>,
}
