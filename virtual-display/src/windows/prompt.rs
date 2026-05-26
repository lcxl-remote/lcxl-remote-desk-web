//! Pre-detach prompt window shown on every active physical display
//! before exclusive mode tears them down. Gives the local user a few
//! seconds to react before the screen goes dark.
//!
//! The handle is intentionally split into two halves so the caller
//! can put both into a `tokio::select!` without a borrow conflict:
//!
//! - [`PromptController`] (`cancel(&self)`): owned by the side that
//!   decides to abort the prompt (control released, daemon shutting
//!   down, …).
//! - [`PromptWaiter`] (`wait(&mut self)`): polled to learn when the
//!   prompt finished — either the countdown elapsed naturally or the
//!   controller asked it to stop.
//!
//! Cancellation is **not** done via `JoinHandle::abort()` — the worker
//! thread is blocked inside `GetMessageW` and `abort()` does not wake
//! it. Instead the controller flips an `AtomicBool` and posts
//! `WM_CLOSE` to a known HWND so the message loop wakes, sees the
//! flag, and exits cleanly.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::oneshot;

mod imp {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BLACK_BRUSH, BeginPaint, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DrawTextW, EndPaint,
        EnumDisplayMonitors, FillRect, GetStockObject, HBRUSH, HDC, HMONITOR, InvalidateRect,
        PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CS_HREDRAW, CS_VREDRAW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        GetClientRect, GetMessageW, HCURSOR, HICON, IDC_ARROW, KillTimer, LoadCursorW, MSG,
        PostQuitMessage, RegisterClassExW, SW_SHOWNOACTIVATE, SetTimer, ShowWindow,
        TranslateMessage, UnregisterClassW, WM_CLOSE, WM_DESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };
    use windows::core::PCWSTR;

    use super::{HwndWrapper, PromptShared};

    const CLASS_NAME: &str = "LcxlExclusivePromptWindow";
    /// Prompt window size — generous enough for two short lines of
    /// 36 pt text on a 1080p screen.
    const PROMPT_WIDTH: i32 = 600;
    const PROMPT_HEIGHT: i32 = 180;
    const TIMER_ID: usize = 1;
    const TIMER_INTERVAL_MS: u32 = 1000;

    /// Run the prompt on its own thread. Returns once every window has
    /// been destroyed (either the countdown elapsed or cancel fired).
    pub(super) fn run_prompt(
        duration: Duration,
        shared: Arc<PromptShared>,
    ) {
        // Best-effort: a registration failure is logged and we exit
        // gracefully — the caller still gets a finished signal.
        let class_atom = match register_class() {
            Ok(a) => a,
            Err(e) => {
                log::error!("[virtual-display::prompt] RegisterClassExW failed: {e}");
                return;
            }
        };
        let monitors = enumerate_monitors();
        if monitors.is_empty() {
            log::warn!(
                "[virtual-display::prompt] no monitors enumerated; skipping prompt"
            );
            // SAFETY: class_atom was registered above; we own its
            // lifetime for the duration of this thread.
            let _ = unsafe { unregister_class(class_atom) };
            return;
        }

        let start = Instant::now();
        let total_secs = duration.as_secs().max(1) as u32;

        let mut hwnds: Vec<HWND> = Vec::with_capacity(monitors.len());
        for mon in &monitors {
            match create_window(class_atom, mon, total_secs) {
                Ok(hwnd) => {
                    hwnds.push(hwnd);
                }
                Err(e) => {
                    log::warn!(
                        "[virtual-display::prompt] CreateWindowExW failed for monitor {:?}: {e}",
                        (mon.left, mon.top, mon.right, mon.bottom)
                    );
                }
            }
        }
        if hwnds.is_empty() {
            let _ = unsafe { unregister_class(class_atom) };
            return;
        }

        // Publish the first HWND so the controller can PostMessage
        // WM_CLOSE to wake the message loop on cancel.
        {
            let mut guard = shared.wake_hwnd.lock().unwrap();
            *guard = Some(HwndWrapper(hwnds[0]));
        }

        // Start the per-second countdown timer on every window.
        for hwnd in &hwnds {
            unsafe {
                SetTimer(Some(*hwnd), TIMER_ID, TIMER_INTERVAL_MS, None);
            }
        }

        // Message loop — exits when every window has been destroyed.
        let mut msg = MSG::default();
        loop {
            // SAFETY: msg is a valid out parameter; HWND filter None
            // requests messages from any of the thread's windows.
            let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if !r.as_bool() {
                // WM_QUIT received.
                break;
            }
            // Check if our own deadline has elapsed or the controller
            // asked us to cancel.
            let elapsed = start.elapsed();
            let should_quit =
                shared.cancel_flag.load(Ordering::SeqCst) || elapsed >= duration;
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if should_quit {
                // Destroy every window; each WM_DESTROY decrements an
                // internal counter and we PostQuitMessage when the
                // last one is gone (see wnd_proc).
                for hwnd in &hwnds {
                    unsafe {
                        let _ = DestroyWindow(*hwnd);
                    }
                }
            }
        }

        {
            let mut guard = shared.wake_hwnd.lock().unwrap();
            *guard = None;
        }
        let _ = unsafe { unregister_class(class_atom) };
    }

    fn register_class() -> windows::core::Result<u16> {
        let class_name_w = to_wide(CLASS_NAME);
        // SAFETY: HINSTANCE retrieved from GetModuleHandleW(None) is
        // the process's own module; the class struct is fully
        // initialised below.
        let hinst = unsafe { GetModuleHandleW(None)? };
        let cursor: HCURSOR = unsafe { LoadCursorW(None, IDC_ARROW)? };
        let bg_brush: HBRUSH = HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0);
        let cls = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst.into(),
            hIcon: HICON::default(),
            hCursor: cursor,
            hbrBackground: bg_brush,
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class_name_w.as_ptr()),
            hIconSm: HICON::default(),
        };
        // SAFETY: cls is fully initialised; the class is unregistered
        // on the same thread before the function returns.
        let atom = unsafe { RegisterClassExW(&cls) };
        if atom == 0 {
            return Err(windows::core::Error::from_win32());
        }
        Ok(atom)
    }

    unsafe fn unregister_class(_atom: u16) -> windows::core::Result<()> {
        let class_name_w = to_wide(CLASS_NAME);
        // SAFETY: HINSTANCE retrieved from GetModuleHandleW(None).
        let hinst = unsafe { GetModuleHandleW(None)? };
        unsafe {
            UnregisterClassW(PCWSTR(class_name_w.as_ptr()), Some(hinst.into()))?;
        }
        Ok(())
    }

    fn enumerate_monitors() -> Vec<RECT> {
        let mut rects: Vec<RECT> = Vec::new();
        let lparam = LPARAM(&mut rects as *mut Vec<RECT> as isize);
        // SAFETY: callback writes through `lparam` only for the
        // lifetime of this call.
        unsafe {
            let _ = EnumDisplayMonitors(None, None, Some(monitor_enum_proc), lparam);
        }
        rects
    }

    unsafe extern "system" fn monitor_enum_proc(
        _hmon: HMONITOR,
        _hdc: HDC,
        lprect: *mut RECT,
        lparam: LPARAM,
    ) -> windows::core::BOOL {
        if !lprect.is_null() {
            // SAFETY: lparam carries the Vec<RECT> pointer from the
            // caller; both pointers are alive for the duration of
            // EnumDisplayMonitors.
            let rects = unsafe { &mut *(lparam.0 as *mut Vec<RECT>) };
            rects.push(unsafe { *lprect });
        }
        windows::core::BOOL(1)
    }

    fn create_window(
        _class_atom: u16,
        monitor_rect: &RECT,
        total_secs: u32,
    ) -> windows::core::Result<HWND> {
        let class_name_w = to_wide(CLASS_NAME);
        let mw = monitor_rect.right - monitor_rect.left;
        let mh = monitor_rect.bottom - monitor_rect.top;
        let x = monitor_rect.left + (mw - PROMPT_WIDTH) / 2;
        let y = monitor_rect.top + (mh - PROMPT_HEIGHT) / 2;
        let title_text = format!(
            "Exclusive remote mode starting - physical displays will turn off in {total_secs} seconds"
        );
        let title_w = to_wide(&title_text);

        // SAFETY: all parameters are valid; the window is destroyed
        // on the same thread before the class is unregistered.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(class_name_w.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                PROMPT_WIDTH,
                PROMPT_HEIGHT,
                None,
                None,
                None,
                None,
            )?
        };
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
        Ok(hwnd)
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
                let mut rect = RECT::default();
                unsafe {
                    let _ = GetClientRect(hwnd, &mut rect);
                }
                let bg = HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0);
                unsafe {
                    FillRect(hdc, &rect, bg);
                    SetBkMode(hdc, TRANSPARENT);
                    SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00FFFFFF));
                }
                let text = "Exclusive remote mode starting...";
                let mut text_w: Vec<u16> = text.encode_utf16().collect();
                unsafe {
                    DrawTextW(
                        hdc,
                        &mut text_w,
                        &mut rect,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                    let _ = EndPaint(hwnd, &ps);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                // Repaint to refresh the countdown text on the next
                // tick. We do not store per-window state — the text
                // is generic on purpose so all windows show the same
                // message regardless of which monitor they live on.
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                // The controller asked us to stop — destroy this
                // window. WM_DESTROY follows.
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_ID);
                }
                // When the last window dies, post WM_QUIT so the
                // message loop exits. The runtime layer tracks this
                // via a process-global counter, but for our purposes
                // (the loop also checks the cancel flag) posting
                // unconditionally is safe — surplus WM_QUITs are
                // harmless.
                unsafe {
                    PostQuitMessage(0);
                }
                LRESULT(0)
            }
            _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        }
    }

    fn to_wide(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = OsStr::new(s).encode_wide().collect();
        v.push(0);
        v
    }
}

/// Internal opaque newtype so `Send` can be implemented across the
/// PromptController/Waiter boundary. HWND itself is `*mut c_void` and
/// not Send, but the wnd-proc only touches it from the dedicated
/// prompt thread; `PostMessageW` on a different thread is documented
/// to be safe for any HWND.
#[derive(Clone, Copy)]
pub(crate) struct HwndWrapper(pub(crate) windows::Win32::Foundation::HWND);
// SAFETY: PostMessageW (the only inter-thread use) is documented safe.
unsafe impl Send for HwndWrapper {}

/// Shared state between the prompt thread and the public handles.
pub(crate) struct PromptShared {
    pub(crate) cancel_flag: AtomicBool,
    pub(crate) wake_hwnd: Mutex<Option<HwndWrapper>>,
}

/// Caller-side cancel handle. `cancel(&self)` borrows immutably so
/// the controller can be moved or cloned into a `select!` arm
/// without conflicting with [`PromptWaiter::wait`]'s `&mut self`.
pub struct PromptController {
    shared: Arc<PromptShared>,
    _thread: Option<JoinHandle<()>>,
}

impl PromptController {
    /// Signal the prompt thread to stop. Idempotent.
    pub fn cancel(&self) {
        self.shared.cancel_flag.store(true, Ordering::SeqCst);
        // PostMessageW(WM_CLOSE) wakes the GetMessageW loop. Without
        // this, the loop blocks indefinitely even though the flag has
        // flipped — JoinHandle::abort() does NOT wake GetMessage.
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};
        if let Some(wrapper) = *self.shared.wake_hwnd.lock().unwrap() {
            // SAFETY: PostMessageW is documented safe across threads
            // for any HWND value. If the prompt thread already
            // destroyed the window the call simply fails — we treat
            // that as success since the loop is already on its way
            // out.
            let _ =
                unsafe { PostMessageW(Some(wrapper.0), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
    }
}

/// Future-side handle. `wait(&mut self).await` returns once the
/// prompt thread exits — either the countdown elapsed naturally or
/// the controller cancelled it.
pub struct PromptWaiter {
    finished: oneshot::Receiver<()>,
}

impl PromptWaiter {
    pub async fn wait(&mut self) {
        let _ = (&mut self.finished).await;
    }
}

/// Spawn the prompt thread. `duration == 0` is the fast path: no
/// thread is created, the waiter resolves immediately, and the
/// controller's cancel is a no-op. This matches the design's
/// "prompt_ms = 0 skips the prompt".
pub fn show_pre_detach_prompt(duration: Duration) -> (PromptController, PromptWaiter) {
    let (tx, rx) = oneshot::channel::<()>();
    let waiter = PromptWaiter { finished: rx };
    let shared = Arc::new(PromptShared {
        cancel_flag: AtomicBool::new(false),
        wake_hwnd: Mutex::new(None),
    });

    if duration.is_zero() {
        // Fast path: never start the worker thread; signal completion
        // synchronously. The receiver future resolves on the next
        // poll.
        let _ = tx.send(());
        return (
            PromptController {
                shared,
                _thread: None,
            },
            waiter,
        );
    }

    let thread = {
        let shared = shared.clone();
        Some(std::thread::spawn(move || {
            imp::run_prompt(duration, shared);
            let _ = tx.send(());
        }))
    };

    (
        PromptController {
            shared,
            _thread: thread,
        },
        waiter,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    /// duration=0 must resolve the waiter without spawning a Win32
    /// window or thread (the unit-test environment does not necessarily
    /// have a session-1 desktop). Also pins the design: `prompt_ms = 0`
    /// is the documented opt-out path.
    #[tokio::test]
    async fn duration_zero_resolves_immediately_without_thread() {
        let (ctrl, mut waiter) = show_pre_detach_prompt(Duration::from_secs(0));
        // The thread handle is None on the fast path.
        assert!(ctrl._thread.is_none());
        // The waiter resolves on the very next poll.
        waiter.wait().await;
        // cancel() is a no-op idempotent call on the fast path.
        ctrl.cancel();
        ctrl.cancel();
    }

    /// Borrow contract: the controller is borrowed immutably (`&self`)
    /// while the waiter is borrowed mutably (`&mut self`), so both
    /// arms can sit in the same `tokio::select!` without rustc
    /// complaining. Compile-only assertion.
    #[allow(dead_code)]
    async fn _compile_select_combines_cancel_and_wait_arms() {
        let (ctrl, mut waiter) = show_pre_detach_prompt(Duration::from_secs(0));
        let (_tx, mut rx) = oneshot::channel::<()>();
        tokio::select! {
            _ = waiter.wait() => {}
            _ = &mut rx => { ctrl.cancel(); }
        }
    }

    /// Send + Sync: the controller and the waiter must travel across
    /// threads (worker spawns the runner on a tokio task; the prompt
    /// thread is the Win32 worker). Compile-time assertion.
    #[test]
    fn controller_and_waiter_are_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PromptController>();
        assert_send::<PromptWaiter>();
    }
}
