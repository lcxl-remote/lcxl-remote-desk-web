use serde::{Deserialize, Serialize};

/// User settings
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UserSettings {
    /// login user name
    pub login_user_name: String,
    /// login password
    pub login_password: String,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            login_user_name: "admin".to_string(),
            login_password: "".to_string(),
        }
    }
}
