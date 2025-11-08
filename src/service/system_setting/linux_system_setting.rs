use std::{env, process::Command};

use crate::{
    desk_error::DeskError,
    model::{
        common::ErrorCode,
        settings::DeskSettings,
        system_setting::{DisplaySettings, SystemSettingHelper},
    },
};

pub struct LinuxSystemSettingHelper {}

impl LinuxSystemSettingHelper {
    pub fn new(desk_setting: &DeskSettings) -> Self {
        Self {}
    }
}

impl SystemSettingHelper for LinuxSystemSettingHelper {
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError> {
        // FIXME Implement the logic to change display settings on Linux
        if let Ok(env_value) = env::var("WAYLAND_DISPLAY ") {
            log::info!("Current Wayland display: {}", env_value);
            Command::new("wlr-randr")
                .arg("--output")
                .arg("eDP-1")
                .arg("--mode")
                .arg(format!(
                    "{}x{}@{}",
                    display_settings.width.unwrap_or(1920),
                    display_settings.height.unwrap_or(1080),
                    display_settings.frequency.unwrap_or(60)
                ))
                .status()?;
        } else if let Ok(env_value) = env::var("DISPLAY") {
            log::info!("Current X11 display: {}", env_value);
            Command::new("xrandr")
                .arg("--output")
                .arg("eDP-1")
                .arg("--mode")
                .arg(format!(
                    "{}x{}",
                    display_settings.width.unwrap_or(1920),
                    display_settings.height.unwrap_or(1080)
                ))
                .status()?;
        } else {
            log::warn!("No display environment variable found");
            return DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                "No display environment variable found".to_owned(),
            );
        }
        Ok(())
    }

    fn block_input(&self, block: bool) -> Result<(), DeskError> {
        // FIXME
        DeskError::custom_error(ErrorCode::NOT_IMPLEMENTED_YET, "".to_owned())
    }

    fn enable_private_screen(&self, enable: bool) -> Result<(), DeskError> {
        DeskError::custom_error(ErrorCode::NOT_IMPLEMENTED_YET, "".to_owned())
    }
    
    fn control_monitor_power(&self, turn_off: bool) -> Result<(), DeskError> {
        DeskError::custom_error(ErrorCode::NOT_IMPLEMENTED_YET, "".to_owned())
    }
}
