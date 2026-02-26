use crate::{
    error::DeskError,
    model::system_setting::{DisplaySettings, SystemSettingHelper, SystemSettingSubscriber},
};
use desk_signal_facade::model::desk_settings::DeskSettings;

pub struct MacSystemSettingHelper {
    subscriber: SystemSettingSubscriber,
}

impl MacSystemSettingHelper {
    pub fn new(_settings: &DeskSettings, subscriber:SystemSettingSubscriber) -> Result<Self, DeskError> {
        Ok(Self {
            subscriber,
        })
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

    fn enable_private_screen(&self, _enable: bool) -> Result<(), DeskError> {
        // Not implemented on macOS yet
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
