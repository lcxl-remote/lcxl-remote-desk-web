use windows::Win32::{
    Graphics::Gdi::{
        CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT,
        DM_PELSWIDTH,
    },
    UI::Input::KeyboardAndMouse::BlockInput,
};
use windows_core::HSTRING;

use crate::{
    desk_error::DeskError,
    model::{
        common::ErrorCode,
        system_setting::{DisplaySettings, SystemSettingHelper},
    },
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
        let device_name = HSTRING::from(display_settings.device_name.as_str());
        let mut dev_mode = DEVMODEW::default();
        dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as _;
        if let Some(width) = display_settings.width {
            dev_mode.dmPelsWidth = width;
            dev_mode.dmFields |= DM_PELSWIDTH;
        }
        if let Some(height) = display_settings.height {
            dev_mode.dmPelsHeight = height;
            dev_mode.dmFields |= DM_PELSHEIGHT;
        }
        let result = unsafe {
            ChangeDisplaySettingsExW(
                &device_name,
                Some(&dev_mode),
                None,
                //CDS_UPDATEREGISTRY | CDS_GLOBAL | CDS_RESET,
                CDS_TYPE(0),
                None,
            )
        };
        if result != DISP_CHANGE_SUCCESSFUL {
            return DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!("Failed to change display settings, code: {}", result.0),
            );
        }
        Ok(())
    }

    fn block_input(&self, block: bool) -> Result<(), DeskError> {
        unsafe { Ok(BlockInput(block)?) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_change_display_settings() {
        let helper =
            WindowsSystemSettingHelper::new(&crate::model::settings::DeskSettings::default())
                .unwrap();
        let display_settings = DisplaySettings {
            device_name: String::from("\\\\.\\DISPLAY1"),
            width: Some(1080),
            height: Some(1080),
            frequency: None,
            scaling_factor: None,
        };
        let result = helper.change_display_settings(&display_settings);
        assert!(
            result.is_ok(),
            "failed to change display settings: {:?}",
            result
        );
    }
}
