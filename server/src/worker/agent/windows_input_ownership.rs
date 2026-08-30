//! Windows low-level input ownership monitor.
//!
//! The hook runs inside the interactive SessionWorker. Product-stamped AI and
//! browser `SendInput` events are ignored by this monitor; every other keyboard
//! or mouse event is conservatively external and invalidates observations. The
//! marker is only a loop-avoidance label and never an authorization primitive.

use std::sync::{Arc, Mutex, Weak, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use desk_input_injection::windows_event::{InputSource, classify_input};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_HOOKCONTROL, DESKTOP_READOBJECTS,
    DESKTOP_WRITEOBJECTS, GetThreadDesktop, OpenInputDesktop, SetThreadDesktop,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_QUIT,
};

use super::computer_use_broker::ComputerUseBroker;

static ACTIVE_BROKER: Mutex<Option<Weak<ComputerUseBroker>>> = Mutex::new(None);

pub struct WindowsInputOwnershipMonitor {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl WindowsInputOwnershipMonitor {
    pub fn start(broker: &Arc<ComputerUseBroker>) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let broker = Arc::downgrade(broker);
        let join = thread::Builder::new()
            .name("computer-use-input-owner".into())
            .spawn(move || run_hook_loop(broker, ready_tx))
            .map_err(|error| format!("cannot start Windows input ownership thread: {error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                join: Some(join),
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(error) => Err(format!(
                "Windows input ownership hook did not initialize in time: {error}"
            )),
        }
    }
}

impl Drop for WindowsInputOwnershipMonitor {
    fn drop(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run_hook_loop(broker: Weak<ComputerUseBroker>, ready_tx: mpsc::SyncSender<Result<u32, String>>) {
    {
        let mut active = ACTIVE_BROKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .as_ref()
            .is_some_and(|broker| broker.strong_count() > 0)
        {
            let _ = ready_tx.send(Err(
                "a Windows input ownership monitor is already active in this process".into(),
            ));
            return;
        }
        *active = Some(broker.clone());
    }

    let thread_id = unsafe { GetCurrentThreadId() };
    let original_desktop = match unsafe { GetThreadDesktop(thread_id) } {
        Ok(desktop) => desktop,
        Err(error) => {
            clear_active_broker();
            let _ = ready_tx.send(Err(format!(
                "cannot resolve the Windows input monitor thread desktop: {error}"
            )));
            return;
        }
    };
    let input_desktop = match unsafe {
        OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS::default(),
            false,
            windows::Win32::System::StationsAndDesktops::DESKTOP_ACCESS_FLAGS(
                DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0 | DESKTOP_HOOKCONTROL.0,
            ),
        )
    } {
        Ok(desktop) => desktop,
        Err(error) => {
            clear_active_broker();
            let _ = ready_tx.send(Err(format!(
                "cannot open the active Windows input desktop: {error}"
            )));
            return;
        }
    };
    if let Err(error) = unsafe { SetThreadDesktop(input_desktop) } {
        let _ = unsafe { CloseDesktop(input_desktop) };
        clear_active_broker();
        let _ = ready_tx.send(Err(format!(
            "cannot bind the Windows input monitor to the active desktop: {error}"
        )));
        return;
    }

    let keyboard_hook =
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_callback), None, 0) } {
            Ok(hook) => hook,
            Err(error) => {
                let _ = unsafe { SetThreadDesktop(original_desktop) };
                let _ = unsafe { CloseDesktop(input_desktop) };
                clear_active_broker();
                let _ = ready_tx.send(Err(format!(
                    "cannot install the Windows low-level keyboard hook: {error}"
                )));
                return;
            }
        };
    let mouse_hook =
        match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_callback), None, 0) } {
            Ok(hook) => hook,
            Err(error) => {
                let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
                let _ = unsafe { SetThreadDesktop(original_desktop) };
                let _ = unsafe { CloseDesktop(input_desktop) };
                clear_active_broker();
                let _ = ready_tx.send(Err(format!(
                    "cannot install the Windows low-level mouse hook: {error}"
                )));
                return;
            }
        };

    let mut message = MSG::default();
    // Force creation of the thread message queue before publishing thread_id,
    // otherwise an immediate Drop could race PostThreadMessageW against it.
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };
    if let Some(broker) = broker.upgrade() {
        broker.set_input_ownership_ready(true);
    }
    if ready_tx.send(Ok(thread_id)).is_ok() {
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {}
    }

    if let Some(broker) = broker.upgrade() {
        broker.set_input_ownership_ready(false);
    }

    let _ = unsafe { UnhookWindowsHookEx(mouse_hook) };
    let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
    let _ = unsafe { SetThreadDesktop(original_desktop) };
    let _ = unsafe { CloseDesktop(input_desktop) };
    clear_active_broker();
}

unsafe extern "system" fn keyboard_hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        note_if_external(event.dwExtraInfo, event.flags.contains(LLKHF_INJECTED));
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 {
        let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        note_if_external(event.dwExtraInfo, event.flags & LLMHF_INJECTED != 0);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn note_if_external(extra_info: usize, injected: bool) {
    if classify_input(extra_info, injected) != InputSource::External {
        return;
    }
    let broker = ACTIVE_BROKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade);
    if let Some(broker) = broker {
        broker.note_external_input();
    }
}

fn clear_active_broker() {
    *ACTIVE_BROKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
mod tests {
    use desk_input_injection::windows_event::{AI_INPUT_MARKER, BROWSER_INPUT_MARKER};

    use super::*;

    #[test]
    fn product_markers_do_not_preempt_but_everything_else_does() {
        let broker = Arc::new(ComputerUseBroker::new());
        *ACTIVE_BROKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(&broker));

        note_if_external(AI_INPUT_MARKER, true);
        note_if_external(BROWSER_INPUT_MARKER, true);
        assert_eq!(broker.human_input_epoch(), 0);

        note_if_external(0, false);
        note_if_external(0, true);
        note_if_external(AI_INPUT_MARKER, false);
        assert_eq!(broker.human_input_epoch(), 3);
        clear_active_broker();
    }

    #[test]
    #[ignore = "requires an active interactive Windows desktop"]
    fn hook_observes_unmarked_send_input_and_ignores_browser_marker() {
        use std::mem::size_of;

        use desk_input_injection::windows_event::mark_browser_input;
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            INPUT, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
        };

        fn key(up: bool, mark_browser: bool) -> INPUT {
            let mut input = INPUT::default();
            input.r#type = INPUT_KEYBOARD;
            input.Anonymous.ki.wVk = VIRTUAL_KEY(0x87); // F24: no text side effect.
            input.Anonymous.ki.dwFlags = if up {
                KEYEVENTF_KEYUP
            } else {
                KEYBD_EVENT_FLAGS(0)
            };
            if mark_browser {
                mark_browser_input(&mut input);
            }
            input
        }

        let broker = Arc::new(ComputerUseBroker::new());
        let monitor = WindowsInputOwnershipMonitor::start(&broker)
            .expect("the low-level hooks must start on an active desktop");
        assert!(broker.input_ownership_is_ready());
        let browser = [key(false, true), key(true, true)];
        assert_eq!(
            unsafe { SendInput(&browser, size_of::<INPUT>() as i32) },
            browser.len() as u32
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(broker.human_input_epoch(), 0);

        let external = [key(false, false), key(true, false)];
        assert_eq!(
            unsafe { SendInput(&external, size_of::<INPUT>() as i32) },
            external.len() as u32
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(broker.human_input_epoch(), 2);
        drop(monitor);
        assert!(!broker.input_ownership_is_ready());
    }
}
