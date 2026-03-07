use crate::{
    error::DeskError,
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
    ) -> Result<Self, DeskError> {
        Ok(Self { cmd_sender })
    }
}

impl HostControlHelper for MacHostControlHelper {
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
        // Not implemented on macOS yet
        Ok(())
    }

    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), DeskError> {
        let mut clipboard = arboard::Clipboard::new().map_err(DeskError::ArboardError)?;
        clipboard.set_text(text).map_err(DeskError::ArboardError)?;
        Ok(())
    }

    fn get_text_from_clipboard(&mut self) -> Result<Option<String>, DeskError> {
        let mut clipboard = arboard::Clipboard::new().map_err(DeskError::ArboardError)?;
        match clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(DeskError::ArboardError(e)),
        }
    }

    fn get_image_from_clipboard(
        &mut self,
    ) -> Result<Option<crate::model::host_control::ClipboardImage>, DeskError> {
        let mut clipboard = arboard::Clipboard::new().map_err(DeskError::ArboardError)?;
        match clipboard.get_image() {
            Ok(img) => Ok(Some(crate::model::host_control::ClipboardImage {
                width: img.width,
                height: img.height,
                bytes: img.bytes,
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(DeskError::ArboardError(e)),
        }
    }

    fn set_image_to_clipboard(
        &mut self,
        image: &crate::model::host_control::ClipboardImage,
    ) -> Result<(), DeskError> {
        let mut clipboard = arboard::Clipboard::new().map_err(DeskError::ArboardError)?;
        let img_data = arboard::ImageData {
            width: image.width,
            height: image.height,
            bytes: image.bytes.clone(),
        };
        clipboard
            .set_image(img_data)
            .map_err(DeskError::ArboardError)?;
        Ok(())
    }
}
