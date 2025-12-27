use std::{marker::PhantomPinned, time::Duration};

use desk_signal_facade::model::desk_settings::PrivateScreenSettings;
use rust_i18n::t;
use windows::Win32::{
    Foundation::{COLORREF, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM},
    Graphics::{
        Direct2D::{
            Common::{D2D_RECT_F, D2D_SIZE_U, D2D1_COLOR_F},
            D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_OPTIONS, D2D1_FACTORY_TYPE_SINGLE_THREADED,
            D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE,
            D2D1_RENDER_TARGET_PROPERTIES, D2D1CreateFactory, ID2D1Factory1, ID2D1HwndRenderTarget,
            ID2D1SolidColorBrush,
        },
        DirectWrite::{
            DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL,
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_CENTER, DWriteCreateFactory,
            IDWriteFactory2, IDWriteTextFormat,
        },
        Dxgi::{CreateDXGIFactory1, IDXGIFactory2},
        Gdi::ValidateRect,
    },
    System::{
        Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize},
        LibraryLoader::GetModuleHandleW,
    },
    UI::{
        Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, RegisterHotKey, UnregisterHotKey},
        WindowsAndMessaging::{
            CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW,
            DestroyWindow, DispatchMessageW, GWLP_USERDATA, GetDesktopWindow, GetWindowLongPtrW,
            GetWindowRect, HWND_TOPMOST, IDC_ARROW, LWA_COLORKEY, LoadCursorW, MSG, PM_REMOVE,
            PeekMessageW, PostQuitMessage, RegisterClassW, SWP_HIDEWINDOW, SWP_NOMOVE, SWP_NOSIZE,
            SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowDisplayAffinity,
            SetWindowLongPtrW, SetWindowPos, TranslateMessage, UnregisterClassW,
            WDA_EXCLUDEFROMCAPTURE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WM_DISPLAYCHANGE,
            WM_HOTKEY, WM_NCCREATE, WM_PAINT, WM_QUIT, WM_SIZE, WNDCLASSW, WS_EX_LAYERED,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_OVERLAPPED,
            WS_VISIBLE,
        },
    },
};
use windows_core::{PCWSTR, w};
use windows_numerics::Matrix3x2;

use crate::error::DeskError;

pub fn loword(l: isize) -> isize {
    l & 0xffff
}

pub fn hiword(l: isize) -> isize {
    (l >> 16) & 0xffff
}

#[derive(Debug)]
pub enum PrivateScreenCommand {
    Quit,
    ShowWindow,
    HideWindow,
}

const EXIT_PRIVATE_SCREEN_HOTKEY_ID: usize = 2222;

const CARD_HEIGHT: f32 = 150.0;

#[derive(Debug, Clone)]
pub struct PrivateScreenWindowState {
    pub hwnd: HWND,
    pub hotkey_clicked: bool,
    pub visible: bool,
}

impl Default for PrivateScreenWindowState {
    fn default() -> Self {
        Self {
            hwnd: HWND::default(),
            hotkey_clicked: false,
            visible: false,
        }
    }
}
/// Safety: HWND is Send
unsafe impl Send for PrivateScreenWindowState {}

/// Private screen window struct
///
/// see https://github.com/microsoft/windows-rs/blob/master/crates/samples/windows/direct2d/src/main.rs
#[derive(Debug)]
pub struct PrivateScreenWindow {
    pub settings: PrivateScreenSettings,
    pub state: PrivateScreenWindowState,
    pub sender: std::sync::mpsc::Sender<PrivateScreenWindowState>,
    pub receiver: std::sync::mpsc::Receiver<PrivateScreenCommand>,
    pub window_class: PCWSTR,
    pub instance: HMODULE,
    pub factory: ID2D1Factory1,
    pub dxfactory: IDXGIFactory2,
    pub height: isize,
    pub width: isize,
    pub format: IDWriteTextFormat,
    pub hwnd_render_target: Option<ID2D1HwndRenderTarget>,
    pub black_brush: Option<ID2D1SolidColorBrush>,
    /// This marker ensures that the struct is !Unpin
    _marker: PhantomPinned,
}

impl PrivateScreenWindow {
    extern "system" fn wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if message == WM_NCCREATE {
                let cs = lparam.0 as *const CREATESTRUCTW;
                let this = (*cs).lpCreateParams as *mut Self;
                (*this).state.hwnd = hwnd;
                log::debug!(
                    "Storing pointer to PrivateScreenWindow: {:?}, {:?}",
                    this,
                    *this
                );
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, this as _);
            } else {
                let this = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Self;
                if !this.is_null() {
                    assert_eq!((*this).state.hwnd, hwnd);
                    return (*this).message_handler(message, wparam, lparam);
                }
            }

            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }

    pub fn new(
        settings: PrivateScreenSettings,
        sender: std::sync::mpsc::Sender<PrivateScreenWindowState>,
        receiver: std::sync::mpsc::Receiver<PrivateScreenCommand>,
    ) -> Result<Box<Self>, DeskError> {
        unsafe {
            // Initialize COM library
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            // DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 is available on Windows 10 version 1703 and later, this application need to run on Windows 8, so do not use it.
            // SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?;
            // Create Direct2D factory
            let options = D2D1_FACTORY_OPTIONS::default();
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&options))?;
            let dxfactory: IDXGIFactory2 = CreateDXGIFactory1()?;

            // Create window
            let instance = Self {
                settings,
                state: PrivateScreenWindowState::default(),
                sender,
                receiver,
                window_class: w!("lcxl-web-private-screen-window-class"),
                instance: HMODULE::default(),
                factory,
                dxfactory,
                format: Self::create_text_format()?,
                height: 0,
                width: 0,
                hwnd_render_target: None,
                black_brush: None,
                _marker: PhantomPinned,
            };
            // !!! Note that you must use `Box::new` to allocate memory here;
            // otherwise, the address of `self` will be changed, making the `self` pointer obtained in the window procedure function invalid.
            let mut instance = Box::new(instance);

            instance.create_window()?;

            Ok(instance)
        }
    }

    pub fn run(&mut self) -> Result<(), DeskError> {
        unsafe {
            let mut message = MSG::default();
            loop {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).into() {
                    //see https://learn.microsoft.com/zh-cn/windows/win32/winmsg/about-messages-and-message-queues#message-handling
                    if message.message == WM_QUIT {
                        log::warn!("Private screen window thread received WM_QUIT, exiting");
                        return Ok(());
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
                    log::info!(
                        "Private screen window thread received command: {:?}",
                        command
                    );
                    match command {
                        PrivateScreenCommand::Quit => PostQuitMessage(0),
                        PrivateScreenCommand::ShowWindow => self.show_window()?,
                        PrivateScreenCommand::HideWindow => self.hide_window()?,
                    }
                } else {
                    log::error!("Private screen window thread unknown recv result");
                }
            }
            Ok(())
        }
    }

    pub fn show_window(&mut self) -> Result<(), DeskError> {
        log::info!("Showing private screen window: {:?}", self.state.hwnd);
        unsafe {
            let desktop_hwnd = GetDesktopWindow();
            let mut desktop_rect = RECT::default();
            GetWindowRect(desktop_hwnd, &mut desktop_rect)?;
            log::info!("Desktop rect: {:?}", desktop_rect);

            let window_width = (desktop_rect.right - desktop_rect.left) / 2;
            let window_height = (desktop_rect.bottom - desktop_rect.top) / 2;
            let window_left =
                desktop_rect.left + (desktop_rect.right - desktop_rect.left - window_width) / 2;
            let window_top =
                desktop_rect.top + (desktop_rect.bottom - desktop_rect.top - window_height) / 2;
            SetWindowPos(
                self.state.hwnd,
                Some(HWND_TOPMOST),
                window_left,
                window_top,
                window_width,
                window_height,
                SWP_SHOWWINDOW,
            )?;

            RegisterHotKey(
                Some(self.state.hwnd),
                EXIT_PRIVATE_SCREEN_HOTKEY_ID as i32,
                MOD_ALT | MOD_CONTROL,
                'L' as u32,
            )?;

            self.state.visible = true;
            log::info!("Hotkey registered for private screen exit: Ctrl + Alt + L");
            Ok(())
        }
    }

    pub fn hide_window(&mut self) -> Result<(), DeskError> {
        log::info!("Hiding private screen window: {:?}", self.state.hwnd);
        unsafe {
            SetWindowPos(
                self.state.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_HIDEWINDOW,
            )?;
            let result =
                UnregisterHotKey(Some(self.state.hwnd), EXIT_PRIVATE_SCREEN_HOTKEY_ID as i32);
            if let Err(ref e) = result {
                if e.code().0 != 0x8007058Bu32 as i32 {
                    // ERROR_HOTKEY_NOT_REGISTERED
                    log::error!(
                        "Failed to unregister hotkey for private screen exit: {:?}",
                        e
                    );
                    result?;
                } else {
                    log::warn!(
                        "Hotkey for private screen exit was not registered, cannot unregister"
                    );
                }
            }
            self.state.visible = false;
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
            log::info!("Creating window with self address: {:p}", self);

            let dwstyle = if let Some(style) = self.settings.window_style {
                WINDOW_STYLE(style)
            } else {
                WS_OVERLAPPED
            };

            if dwstyle.0 & WS_VISIBLE.0 != 0 {
                self.state.visible = true;
            }
            let dwexstyle = if let Some(style) = self.settings.window_ex_style {
                WINDOW_EX_STYLE(style)
            } else {
                WS_EX_TOPMOST
                    | WS_EX_TRANSPARENT
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TOOLWINDOW
            };
            let hwnd = CreateWindowExW(
                dwexstyle,
                self.window_class,
                w!("LCXL Web Private Screen"),
                dwstyle,
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
            if WS_EX_LAYERED.0 & dwexstyle.0 != 0 {
                log::info!("Setting layered window attributes for hwnd: {:?}", hwnd);
                SetLayeredWindowAttributes(hwnd, crkey, 255, LWA_COLORKEY)?;
            }

            assert_eq!(self.state.hwnd, hwnd);
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
                    self.render()?;
                    ValidateRect(Some(self.state.hwnd), None).ok()?;
                    LRESULT(0)
                }
                WM_SIZE => {
                    self.width = loword(lparam.0);
                    self.height = hiword(lparam.0);
                    log::info!("WM_SIZE: width = {}, height = {}", self.width, self.height);
                    LRESULT(0)
                }
                WM_DISPLAYCHANGE => {
                    log::info!("WM_DISPLAYCHANGE");
                    self.render()?;
                    LRESULT(0)
                }
                WM_HOTKEY => {
                    let hotkey_id = wparam.0;
                    if hotkey_id == EXIT_PRIVATE_SCREEN_HOTKEY_ID {
                        log::warn!(
                            "Exit private screen hotkey pressed: hwnd = {:?}",
                            self.state.hwnd
                        );
                        // Handle exit private screen hotkey
                        // For example, hide the window
                        self.hide_window()?;
                    }
                    LRESULT(0)
                }
                WM_DESTROY => {
                    log::warn!("WM_DESTROY");
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(self.state.hwnd, message, wparam, lparam),
            }
        };
        Ok(result)
    }

    /// Render the window content using Direct2D
    /// see https://learn.microsoft.com/zh-cn/windows/win32/direct2d/how-to--draw-text
    /// see https://github.com/microsoft/Windows-classic-samples/blob/main/Samples/Win7Samples/multimedia/Direct2D/SimpleDirect2DApplication/SimpleDirect2dApplication.cpp
    fn render(&mut self) -> Result<LRESULT, DeskError> {
        if self.width == 0 || self.height == 0 {
            log::warn!("Window size is zero, skipping render");
            return Ok(LRESULT(0));
        }

        let rendertargetproperties = D2D1_RENDER_TARGET_PROPERTIES::default();
        let hwndrendertargetproperties = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: self.state.hwnd,
            pixelSize: D2D_SIZE_U {
                width: self.width as _,
                height: self.height as _,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        unsafe {
            if self.hwnd_render_target.is_none() {
                let hwnd_render_target = self
                    .factory
                    .CreateHwndRenderTarget(&rendertargetproperties, &hwndrendertargetproperties)?;

                let black_brush = hwnd_render_target.CreateSolidColorBrush(
                    &D2D1_COLOR_F {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    },
                    None,
                )?;

                self.hwnd_render_target = Some(hwnd_render_target);
                self.black_brush = Some(black_brush);
            }
            let target = self.hwnd_render_target.as_ref().unwrap();
            let black_brush = self.black_brush.as_ref().unwrap();
            let render_target_size = target.GetSize();
            if render_target_size.width as isize != self.width
                || render_target_size.height as isize != self.height
            {
                log::info!(
                    "Resizing render target to width = {}, height = {}",
                    self.width,
                    self.height
                );
                target.Resize(&D2D_SIZE_U {
                    width: self.width as _,
                    height: self.height as _,
                })?;
            }

            target.BeginDraw();
            target.SetTransform(&Matrix3x2::identity());

            target.Clear(Some(&D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            }));
            let hotkey_display = "Ctrl+Alt+L";
            let content = t!("private_screen_tip_content", hotkey = hotkey_display);
            let content_str = format!("{}\0", content.to_string())
                .encode_utf16()
                .collect::<Vec<u16>>();

            target.DrawText(
                content_str.as_slice(),
                &self.format,
                &D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: render_target_size.width,
                    bottom: render_target_size.height,
                },
                black_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );

            target.EndDraw(None, None)?;
        }
        Ok(LRESULT(0))
    }

    /// Create text format for drawing text
    /// see https://github.com/microsoft/windows-rs/blob/3a454d71bc091c20181415bdcf21371bd15ff74d/crates/samples/windows/dcomp/src/main.rs
    fn create_text_format() -> Result<IDWriteTextFormat, DeskError> {
        unsafe {
            let factory: IDWriteFactory2 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let format = factory.CreateTextFormat(
                w!("Candara"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                CARD_HEIGHT / 2.0,
                w!("en"),
            )?;

            format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            Ok(format)
        }
    }
}

impl Drop for PrivateScreenWindow {
    fn drop(&mut self) {
        unsafe {
            log::info!("Dropping PrivateScreenWindow: {:?}", self);
            // Clean up resources
            if !self.state.hwnd.is_invalid() {
                // Destroy the window
                let result = DestroyWindow(self.state.hwnd);
                if let Err(err) = result {
                    log::error!("DestroyWindow error: {:?}", err);
                }
                self.state.hwnd = HWND::default();
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
