use actix_session::{Session, SessionGetError, SessionInsertError};
use serde::de::DeserializeOwned;

use crate::model::BaseUser;

pub const SESSION_KEY_USERNAME: &str = "username";

/// Trait for user session access.
pub trait UserSessionAccessor {
    /// Get current user from session.
    fn get_current_user<T: BaseUser + DeserializeOwned>(
        &self,
    ) -> Result<Option<T>, SessionGetError>;
    /// Set current user to session.
    fn set_current_user<T: BaseUser + serde::Serialize>(
        &self,
        current_user: &T,
    ) -> Result<(), SessionInsertError>;
    /// Remove current user from session.
    fn remove_current_user(&self) -> Option<String>;
}

impl UserSessionAccessor for Session {
    fn get_current_user<T: BaseUser + DeserializeOwned>(
        &self,
    ) -> Result<Option<T>, SessionGetError> {
        self.get::<T>(SESSION_KEY_USERNAME)
    }

    fn set_current_user<T: BaseUser + serde::Serialize>(
        &self,
        current_user: &T,
    ) -> Result<(), SessionInsertError> {
        // Store user information in session
        self.insert(SESSION_KEY_USERNAME, current_user)
    }

    fn remove_current_user(&self) -> Option<String> {
        self.remove(SESSION_KEY_USERNAME)
    }
}
