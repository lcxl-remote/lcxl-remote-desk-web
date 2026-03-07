use arboard::Clipboard;
use desk_signal_facade::model::desk_settings::DeskSettings;
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    Graphics::Gdi::{
        CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT,
        DM_PELSWIDTH,
    },
    UI::{
        Input::KeyboardAndMouse::BlockInput,
        WindowsAndMessaging::{HWND_BROADCAST, SC_MONITORPOWER, SendMessageW, WM_SYSCOMMAND},
    },
};
use windows_core::HSTRING;

use crate::{
    error::DeskError,
    model::host_control::{DisplaySettings, HostControlHelper, PrivateScreenCommand},
};

pub struct WindowsHostControlHelper {
    clipboard: Clipboard,
    cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
}

impl WindowsHostControlHelper {
    pub fn new(
        _desk_setting: &DeskSettings,
        cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
    ) -> Result<Self, DeskError> {
        let clipboard = Clipboard::new().unwrap();
        Ok(Self {
            clipboard,
            cmd_sender,
        })
    }
}

impl HostControlHelper for WindowsHostControlHelper {
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError> {
        let mut devmode = DEVMODEW::default();
        devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        devmode.dmPelsWidth = display_settings.width.unwrap_or(1920);
        devmode.dmPelsHeight = display_settings.height.unwrap_or(1080);
        devmode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;

        let result = unsafe {
            ChangeDisplaySettingsExW(
                &HSTRING::from(display_settings.device_name.clone()),
                Some(&devmode),
                None,
                CDS_TYPE(0),
                None,
            )
        };

        if result == DISP_CHANGE_SUCCESSFUL {
            Ok(())
        } else {
            Err(DeskError::WindowsResultError(
                std::backtrace::Backtrace::capture(),
                windows::core::Error::from_win32(),
            ))
        }
    }

    fn block_input(&self, block: bool) -> Result<(), DeskError> {
        unsafe {
            let result = BlockInput(block);

            if let Err(err) = result {
                log::warn!("Failed to block input: {}", err);
                return Err(DeskError::from(err));
            }
        }
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

    fn control_monitor_power(&self, turn_off: bool) -> Result<(), DeskError> {
        let monitor_state = if turn_off { 2isize } else { -1isize };
        unsafe {
            SendMessageW(
                HWND_BROADCAST,
                WM_SYSCOMMAND,
                Some(WPARAM(SC_MONITORPOWER as usize)),
                Some(LPARAM(monitor_state)),
            );
        }
        Ok(())
    }

    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), DeskError> {
        self.clipboard.set_text(text)?;
        Ok(())
    }

    fn get_text_from_clipboard(&mut self) -> Result<Option<String>, DeskError> {
        match self.clipboard.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(DeskError::from(e)),
        }
    }

    fn get_image_from_clipboard(
        &mut self,
    ) -> Result<Option<crate::model::host_control::ClipboardImage>, DeskError> {
        match self.clipboard.get_image() {
            Ok(img) => Ok(Some(crate::model::host_control::ClipboardImage {
                width: img.width,
                height: img.height,
                bytes: img.bytes,
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(e) => Err(DeskError::from(e)),
        }
    }

    fn set_image_to_clipboard(
        &mut self,
        image: &crate::model::host_control::ClipboardImage,
    ) -> Result<(), DeskError> {
        let img_data = arboard::ImageData {
            width: image.width,
            height: image.height,
            bytes: image.bytes.clone(),
        };
        self.clipboard.set_image(img_data)?;
        Ok(())
    }
}
