//! Windows display-change watcher implementation.
//!
//! Spawns a dedicated thread that hosts a hidden top-level window. The
//! window's `WndProc` posts a [`DisplayChangeEvent`] onto an unbounded
//! mpsc whenever Windows sends `WM_DISPLAYCHANGE` (or any broadcast
//! message that signals display reconfiguration).
//!
//! ## Why top-level instead of `HWND_MESSAGE`
//!
//! `WM_DISPLAYCHANGE` is a broadcast message — the USER subsystem
//! dispatches it to top-level windows owned by the desktop, **not** to
//! message-only windows (those parented to `HWND_MESSAGE`). The window
//! is therefore a normal `WS_OVERLAPPEDWINDOW` without `WS_VISIBLE`,
//! never shown.
//!
//! ## Shutdown
//!
//! Drop posts `WM_CLOSE` to the window. The wnd-proc calls
//! `DestroyWindow`, which in turn dispatches `WM_DESTROY`; the
//! wnd-proc then calls `PostQuitMessage(0)`. Only then does
//! `GetMessageW` return 0 and the message-pump thread exit. Skipping
//! `PostQuitMessage(0)` would leave the thread blocked in
//! `GetMessageW` and `join()` would hang.
//!
//! ## Class name uniqueness
//!
//! Each `spawn()` derives a class name like
//! `LcxlDisplayWatcher-<pid>-<n>` where `n` is a process-wide
//! monotonic counter. This prevents `ERROR_CLASS_ALREADY_EXISTS` when
//! the same process spawns multiple watchers serially (e.g. during a
//! worker restart inside a portable host).

use std::ffi::c_void;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, HMENU, MSG, PostMessageW, PostQuitMessage,
    RegisterClassExW, SetWindowLongPtrW, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY,
    WM_DISPLAYCHANGE, WM_NCCREATE, WM_NCDESTROY, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::PCWSTR;

use super::error::DisplayWatcherError;

/// Process-wide monotonic counter used to derive unique window class
/// names. Same process can call `spawn()` multiple times serially
/// without colliding on `RegisterClassExW`.
static CLASS_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Event emitted whenever the OS reports a display reconfiguration.
/// `seq` is a monotonic per-watcher counter — useful for test
/// observability and for distinguishing "we missed N events" from
/// "channel went silent".
#[derive(Debug, Clone, Copy)]
pub struct DisplayChangeEvent {
    pub seq: u64,
}

/// Per-watcher state stashed inside the hidden window via
/// `GWLP_USERDATA`. The wnd-proc reads it on each `WM_DISPLAYCHANGE`
/// to push events into the channel.
///
/// **Lifetime:** allocated as `Box<WatcherState>` when the worker
/// thread builds the window, raw-pointer-stashed in `GWLP_USERDATA`
/// during `WM_NCCREATE`, and reclaimed in `WM_NCDESTROY` so it does
/// not leak.
struct WatcherState {
    tx: mpsc::UnboundedSender<DisplayChangeEvent>,
    seq: AtomicU64,
}

/// Handle to the live watcher. Drop tears down the hidden window and
/// joins the message-pump thread.
pub struct DisplayChangeWatcher {
    /// Window handle stored as `isize` so the field is `Send + Sync`
    /// across the thread boundary (raw `HWND` is `*mut c_void`).
    /// Written by the worker thread after `CreateWindowExW` succeeds;
    /// Drop swaps it to 0 and posts `WM_CLOSE`. A 0 value means
    /// "window already destroyed" — Drop is a no-op for it.
    hwnd: Arc<AtomicIsize>,
    /// `Option` so Drop can `take()` without leaving a dangling join
    /// handle behind.
    join: Option<JoinHandle<()>>,
}

impl Drop for DisplayChangeWatcher {
    fn drop(&mut self) {
        let raw = self.hwnd.swap(0, Ordering::AcqRel);
        if raw != 0 {
            // SAFETY: `raw` was written by the worker thread after a
            // successful `CreateWindowExW`. Posting `WM_CLOSE` triggers
            // the documented shutdown chain (see module-level doc).
            unsafe {
                let hwnd = HWND(raw as *mut c_void);
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Spawn the watcher. Returns the live handle and the receiver end of
/// the event channel. On failure, returns a `DisplayWatcherError` —
/// callers should `warn!` and substitute a dummy receiver so the
/// worker keeps running with explicit triggers only.
pub fn spawn() -> Result<
    (
        DisplayChangeWatcher,
        mpsc::UnboundedReceiver<DisplayChangeEvent>,
    ),
    DisplayWatcherError,
> {
    spawn_with_runner(real_runner)
}

/// Internal orchestration: spawn a thread running `runner`, wait for
/// the init signal, return the handle on success. Factored out so
/// tests can substitute a mock runner.
fn spawn_with_runner<R>(
    runner: R,
) -> Result<
    (
        DisplayChangeWatcher,
        mpsc::UnboundedReceiver<DisplayChangeEvent>,
    ),
    DisplayWatcherError,
>
where
    R: FnOnce(
            mpsc::UnboundedSender<DisplayChangeEvent>,
            Arc<AtomicIsize>,
            std::sync::mpsc::Sender<Result<(), DisplayWatcherError>>,
        ) + Send
        + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), DisplayWatcherError>>();
    let hwnd = Arc::new(AtomicIsize::new(0));
    let hwnd_for_thread = Arc::clone(&hwnd);

    let join = std::thread::Builder::new()
        .name("display-watcher".to_string())
        .spawn(move || runner(tx, hwnd_for_thread, init_tx))
        .map_err(DisplayWatcherError::SpawnThread)?;

    match init_rx.recv() {
        Ok(Ok(())) => Ok((
            DisplayChangeWatcher {
                hwnd,
                join: Some(join),
            },
            rx,
        )),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(DisplayWatcherError::ThreadDiedBeforeInit)
        }
    }
}

/// Real production runner: registers a unique window class, creates a
/// hidden top-level window, signals init success, and runs the message
/// pump until `WM_QUIT`.
fn real_runner(
    tx: mpsc::UnboundedSender<DisplayChangeEvent>,
    hwnd_out: Arc<AtomicIsize>,
    init_tx: std::sync::mpsc::Sender<Result<(), DisplayWatcherError>>,
) {
    let class_id = CLASS_COUNTER.fetch_add(1, Ordering::Relaxed);
    let class_name_owned: Vec<u16> =
        format!("LcxlDisplayWatcher-{}-{}", std::process::id(), class_id)
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

    // SAFETY: `GetModuleHandleW(None)` returns the current process's
    // executable handle; used as `hInstance` for class registration.
    let hinstance: HINSTANCE = match unsafe { GetModuleHandleW(None) } {
        Ok(h) => h.into(),
        Err(e) => {
            let _ = init_tx.send(Err(DisplayWatcherError::RegisterClass(io::Error::other(
                format!("GetModuleHandleW: {e}"),
            ))));
            return;
        }
    };

    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance,
        lpszClassName: PCWSTR(class_name_owned.as_ptr()),
        ..Default::default()
    };

    // SAFETY: WNDCLASSEXW is fully initialised above.
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        let err = io::Error::last_os_error();
        let _ = init_tx.send(Err(DisplayWatcherError::RegisterClass(err)));
        return;
    }

    // Allocate the state behind a raw pointer so the WndProc can stash
    // it in GWLP_USERDATA. Ownership transfers to the window; the
    // WM_NCDESTROY arm in wnd_proc reclaims it.
    let state = Box::new(WatcherState {
        tx,
        seq: AtomicU64::new(0),
    });
    let state_ptr = Box::into_raw(state);

    // SAFETY: window-name / class-name UTF-16 buffers stay alive until
    // CreateWindowExW returns (owned in this stack frame); state_ptr
    // is passed via lpCreateParams and unboxed in WM_NCCREATE.
    let window_name: Vec<u16> = "LcxlDisplayWatcherWindow\0".encode_utf16().collect();
    let hwnd_res = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name_owned.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WS_OVERLAPPEDWINDOW, // never shown — WS_VISIBLE not set
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None, // top-level (NOT HWND_MESSAGE)
            None::<HMENU>,
            Some(hinstance),
            Some(state_ptr as *const c_void),
        )
    };
    let hwnd = match hwnd_res {
        Ok(h) => h,
        Err(e) => {
            // Reclaim the Box — CreateWindowExW never ran WM_NCCREATE.
            // SAFETY: state_ptr was just Box::into_raw'd and ownership
            // has not transferred to a window.
            unsafe {
                drop(Box::from_raw(state_ptr));
            }
            let _ = init_tx.send(Err(DisplayWatcherError::CreateWindow(io::Error::other(
                format!("{e}"),
            ))));
            return;
        }
    };

    hwnd_out.store(hwnd.0 as isize, Ordering::Release);
    if init_tx.send(Ok(())).is_err() {
        // The parent dropped the receiver — tear down immediately.
        // SAFETY: same hwnd we just created.
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        return;
    }

    // Standard Win32 message pump. Exits when `GetMessageW` returns 0
    // (= a WM_QUIT was retrieved), which only happens after
    // PostQuitMessage(0) in our WM_DESTROY handler.
    let mut msg = MSG::default();
    loop {
        // SAFETY: msg is a stack-allocated MSG; passing &mut msg satisfies
        // the API's out-param contract.
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 == 0 {
            break;
        }
        if ret.0 == -1 {
            // GetMessageW failed (rare — invalid hwnd). Tear down.
            log::warn!(
                "display-watcher: GetMessageW failed: {}",
                io::Error::last_os_error()
            );
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Static WndProc for the hidden window. Handles the four messages
/// we care about and forwards everything else to `DefWindowProcW`.
///
/// # Safety
///
/// Win32 invokes this directly. Argument validity is guaranteed by the
/// OS. The function never panics — `state_ptr` reads are guarded with
/// `is_null()` checks because Windows can call WndProc with messages
/// before WM_NCCREATE has run (rarely, e.g. WM_GETMINMAXINFO).
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // Stash the state pointer that came in via lpCreateParams.
            // SAFETY: WM_NCCREATE's lparam is `*const CREATESTRUCTW`
            // per Win32 contract; lpCreateParams is the value we
            // passed to CreateWindowExW (our state_ptr).
            let cs = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_DISPLAYCHANGE => {
            // SAFETY: GWLP_USERDATA holds the state_ptr we stashed in
            // WM_NCCREATE. May be 0 if Windows sent this message
            // before WM_NCCREATE (extremely unlikely for top-level
            // windows but guarded for robustness).
            let raw = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WatcherState;
            if !raw.is_null() {
                let state = unsafe { &*raw };
                let seq = state.seq.fetch_add(1, Ordering::SeqCst);
                // Send is best-effort: receiver may be gone if the
                // worker is mid-shutdown. Drop silently.
                let _ = state.tx.send(DisplayChangeEvent { seq });
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // SAFETY: hwnd is the window we own.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // ⚠️ CRITICAL: PostQuitMessage(0) is what eventually
            // unblocks GetMessageW in the message pump. Without it the
            // pump would block forever and join() would hang.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Reclaim the Box now that the OS will never call us
            // again for this hwnd.
            // SAFETY: state_ptr was Box::into_raw'd in real_runner and
            // ownership transferred to the window via WM_NCCREATE.
            let raw = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) } as *mut WatcherState;
            if !raw.is_null() {
                unsafe {
                    drop(Box::from_raw(raw));
                }
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock-runner test: when the runner signals init failure,
    /// `spawn_with_runner` propagates the error and `join()`s cleanly
    /// (no hang). Exercises the orchestration without touching Win32.
    #[test]
    fn spawn_with_runner_returns_err_when_runner_reports_failure() {
        let result = spawn_with_runner(|_tx, _hwnd, init_tx| {
            let _ = init_tx.send(Err(DisplayWatcherError::CreateWindow(io::Error::other(
                "synthetic CreateWindowExW failure",
            ))));
        });
        match result {
            Err(DisplayWatcherError::CreateWindow(_)) => {}
            Err(e) => panic!("expected CreateWindow err, got {e}"),
            Ok(_) => panic!("expected CreateWindow err, got Ok"),
        }
    }

    /// Mock-runner test: when the thread terminates without sending
    /// init_tx, `spawn_with_runner` reports `ThreadDiedBeforeInit`
    /// rather than hanging on the recv.
    #[test]
    fn spawn_with_runner_returns_thread_died_when_no_init_signal() {
        let result = spawn_with_runner(|_tx, _hwnd, _init_tx| {
            // intentionally drop init_tx without sending — caller
            // sees a closed channel.
        });
        match result {
            Err(DisplayWatcherError::ThreadDiedBeforeInit) => {}
            Err(e) => panic!("expected ThreadDiedBeforeInit, got {e}"),
            Ok(_) => panic!("expected ThreadDiedBeforeInit, got Ok"),
        }
    }

    /// Mock-runner test: a runner that signals Ok(()) and then sits
    /// idle yields a live watcher. Drop posts WM_CLOSE — but since the
    /// runner never created a real window, hwnd is still 0, so Drop
    /// short-circuits the PostMessageW path and just join()s the
    /// thread (which we make exit on a oneshot to keep the test fast).
    #[test]
    fn spawn_with_runner_returns_handle_on_init_success() {
        let (exit_tx, exit_rx) = std::sync::mpsc::channel::<()>();
        let result = spawn_with_runner(move |_tx, _hwnd, init_tx| {
            let _ = init_tx.send(Ok(()));
            let _ = exit_rx.recv();
        });
        let (watcher, _rx) = match result {
            Ok(pair) => pair,
            Err(e) => panic!("expected Ok, got {e}"),
        };
        // Drive the worker thread to exit so Drop's join() is fast.
        drop(exit_tx);
        drop(watcher);
    }
}
