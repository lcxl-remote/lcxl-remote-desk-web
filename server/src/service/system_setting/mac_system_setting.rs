use crate::{
    error::DeskError,
    model::system_setting::{DisplaySettings, SystemSettingHelper},
};
use desk_signal_facade::model::desk_settings::DeskSettings;

pub struct MacSystemSettingHelper {
    cmd_sender: Option<std::sync::mpsc::Sender<crate::model::system_setting::PrivateScreenCommand>>,
}

impl MacSystemSettingHelper {
    pub fn new(
        _settings: &DeskSettings,
        cmd_sender: Option<
            std::sync::mpsc::Sender<crate::model::system_setting::PrivateScreenCommand>,
        >,
    ) -> Result<Self, DeskError> {
        Ok(Self { cmd_sender })
    }
}

impl SystemSettingHelper for MacSystemSettingHelper {
    fn change_display_settings(
        &self,
        _display_settings: &DisplaySettings,
    ) -> Result<(), DeskError> {
        // Not implemented on macOS yet
        Ok(())
    }

    fn block_input(&self, _block: bool) -> Result<(), DeskError> {
        // Not implemented on macOS yet
        Ok(())
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
        // Not implemented on macOS yet
        Ok(())
    }

    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), DeskError> {
        let mut clipboard = arboard::Clipboard::new().map_err(DeskError::ArboardError)?;
        clipboard.set_text(text).map_err(DeskError::ArboardError)?;
        Ok(())
    }
}
