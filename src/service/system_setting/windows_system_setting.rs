use std::time::Duration;

use windows::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::Gdi::{
        BeginPaint, CDS_TYPE, COLOR_WINDOW, ChangeDisplaySettingsExW, DEVMODEW,
        DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT, DM_PELSWIDTH, EndPaint, FillRect, HBRUSH,
        PAINTSTRUCT, UpdateWindow, ValidateRect,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Input::KeyboardAndMouse::BlockInput,
        WindowsAndMessaging::{
            CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
            DispatchMessageW, GetDesktopWindow, GetSystemMetrics, GetWindowRect, HWND_TOPMOST,
            IDC_ARROW, LoadCursorW, MSG, MoveWindow, PM_REMOVE, PeekMessageW, PostQuitMessage,
            RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW, SW_SHOWMAXIMIZED,
            SWP_HIDEWINDOW, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetWindowDisplayAffinity,
            SetWindowPos, ShowWindow, TranslateMessage, UnregisterClassW, WDA_EXCLUDEFROMCAPTURE,
            WINDOW_EX_STYLE, WM_DESTROY, WM_PAINT, WM_QUIT, WNDCLASSW, WS_EX_LAYERED,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_OVERLAPPED,
            WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
        },
    },
};
use windows_core::{HSTRING, w};

use crate::{
    desk_error::{CustomDeskError, DeskError},
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
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(window, &mut ps);

                // All painting occurs here, between BeginPaint and EndPaint.
                log::debug!("WM_PAINT: ps = {:?}", ps);
                FillRect(hdc, &ps.rcPaint, HBRUSH((COLOR_WINDOW.0 + 1) as _));

                let _ = EndPaint(window, &ps);
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

pub fn show_window(hwnd: HWND) -> Result<(), DeskError> {
    unsafe {
        let desktop_hwnd = GetDesktopWindow();
        let mut desktop_rect = RECT::default();
        GetWindowRect(desktop_hwnd, &mut desktop_rect)?;
        log::info!("Desktop rect: {:?}", desktop_rect);

        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            desktop_rect.left,
            desktop_rect.top,
            desktop_rect.right - desktop_rect.left,
            desktop_rect.bottom - desktop_rect.top,
            SWP_SHOWWINDOW,
        )?;
        Ok(())
    }
}

pub fn hide_window(hwnd: HWND) -> Result<(), DeskError> {
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_HIDEWINDOW,
        )?;
        Ok(())
    }
}

pub enum PrivateScreenCommand {
    Show,
    Hide,
    Quit,
}

pub enum PrivateScreenWindowState {
    WindowHandle(HWND),
}
/// Safety: HWND is Send
unsafe impl Send for PrivateScreenWindowState {}

/// Thread function to create and manage the private screen window,
/// see https://bbs.kanxue.com/thread-279475.htm
fn private_screen_window_thread(
    receiver: std::sync::mpsc::Receiver<PrivateScreenCommand>,
    sender: std::sync::mpsc::Sender<PrivateScreenWindowState>,
) -> Result<(), DeskError> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let window_class = w!("lcxl-web-private-screen-window-class");

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
            //WINDOW_EX_STYLE::default(),
            WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            //WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_LAYERED/* | WS_EX_TOOLWINDOW */,
            window_class,
            w!("This is a sample window"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            None,
            None,
        )?;
        // Set the window to be excluded from screen capture
        SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;

        sender
            .send(PrivateScreenWindowState::WindowHandle(hwnd))
            .map_err(|e| {
                DeskError::CustomError(CustomDeskError::new(
                    ErrorCode::SYSTEM_ERROR,
                    format!("Failed to send window handle: {}", e),
                ))
            })?;

        let mut message = MSG::default();
        loop {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).into() {
                //see https://learn.microsoft.com/zh-cn/windows/win32/winmsg/about-messages-and-message-queues#message-handling
                if message.message == WM_QUIT {
                    log::warn!("Private screen window thread received WM_QUIT");
                    break;
                }

                let result = TranslateMessage(&message);
                if !result.as_bool() {
                    log::trace!(
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
                    PrivateScreenCommand::Show => show_window(hwnd)?,
                    PrivateScreenCommand::Hide => hide_window(hwnd)?,
                    PrivateScreenCommand::Quit => {
                        log::warn!("Private screen window thread quitting");
                        break;
                    }
                }
            }
        }
        DestroyWindow(hwnd)?;
        UnregisterClassW(window_class, Some(instance.into()))?;

        Ok(())
    }
}

pub struct WindowsSystemSettingHelper {
    main_sender: std::sync::mpsc::Sender<PrivateScreenCommand>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
    /// Handle to the private screen window. This is used to manage the window's visibility.
    hwnd: HWND,
}
/// Safety: WindowsSystemSettingHelper is Send + Sync
unsafe impl Send for WindowsSystemSettingHelper {}
unsafe impl Sync for WindowsSystemSettingHelper {}

impl WindowsSystemSettingHelper {
    pub fn new(
        _desk_setting: &crate::model::settings::DeskSettings,
    ) -> Result<Self, crate::desk_error::DeskError> {
        let (main_sender, window_receiver) = std::sync::mpsc::channel::<PrivateScreenCommand>();
        let (window_sender, main_receiver) = std::sync::mpsc::channel::<PrivateScreenWindowState>();

        let thread_handle = std::thread::spawn(move || {
            let result = private_screen_window_thread(window_receiver, window_sender);
            if result.is_err() {
                log::error!(
                    "Private screen window thread exited with error: {:?}",
                    result.err()
                );
            } else {
                log::warn!("Private screen window thread exited normally");
            }
        });
        let window_state = main_receiver.recv()?;
        let hwnd = if let PrivateScreenWindowState::WindowHandle(hwnd) = window_state {
            log::info!("Private screen window handle received: {:?}", hwnd);
            hwnd
        } else {
            return DeskError::custom_error(
                ErrorCode::SYSTEM_ERROR,
                "Failed to receive private screen window handle".to_string(),
            );
        };
        Ok(Self {
            main_sender,
            thread_handle: Some(thread_handle),
            hwnd,
        })
    }

    pub fn show_window(&self) -> Result<(), DeskError> {
        show_window(self.hwnd)
    }
    pub fn hide_window(&self) -> Result<(), DeskError> {
        hide_window(self.hwnd)
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
        if enable {
            self.show_window()?;
        } else {
            self.hide_window()?;
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
        std::thread::sleep(std::time::Duration::from_secs(600));
        let result = helper.enable_private_screen(false);
        assert!(
            result.is_ok(),
            "failed to disable private screen: {:?}",
            result
        );
    }
}
