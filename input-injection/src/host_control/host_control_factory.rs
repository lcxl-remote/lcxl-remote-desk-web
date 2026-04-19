use desk_signal_facade::model::desk_settings::DeskSettings;

use crate::{error::InputError, model::host_control::HostControlHelper};

#[cfg(target_os = "linux")]
use crate::host_control::linux_host_control::LinuxHostControlHelper;

#[cfg(target_os = "windows")]
use crate::host_control::windows_host_control::WindowsHostControlHelper;

#[cfg(target_os = "macos")]
use crate::host_control::mac_host_control::MacHostControlHelper;

pub fn create_host_control_helper(
    desk_setting: &DeskSettings,
    cmd_sender: Option<std::sync::mpsc::Sender<crate::model::host_control::PrivateScreenCommand>>,
) -> Result<Box<dyn HostControlHelper + Send + Sync>, InputError> {
    #[cfg(target_os = "windows")]
    {
        let helper = WindowsHostControlHelper::new(desk_setting, cmd_sender)?;
        Ok(Box::new(helper))
    }
    #[cfg(target_os = "linux")]
    {
        let helper = LinuxHostControlHelper::new(desk_setting, cmd_sender)?;
        Ok(Box::new(helper))
    }
    #[cfg(target_os = "macos")]
    {
        let helper = MacHostControlHelper::new(desk_setting, cmd_sender)?;
        Ok(Box::new(helper))
    }
}
