use std::{env, process::Command};

use arboard::Clipboard;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_utils::error::DeskErrorCode;

use crate::{
    error::DeskError,
    model::system_setting::{DisplaySettings, SystemSettingHelper},
};

pub struct LinuxSystemSettingHelper {
    clipboard: Clipboard,
    cmd_sender: Option<std::sync::mpsc::Sender<crate::model::system_setting::PrivateScreenCommand>>,
}

impl LinuxSystemSettingHelper {
    pub fn new(
        _desk_setting: &DeskSettings,
        cmd_sender: Option<
            std::sync::mpsc::Sender<crate::model::system_setting::PrivateScreenCommand>,
        >,
    ) -> Result<Self, DeskError> {
        Ok(Self {
            clipboard: Clipboard::new()?, // Assuming clipboard
            cmd_sender,
        })
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
                DeskErrorCode::SYSTEM_ERROR,
                "No display environment variable found",
            );
        }
        Ok(())
    }

    fn block_input(&self, _block: bool) -> Result<(), DeskError> {
        // FIXME
        DeskError::custom_error(DeskErrorCode::NOT_IMPLEMENTED_YET, "")
    }

    fn enable_private_screen(&self, enable: bool) -> Result<(), DeskError> {
        if let Some(sender) = &self.cmd_sender {
            let cmd = if enable {
                crate::model::system_setting::PrivateScreenCommand::Show
            } else {
                crate::model::system_setting::PrivateScreenCommand::Hide
            };
            if let Err(e) = sender.send(cmd) {
                log::error!("Failed to send private screen command: {}", e);
            }
        } else {
            log::warn!(
                "Private screen command sender is not configured (maybe starting as standalone server)"
            );
        }
        Ok(())
    }

    fn control_monitor_power(&self, _turn_off: bool) -> Result<(), DeskError> {
        DeskError::custom_error(DeskErrorCode::NOT_IMPLEMENTED_YET, "")
    }

    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), DeskError> {
        self.clipboard.set_text(text)?;
        Ok(())
    }
}
