use std::time::Duration;

use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{
        CDS_TYPE, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT,
        DM_PELSWIDTH, ValidateRect,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::BlockInput,
        WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
            DispatchMessageW, IDC_ARROW, LoadCursorW, MSG, PM_REMOVE, PeekMessageW,
            PostQuitMessage, RegisterClassW, SW_HIDE, SW_SHOW, ShowWindow, TranslateMessage,
            WINDOW_EX_STYLE, WM_DESTROY, WM_PAINT, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
        },
    },
};
use windows_core::{HSTRING, w};

use crate::{
    desk_error::DeskError,
    model::{
        common::ErrorCode,
        system_setting::{DisplaySettings, SystemSettingHelper},
    },
};
// Windows message handler for private screen window
extern "system" fn wndproc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_PAINT => {
                log::debug!("WM_PAINT");
                _ = ValidateRect(Some(window), None);
                LRESULT(0)
            }
            WM_DESTROY => {
                log::warn!("WM_DESTROY");
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

pub enum PrivateScreenCommand {
    Show,
    Hide,
    Quit,
}
/// Thread function to create and manage the private screen window,
/// see https://bbs.kanxue.com/thread-279475.htm
fn private_screen_window_thread(
    receiver: std::sync::mpsc::Receiver<PrivateScreenCommand>,
) -> Result<(), DeskError> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let window_class = w!("window");

        let wc = WNDCLASSW {
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hInstance: instance.into(),
            lpszClassName: window_class,

            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            ..Default::default()
        };

        let atom = RegisterClassW(&wc);
        debug_assert!(atom != 0);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            window_class,
            w!("This is a sample window"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            None,
            None,
        )?;

        let mut message = MSG::default();
        loop {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).into() {
                let result = TranslateMessage(&message);
                if !result.as_bool() {
                    log::error!(
                        "TranslateMessage failed: {:?}",
                        windows_core::Error::from_win32()
                    );
                }
                DispatchMessageW(&message);
            }
            let result = receiver.recv_timeout(Duration::from_millis(10));
            if let Err(e) = result {
                match e {
                    std::sync::mpsc::RecvTimeoutError::Timeout => continue,
                    _ => {
                        log::error!("Private screen window thread recv error: {}", e);
                        break;
                    }
                };
            } else if let Ok(command) = result {
                match command {
                    PrivateScreenCommand::Show => {
                        let show_result = ShowWindow(hwnd, SW_SHOW);
                        log::info!("ShowWindow result: {:?}", show_result);
                    }
                    PrivateScreenCommand::Hide => {
                        let show_result = ShowWindow(hwnd, SW_HIDE);
                        log::info!("ShowWindow result: {:?}", show_result);
                    }
                    PrivateScreenCommand::Quit => {
                        log::warn!("Private screen window thread quitting");
                        break;
                    }
                }
            }
        }

        Ok(())
    }
}

pub struct WindowsSystemSettingHelper {
    main_sender: std::sync::mpsc::Sender<PrivateScreenCommand>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl WindowsSystemSettingHelper {
    pub fn new(
        _desk_setting: &crate::model::settings::DeskSettings,
    ) -> Result<Self, crate::desk_error::DeskError> {
        let (main_sender, window_receiver) = std::sync::mpsc::channel::<PrivateScreenCommand>();

        let thread_handle = std::thread::spawn(move || {
            let result = private_screen_window_thread(window_receiver);
            if result.is_err() {
                log::error!(
                    "Private screen window thread exited with error: {:?}",
                    result.err()
                );
            } else {
                log::warn!("Private screen window thread exited normally");
            }
        });
        Ok(Self {
            main_sender,
            thread_handle: Some(thread_handle),
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
                ErrorCode::SYSTEM_ERROR,
                format!("Failed to change display settings, code: {}", result.0),
            );
        }
        Ok(())
    }

    fn block_input(&self, block: bool) -> Result<(), DeskError> {
        unsafe { Ok(BlockInput(block)?) }
    }

    fn enable_private_screen(&self, enable: bool) -> Result<(), DeskError> {
        let result;
        if enable {
            result = self.main_sender.send(PrivateScreenCommand::Show);
        } else {
            result = self.main_sender.send(PrivateScreenCommand::Hide);
        }

        if let Err(e) = result {
            return DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                format!("Failed to send private screen command: {}", e),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use log::LevelFilter;

    use crate::utils::logs::init_logs;

    use super::*;
    static INIT: Once = Once::new();

    pub fn initialize() {
        INIT.call_once(|| {
            // initialization code here
            let _ = init_logs(LevelFilter::Debug);
        });
    }
    #[test]
    fn test_change_display_settings() {
        initialize();
        let helper =
            WindowsSystemSettingHelper::new(&crate::model::settings::DeskSettings::default())
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
        let helper =
            WindowsSystemSettingHelper::new(&crate::model::settings::DeskSettings::default())
                .unwrap();
        let result = helper.enable_private_screen(true);
        assert!(
            result.is_ok(),
            "failed to enable private screen: {:?}",
            result
        );
        std::thread::sleep(std::time::Duration::from_secs(3));
        let result = helper.enable_private_screen(false);
        assert!(
            result.is_ok(),
            "failed to disable private screen: {:?}",
            result
        );
    }
}
