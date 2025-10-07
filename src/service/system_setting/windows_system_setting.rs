
use windows::Win32::Graphics::Gdi::ChangeDisplaySettingsExW;
use windows_core::HSTRING;

use crate::{
    desk_error::DeskError,
    model::system_setting::{DisplaySettings, SystemSettingHelper},
};

pub struct WindowsSystemSettingHelper {}

impl WindowsSystemSettingHelper {
    pub fn new(
        _desk_setting: &crate::model::settings::DeskSettings,
    ) -> Result<Self, crate::desk_error::DeskError> {
        Ok(Self {})
    }
}

impl SystemSettingHelper for WindowsSystemSettingHelper {
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError> {
        // Implement Windows-specific system setting application logic here
        let device_name = if let Some(name) = &display_settings.device_name {
            Some(HSTRING::from(name))
        } else {
            None
        };
        let lpszdevicename = device_name.map(|s| s.as_ptr());
        unsafe { ChangeDisplaySettingsExW(lpszdevicename, lpdevmode, hwnd, dwflags, lparam) };
        Ok(())
    }
}
