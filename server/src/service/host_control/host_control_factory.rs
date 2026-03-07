use desk_signal_facade::model::desk_settings::DeskSettings;

use crate::{error::DeskError, model::host_control::HostControlHelper};

#[cfg(target_os = "linux")]
use crate::service::host_control::linux_host_control::LinuxHostControlHelper;

#[cfg(target_os = "windows")]
use crate::service::host_control::windows_host_control::WindowsHostControlHelper;

#[cfg(target_os = "macos")]
use crate::service::host_control::mac_host_control::MacHostControlHelper;

pub fn create_host_control_helper(
    desk_setting: &DeskSettings,
    cmd_sender: Option<std::sync::mpsc::Sender<crate::model::host_control::PrivateScreenCommand>>,
) -> Result<Box<dyn HostControlHelper + Send + Sync>, DeskError> {
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
