use desk_signal_facade::model::desk_settings::DeskSettings;

use crate::{error::DeskError, model::system_setting::SystemSettingHelper};

#[cfg(target_os = "linux")]
use crate::service::system_setting::linux_system_setting::LinuxSystemSettingHelper;

#[cfg(target_os = "windows")]
use crate::service::system_setting::windows_system_setting::WindowsSystemSettingHelper;

pub fn create_system_setting_helper(
    desk_setting: &DeskSettings,
) -> Result<Box<dyn SystemSettingHelper + Send + Sync>, DeskError> {
    #[cfg(target_os = "windows")]
    {
        let helper = WindowsSystemSettingHelper::new(desk_setting)?;
        Ok(Box::new(helper))
    }
    #[cfg(target_os = "linux")]
    {
        let helper = LinuxSystemSettingHelper::new(desk_setting)?;
        Ok(Box::new(helper))
    }
}
