use std::process::Command;
use std::sync::Mutex;

use arboard::Clipboard;
use desk_signal_facade::model::desk_settings::DeskSettings;
use desk_utils::error::DeskErrorCode;

use crate::{
    error::InputError,
    host_control::{
        input_grab::{EvdevGrabber, LocalInputBlocker},
        send_private_screen_command,
    },
    linux_display::{self, Backend},
    model::host_control::{DisplaySettings, HostControlHelper, PrivateScreenCommand},
};

pub struct LinuxHostControlHelper {
    clipboard: Option<Clipboard>,
    cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
    /// Holds the grabbed physical input devices while local input is
    /// blocked. Behind a `Mutex` because `block_input` takes `&self`
    /// (the helper is shared as `dyn HostControlHelper + Send + Sync`).
    input_blocker: Mutex<LocalInputBlocker>,
}

impl LinuxHostControlHelper {
    pub fn new(
        _desk_setting: &DeskSettings,
        cmd_sender: Option<std::sync::mpsc::Sender<PrivateScreenCommand>>,
    ) -> Result<Self, InputError> {
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
            input_blocker: Mutex::new(LocalInputBlocker::new(Box::new(EvdevGrabber::new()))),
        })
    }
}

impl HostControlHelper for LinuxHostControlHelper {
    fn change_display_settings(
        &self,
        display_settings: &DisplaySettings,
    ) -> Result<(), InputError> {
        let width = display_settings.width.unwrap_or(1920);
        let height = display_settings.height.unwrap_or(1080);
        match linux_display::detect_backend() {
            Backend::X11 => {
                let ops = linux_display::RealX11Ops::new()?;
                linux_display::apply_display_settings(
                    &ops,
                    &display_settings.device_name,
                    width,
                    height,
                    display_settings.frequency,
                )
            }
            Backend::Wayland => {
                // Wayland display mode changes go through the external
                // wlr-randr backend; the output name and refresh-rate
                // handling there are not yet on par with the X11 path.
                log::info!("Changing Wayland display settings via wlr-randr");
                Command::new("wlr-randr")
                    .arg("--output")
                    .arg("eDP-1")
                    .arg("--mode")
                    .arg(format!(
                        "{}x{}@{}",
                        width,
                        height,
                        display_settings.frequency.unwrap_or(60)
                    ))
                    .status()?;
                Ok(())
            }
            Backend::Headless => {
                log::warn!("No display environment variable found");
                InputError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    "No display environment variable found",
                )
            }
        }
    }

    fn block_input(&self, block: bool) -> Result<(), InputError> {
        let mut blocker = self
            .input_blocker
            .lock()
            .expect("input blocker mutex poisoned");
        if block {
            blocker.block()
        } else {
            blocker.unblock()
        }
    }

    fn enable_private_screen(
        &self,
        from_connection_id: &str,
        request_id: &str,
        enable: bool,
    ) -> Result<(), InputError> {
        send_private_screen_command(
            self.cmd_sender.as_ref(),
            from_connection_id,
            request_id,
            enable,
        );
        Ok(())
    }

    fn control_monitor_power(&self, turn_off: bool) -> Result<(), InputError> {
        match linux_display::detect_backend() {
            Backend::X11 => {
                let ops = linux_display::RealX11Ops::new()?;
                linux_display::control_monitor_power(&ops, turn_off)
            }
            _ => InputError::custom_error(DeskErrorCode::NOT_IMPLEMENTED_YET, ""),
        }
    }

    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), InputError> {
        if let Some(ref mut clipboard) = self.clipboard {
            clipboard.set_text(text)?;
        } else {
            log::warn!("Clipboard is not available");
        }
        Ok(())
    }

    fn get_text_from_clipboard(&mut self) -> Result<Option<String>, InputError> {
        if let Some(ref mut clipboard) = self.clipboard {
            match clipboard.get_text() {
                Ok(text) => Ok(Some(text)),
                Err(arboard::Error::ContentNotAvailable) => Ok(None),
                Err(e) => Err(InputError::from(e)),
            }
        } else {
            log::warn!("Clipboard is not available");
            Ok(None)
        }
    }

    fn get_image_from_clipboard(
        &mut self,
    ) -> Result<Option<crate::model::host_control::ClipboardImage>, InputError> {
        if let Some(ref mut clipboard) = self.clipboard {
            match clipboard.get_image() {
                Ok(img) => Ok(Some(crate::model::host_control::ClipboardImage {
                    width: img.width,
                    height: img.height,
                    bytes: img.bytes,
                })),
                Err(arboard::Error::ContentNotAvailable) => Ok(None),
                Err(e) => Err(InputError::from(e)),
            }
        } else {
            log::warn!("Clipboard is not available");
            Ok(None)
        }
    }

    fn set_image_to_clipboard(
        &mut self,
        image: &crate::model::host_control::ClipboardImage,
    ) -> Result<(), InputError> {
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
