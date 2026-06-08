use crate::{
    error::InputError,
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
        if let Some(sender) = &self.cmd_sender {
            let cmd = if enable {
                PrivateScreenCommand::Show(from_connection_id.to_string())
            } else {
                PrivateScreenCommand::Hide(from_connection_id.to_string())
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
