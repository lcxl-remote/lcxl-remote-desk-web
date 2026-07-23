use crate::{
    error::InputError,
    host_control::send_private_screen_command,
    model::host_control::{DisplaySettings, HostControlHelper, PrivateScreenCommand},
};
use desk_signal_facade::model::desk_settings::DeskSettings;

pub struct MacHostControlHelper {
    cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
}

impl MacHostControlHelper {
    pub fn new(
        _settings: &DeskSettings,
        cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
    ) -> Result<Self, InputError> {
        Ok(Self { cmd_sender })
    }
}

impl HostControlHelper for MacHostControlHelper {
    fn change_display_settings(
        &self,
        _display_settings: &DisplaySettings,
    ) -> Result<(), InputError> {
        // Not implemented on macOS yet
        Ok(())
    }

    fn block_input(&self, _block: bool) -> Result<(), InputError> {
        // Not implemented on macOS yet
        Ok(())
    }

    fn enable_private_screen(
        &self,
        from_connection_id: &str,
        enable: bool,
    ) -> Result<(), InputError> {
        send_private_screen_command(self.cmd_sender.as_ref(), from_connection_id, enable);
        Ok(())
    }

    fn control_monitor_power(&self, _turn_off: bool) -> Result<(), InputError> {
        // Not implemented on macOS yet
        Ok(())
    }

    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), InputError> {
        let mut clipboard = arboard::Clipboard::new()?;
        clipboard.set_text(text)?;
        Ok(())
    }

    fn get_text_from_clipboard(&mut self) -> Result<Option<String>, InputError> {
        let mut clipboard = arboard::Clipboard::new()?;
        match clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn get_image_from_clipboard(
        &mut self,
    ) -> Result<Option<crate::model::host_control::ClipboardImage>, InputError> {
        let mut clipboard = arboard::Clipboard::new()?;
        match clipboard.get_image() {
            Ok(img) => Ok(Some(crate::model::host_control::ClipboardImage {
                width: img.width,
                height: img.height,
                bytes: img.bytes,
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn set_image_to_clipboard(
        &mut self,
        image: &crate::model::host_control::ClipboardImage,
    ) -> Result<(), InputError> {
        let mut clipboard = arboard::Clipboard::new()?;
        let img_data = arboard::ImageData {
            width: image.width,
            height: image.height,
            bytes: image.bytes.clone(),
        };
        clipboard.set_image(img_data)?;
        Ok(())
    }
}
