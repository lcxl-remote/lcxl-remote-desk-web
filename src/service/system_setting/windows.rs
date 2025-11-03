use std::time::Duration;

use windows::Win32::{
    Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, WPARAM},
    Graphics::{
        Direct2D::{
            D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1CreateFactory,
            ID2D1Factory1,
        },
        Dxgi::{CreateDXGIFactory1, IDXGIFactory2},
        Gdi::{BeginPaint, EndPaint, PAINTSTRUCT},
    },
    System::{
        Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
        LibraryLoader::GetModuleHandleW,
    },
    UI::WindowsAndMessaging::{
        CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
        DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetWindowLongPtrW, IDC_ARROW, LWA_COLORKEY,
        LoadCursorW, MSG, PM_REMOVE, PeekMessageW, PostQuitMessage, RegisterClassW,
        SetLayeredWindowAttributes, SetWindowDisplayAffinity, SetWindowLongPtrW, TranslateMessage,
        UnregisterClassW, WDA_EXCLUDEFROMCAPTURE, WM_DESTROY, WM_DISPLAYCHANGE, WM_NCCREATE,
        WM_PAINT, WM_QUIT, WM_SIZE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_OVERLAPPED,
    },
};
use windows_core::{PCWSTR, w};

use crate::desk_error::DeskError;

pub fn LOWORD(l: isize) -> isize {
    l & 0xffff
}

pub fn HIWORD(l: isize) -> isize {
    (l >> 16) & 0xffff
}

pub enum PrivateScreenCommand {
    Quit,
}

pub struct PrivateScreenWindow {
    pub receiver: std::sync::mpsc::Receiver<PrivateScreenCommand>,
    pub window_class: PCWSTR,
    pub handle: HWND,
    pub instance: HMODULE,
    pub factory: ID2D1Factory1,
    pub dxfactory: IDXGIFactory2,
    pub height: isize,
    pub width: isize,
}

impl PrivateScreenWindow {
    extern "system" fn wndproc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if message == WM_NCCREATE {
                let cs = lparam.0 as *const CREATESTRUCTW;
                let this = (*cs).lpCreateParams as *mut Self;
                (*this).handle = window;

                SetWindowLongPtrW(window, GWLP_USERDATA, this as _);
            } else {
                let this = GetWindowLongPtrW(window, GWLP_USERDATA) as *mut Self;

                if !this.is_null() {
                    return (*this).message_handler(message, wparam, lparam);
                }
            }

            DefWindowProcW(window, message, wparam, lparam)
        }
    }

    pub fn new(
        receiver: std::sync::mpsc::Receiver<PrivateScreenCommand>,
    ) -> Result<Self, DeskError> {
        unsafe {
            // Initialize COM library
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;

            // Create Direct2D factory
            let options = D2D1_FACTORY_OPTIONS::default();
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&options))?;
            let dxfactory: IDXGIFactory2 = CreateDXGIFactory1()?;

            // Create window
            let mut instance = Self {
                receiver,
                window_class: w!("lcxl-web-private-screen-window-class"),
                handle: HWND::default(),
                instance: HMODULE::default(),
                factory,
                dxfactory,
                height: 0,
                width: 0,
            };
            instance.create_window()?;
            Ok(instance)
        }
    }

    pub fn run(&self) -> Result<(), DeskError> {
        unsafe {
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
                let result = self.receiver.recv_timeout(Duration::from_millis(5));
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

    fn create_window(&mut self) -> Result<(), DeskError> {
        unsafe {
            let instance = GetModuleHandleW(None)?;

            let wc = WNDCLASSW {
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                hInstance: instance.into(),
                lpszClassName: self.window_class,

                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(Self::wndproc),
                ..Default::default()
            };

            let atom = RegisterClassW(&wc);
            debug_assert!(atom != 0);

            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TOOLWINDOW,
                self.window_class,
                w!("This is a sample window"),
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                None,
                Some(self as *mut _ as _),
            )?;
            // Set the window to be excluded from screen capture
            SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE)?;
            let crkey = COLORREF(0x00FF00); // Green color key RGB(0,255,0)
            SetLayeredWindowAttributes(hwnd, crkey, 255, LWA_COLORKEY)?;

            self.handle = hwnd;
            self.instance = instance;
            Ok(())
        }
    }

    fn message_handler(&mut self, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        self.inner_message_handler(message, wparam, lparam)
            .unwrap_or_else(|err| {
                log::error!("message_handler error: {:?}", err);
                LRESULT(-1)
            })
    }

    fn inner_message_handler(
        &mut self,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<LRESULT, DeskError> {
        let result = unsafe {
            match message {
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    let hdc = BeginPaint(self.handle, &mut ps);

                    // All painting occurs here, between BeginPaint and EndPaint.
                    log::debug!("WM_PAINT: ps = {:?}, hdc = {:?}", ps, hdc);
                    //FillRect(hdc, &ps.rcPaint, HBRUSH((COLOR_WINDOW.0 + 1) as _));
                    self.render()?;
                    let _ = EndPaint(self.handle, &ps);
                    LRESULT(0)
                }
                WM_SIZE => {
                    self.width = LOWORD(lparam.0);
                    self.height = HIWORD(lparam.0);
                    log::warn!("WM_SIZE: width = {}, height = {}", self.width, self.height);
                    LRESULT(0)
                }
                WM_DISPLAYCHANGE => {
                    self.render()?;
                    LRESULT(0)
                }
                WM_DESTROY => {
                    log::warn!("WM_DESTROY");
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(self.handle, message, wparam, lparam),
            }
        };
        Ok(result)
    }

    /// Render the window content using Direct2D
    fn render(&mut self) -> Result<LRESULT, DeskError> {
        //TODO: implement render logic here
        Ok(LRESULT(0))
    }
}

impl Drop for PrivateScreenWindow {
    fn drop(&mut self) {
        unsafe {
            // Clean up resources
            if !self.handle.is_invalid() {
                // Destroy the window
                let result = DestroyWindow(self.handle);
                if let Err(err) = result {
                    log::error!("DestroyWindow error: {:?}", err);
                }
                self.handle = HWND::default();
            }
            if !self.instance.is_invalid() {
                // Unregister the window class
                log::debug!("PrivateScreenWindow drop instance: {:?}", self.instance);
                let result = UnregisterClassW(self.window_class, Some(self.instance.into()));
                if let Err(err) = result {
                    log::error!("UnregisterClassW error: {:?}", err);
                }
                self.instance = HMODULE::default();
            }

            // Uninitialize COM library
            CoUninitialize();
        }
    }
}
