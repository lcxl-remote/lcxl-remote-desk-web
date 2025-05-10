use actix_session::{Session, SessionGetError, SessionInsertError};

use crate::model::user::CurrentUser;

pub const SESSION_KEY_USERNAME: &str = "username";

pub trait SessionExt {
    fn get_current_user(&self) -> Result<Option<CurrentUser>, SessionGetError>;
    fn set_current_user(&self, current_user: &CurrentUser) -> Result<(), SessionInsertError>;
    fn remove_current_user(&self) -> Option<String>;
}

impl SessionExt for Session {
    fn get_current_user(&self) -> Result<Option<CurrentUser>, SessionGetError> {
        self.get::<CurrentUser>(SESSION_KEY_USERNAME)
    }

    fn set_current_user(&self, current_user: &CurrentUser) -> Result<(), SessionInsertError> {
        // Store user information in session
        self.insert(SESSION_KEY_USERNAME, current_user)
    }

    fn remove_current_user(&self) -> Option<String> {
        self.remove(SESSION_KEY_USERNAME)
    }
}
