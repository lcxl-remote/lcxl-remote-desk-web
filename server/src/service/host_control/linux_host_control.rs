use std::{env, process::Command};

use arboard::Clipboard;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_utils::error::DeskErrorCode;

use crate::{
    error::DeskError,
    model::host_control::{DisplaySettings, HostControlHelper, PrivateScreenCommand},
};

pub struct LinuxHostControlHelper {
    clipboard: Option<Clipboard>,
    cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
}

impl LinuxHostControlHelper {
    pub fn new(
        _desk_setting: &DeskSettings,
        cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
    ) -> Result<Self, DeskError> {
        let clipboard_result = Clipboard::new();
        let clipboard = match clipboard_result {
            Ok(clipboard) => Some(clipboard),
            Err(e) => {
                log::warn!("Failed to create clipboard: {}", e);
                None
            }
        };

        Ok(Self {
            clipboard,
            cmd_sender,
        })
    }
}

impl HostControlHelper for LinuxHostControlHelper {
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError> {
        // FIXME Implement the logic to change display settings on Linux
        if let Ok(env_value) = env::var("WAYLAND_DISPLAY") {
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

    fn enable_private_screen(&self, from_session_id: &str, enable: bool) -> Result<(), DeskError> {
        if let Some(sender) = &self.cmd_sender {
            let cmd = if enable {
                PrivateScreenCommand::Show(from_session_id.to_string())
            } else {
                PrivateScreenCommand::Hide(from_session_id.to_string())
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
        if let Some(ref mut clipboard) = self.clipboard {
            clipboard.set_text(text)?;
        } else {
            log::warn!("Clipboard is not available");
        }
        Ok(())
    }

    fn get_text_from_clipboard(&mut self) -> Result<Option<String>, DeskError> {
        if let Some(ref mut clipboard) = self.clipboard {
            match clipboard.get_text() {
                Ok(text) => Ok(Some(text)),
                Err(arboard::Error::ContentNotAvailable) => Ok(None),
                Err(e) => Err(DeskError::from(e)),
            }
        } else {
            log::warn!("Clipboard is not available");
            Ok(None)
        }
    }

    fn get_image_from_clipboard(
        &mut self,
    ) -> Result<Option<crate::model::host_control::ClipboardImage>, DeskError> {
        if let Some(ref mut clipboard) = self.clipboard {
            match clipboard.get_image() {
                Ok(img) => Ok(Some(crate::model::host_control::ClipboardImage {
                    width: img.width,
                    height: img.height,
                    bytes: img.bytes,
                })),
                Err(arboard::Error::ContentNotAvailable) => Ok(None),
                Err(e) => Err(DeskError::from(e)),
            }
        } else {
            log::warn!("Clipboard is not available");
            Ok(None)
        }
    }

    fn set_image_to_clipboard(
        &mut self,
        image: &crate::model::host_control::ClipboardImage,
    ) -> Result<(), DeskError> {
        if let Some(ref mut clipboard) = self.clipboard {
            let img_data = arboard::ImageData {
                width: image.width,
                height: image.height,
                bytes: image.bytes.clone(),
            };
            clipboard.set_image(img_data)?;
        } else {
            log::warn!("Clipboard is not available");
        }
        Ok(())
    }
}
