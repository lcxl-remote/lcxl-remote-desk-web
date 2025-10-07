use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::desk_error::DeskError;

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, Default)]
pub struct DisplaySettings {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub frequency: Option<u32>,
}

pub trait SystemSettingHelper {
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError>;
}
