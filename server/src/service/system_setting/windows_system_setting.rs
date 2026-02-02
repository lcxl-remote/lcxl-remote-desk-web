use arboard::Clipboard;
use desk_signal_facade::model::desk_settings::{DeskSettings, PrivateScreenSettings};
use desk_utils::error::{CustomDeskError, DeskErrorCode};
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
    model::system_setting::{
        DisplaySettings, PrivateScreenState, SystemSettingEventType, SystemSettingHelper,
        SystemSettingSubscriber,
    },
    service::system_setting::direct2d_private_screen::{PrivateScreenCommand, PrivateScreenWindow},
};

/// Thread function to create and manage the private screen window,
/// see https://bbs.kanxue.com/thread-279475.htm
fn private_screen_window_thread(
    private_screen_settings: PrivateScreenSettings,
    receiver: std::sync::mpsc::Receiver<PrivateScreenCommand>,
    subscriber: SystemSettingSubscriber,
    inited_tx: std::sync::mpsc::Sender<Result<(), DeskError>>,
) -> Result<(), DeskError> {
    let mut window = match PrivateScreenWindow::new(private_screen_settings, subscriber, receiver) {
        Ok(window) => window,
        Err(e) => {
            inited_tx.send(Err(e)).map_err(|_| {
                DeskError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    "Failed to send private screen inited event".to_owned(),
                ))
            })?;
            return Ok(());
        }
    };
    log::info!("Private screen window created: {:?}", window);
    let init_result = (window.subscriber)(SystemSettingEventType::PrivateScreenInited(
        PrivateScreenState::from(&window.state),
    ));
    if let Err(e) = init_result {
        inited_tx.send(Err(e)).map_err(|_| {
            DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to send private screen inited event".to_owned(),
            ))
        })?;
        return Ok(());
    }
    inited_tx.send(Ok(())).map_err(|_| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            "Failed to send private screen inited event".to_owned(),
        ))
    })?;

    log::info!("Entering private screen window message loop");
    window.run()?;
    Ok(())
}

pub struct WindowsSystemSettingHelper {
    main_sender: std::sync::mpsc::Sender<PrivateScreenCommand>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
    clipboard: Clipboard,
}
/// Safety: WindowsSystemSettingHelper is Send + Sync
unsafe impl Send for WindowsSystemSettingHelper {}
unsafe impl Sync for WindowsSystemSettingHelper {}

impl WindowsSystemSettingHelper {
    pub fn new(
        desk_setting: &DeskSettings,
        subscriber: SystemSettingSubscriber,
    ) -> Result<Self, DeskError> {
        let (main_sender, window_receiver) = std::sync::mpsc::channel::<PrivateScreenCommand>();
        let (inited_tx, inited_rx) = std::sync::mpsc::channel::<Result<(), DeskError>>();
        // let (window_sender, main_receiver) = std::sync::mpsc::channel::<PrivateScreenWindowState>();
        let private_screen_settings = desk_setting.private_screen.clone();
        let thread_handle = std::thread::spawn(move || {
            let result = private_screen_window_thread(
                private_screen_settings,
                window_receiver,
                subscriber,
                inited_tx,
            );
            if result.is_err() {
                log::error!(
                    "Private screen window thread exited with error: {:?}",
                    result.err()
                );
            } else {
                log::warn!("Private screen window thread exited normally");
            }
        });
        let init_result = inited_rx.recv()?;
        if let Err(e) = init_result {
            log::error!("Private screen window thread exited with error: {:?}", e);
            return Err(e);
        }
        let clipboard = Clipboard::new().unwrap();
        Ok(Self {
            main_sender,
            thread_handle: Some(thread_handle),
            clipboard,
        })
    }

    pub fn show_window(&self) -> Result<(), DeskError> {
        //PrivateScreenWindow::show_window(self.hwnd)
        self.main_sender
            .send(PrivateScreenCommand::ShowWindow)
            .map_err(|e| {
                DeskError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    format!("Failed to send ShowWindow command: {}", e),
                ))
            })
    }

    pub fn hide_window(&self) -> Result<(), DeskError> {
        //PrivateScreenWindow::hide_window(self.hwnd)
        self.main_sender
            .send(PrivateScreenCommand::HideWindow)
            .map_err(|e| {
                DeskError::CustomError(CustomDeskError::new(
                    DeskErrorCode::SYSTEM_ERROR,
                    format!("Failed to send ShowWindow command: {}", e),
                ))
            })
    }
}

impl Drop for WindowsSystemSettingHelper {
    fn drop(&mut self) {
        // Clean up the private screen window thread
        let result = self.main_sender.send(PrivateScreenCommand::Quit);
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
        log::warn!(
            "WindowsSystemSettingHelper dropped: send quit command result: {:?}",
            result
        );
    }
}

impl SystemSettingHelper for WindowsSystemSettingHelper {
    fn change_display_settings(&self, display_settings: &DisplaySettings) -> Result<(), DeskError> {
        // Implement Windows-specific system setting application logic here
        let device_name = HSTRING::from(display_settings.device_name.as_str());
        let mut dev_mode = DEVMODEW::default();
        dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as _;
        if let Some(width) = display_settings.width {
            dev_mode.dmPelsWidth = width;
            dev_mode.dmFields |= DM_PELSWIDTH;
        }
        if let Some(height) = display_settings.height {
            dev_mode.dmPelsHeight = height;
            dev_mode.dmFields |= DM_PELSHEIGHT;
        }
        let result = unsafe {
            ChangeDisplaySettingsExW(
                &device_name,
                Some(&dev_mode),
                None,
                //CDS_UPDATEREGISTRY | CDS_GLOBAL | CDS_RESET,
                CDS_TYPE(0),
                None,
            )
        };
        if result != DISP_CHANGE_SUCCESSFUL {
            return DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                format!("Failed to change display settings, code: {}", result.0),
            );
        }
        Ok(())
    }

    fn block_input(&self, block: bool) -> Result<(), DeskError> {
        unsafe { Ok(BlockInput(block)?) }
    }

    fn enable_private_screen(&self, enable: bool) -> Result<(), DeskError> {
        if enable {
            self.show_window()?;
        } else {
            self.hide_window()?;
        }
        Ok(())
    }

    fn control_monitor_power(&self, turn_off: bool) -> Result<(), DeskError> {
        unsafe {
            if turn_off {
                SendMessageW(
                    HWND_BROADCAST,
                    WM_SYSCOMMAND,
                    Some(WPARAM(SC_MONITORPOWER as usize)),
                    Some(LPARAM(2)),
                );
            } else {
                SendMessageW(
                    HWND_BROADCAST,
                    WM_SYSCOMMAND,
                    Some(WPARAM(SC_MONITORPOWER as usize)),
                    Some(LPARAM(-1)),
                );
            }
            Ok(())
        }
    }

    fn set_text_to_clipboard(&mut self, text: &str) -> Result<(), DeskError> {
        self.clipboard.set_text(text)?;
        //self.clipboard.get()
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use desk_utils::logs::init_logs;
    use log::LevelFilter;
    use windows::Win32::UI::WindowsAndMessaging::{
        WS_EX_OVERLAPPEDWINDOW, WS_EX_TOPMOST, WS_OVERLAPPEDWINDOW,
    };

    use super::*;
    static INIT: Once = Once::new();

    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            let _ = init_logs(LevelFilter::Debug);

            rust_i18n::set_locale("zh-CN");
        });
    }
    #[test]
    fn test_change_display_settings() {
        initialize();
        let helper = WindowsSystemSettingHelper::new(
            &DeskSettings::default(),
            |event_type: SystemSettingEventType| -> Result<(), DeskError> {
                log::info!("Event type: {:?}", event_type);
                Ok(())
            },
        )
        .unwrap();
        let display_settings = DisplaySettings {
            device_name: String::from("\\\\.\\DISPLAY1"),
            width: Some(1080),
            height: Some(1080),
            frequency: None,
            scaling_factor: None,
        };
        let result = helper.change_display_settings(&display_settings);
        assert!(
            result.is_ok(),
            "failed to change display settings: {:?}",
            result
        );
    }

    #[test]
    fn test_private_screen() {
        initialize();

        let mut desk_settings = DeskSettings::default();
        desk_settings.private_screen.window_style = Some(WS_OVERLAPPEDWINDOW.0);
        desk_settings.private_screen.window_ex_style =
            Some(WS_EX_TOPMOST.0 | WS_EX_OVERLAPPEDWINDOW.0);
        let helper = WindowsSystemSettingHelper::new(
            &desk_settings,
            |event_type: SystemSettingEventType| -> Result<(), DeskError> {
                log::info!("Event type: {:?}", event_type);
                Ok(())
            },
        )
        .unwrap();
        let result = helper.enable_private_screen(true);
        assert!(
            result.is_ok(),
            "failed to enable private screen: {:?}",
            result
        );
        std::thread::sleep(std::time::Duration::from_secs(30));
        let result = helper.enable_private_screen(false);
        assert!(
            result.is_ok(),
            "failed to disable private screen: {:?}",
            result
        );
    }
}
