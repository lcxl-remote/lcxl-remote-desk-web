//! Windows application discovery and process lifetime validation.

use super::*;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, IsWindowVisible};
use windows::core::BOOL;

pub(crate) fn resolve_foreground_application() -> Result<WindowsForegroundApplication, AgentError> {
    let hwnd = unsafe { GetForegroundWindow() };
    application_by_window(hwnd.0 as isize)
}

/// Application references bind a process lifetime, independently of any window.
pub(crate) fn application_by_process(
    process_id: u32,
) -> Result<WindowsForegroundApplication, AgentError> {
    require_current_session(process_id)?;
    let started = process_start(process_id)
        .ok_or_else(|| failure("the selected application process disappeared", false))?;
    let image_path = process_image(process_id)
        .ok_or_else(|| failure("cannot resolve the selected application image", false))?;
    if process_start(process_id) != Some(started) {
        return Err(failure("the selected application restarted", false));
    }
    Ok(WindowsForegroundApplication {
        window_handle: 0,
        process_id,
        image_path,
        process_started_at: started,
    })
}

pub(crate) fn application_by_window(
    window_handle: isize,
) -> Result<WindowsForegroundApplication, AgentError> {
    let hwnd = HWND(window_handle as *mut std::ffi::c_void);
    if hwnd.0.is_null() {
        return Err(failure("the selected window disappeared", true));
    }
    let mut host_process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut host_process_id)) };
    require_current_session(host_process_id)?;
    let host_started = process_start(host_process_id)
        .ok_or_else(|| failure("cannot identify the window host lifetime", false))?;
    let host_image_path = process_image(host_process_id)
        .ok_or_else(|| failure("cannot resolve the foreground process image", true))?;
    let (process_id, image_path) = if executable_name(&host_image_path)
        .eq_ignore_ascii_case("ApplicationFrameHost.exe")
    {
        let _com = ComGuard::initialize()?;
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| failure("Windows UI Automation is unavailable", true))?;
        let root = unsafe { automation.ElementFromHandle(hwnd) }
            .map_err(|_| failure("the foreground window has no UI Automation root", true))?;
        let walker = unsafe { automation.ControlViewWalker() }
            .map_err(|_| failure("cannot create a UI Automation tree walker", true))?;
        let mut candidates = Vec::new();
        let mut visited = 0usize;
        collect_hosted_window_processes(
            &root,
            &walker,
            host_process_id,
            0,
            Instant::now() + HARD_DEADLINE,
            &mut visited,
            &mut candidates,
        );
        candidates.sort_unstable();
        candidates.dedup();
        if candidates.len() != 1 {
            return Err(failure(
                "the hosted foreground window does not resolve to exactly one application process",
                false,
            ));
        }
        let process_id = candidates[0];
        let image_path = process_image(process_id).ok_or_else(|| {
            failure(
                "cannot resolve the hosted foreground application image",
                false,
            )
        })?;
        (process_id, image_path)
    } else {
        (host_process_id, host_image_path)
    };
    let process_started_at = process_start(process_id).ok_or_else(|| {
        failure(
            "cannot bind the foreground application to its process incarnation",
            false,
        )
    })?;
    require_current_session(process_id)?;
    let mut current_host = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut current_host)) };
    if current_host != host_process_id || process_start(current_host) != Some(host_started) {
        return Err(failure(
            "the selected window host changed during inspection",
            false,
        ));
    }
    Ok(WindowsForegroundApplication {
        window_handle: hwnd.0 as isize,
        process_id,
        image_path,
        process_started_at,
    })
}

fn require_current_session(process_id: u32) -> Result<(), AgentError> {
    let mut expected = 0;
    let mut actual = 0;
    unsafe {
        ProcessIdToSessionId(std::process::id(), &mut expected)
            .and_then(|_| ProcessIdToSessionId(process_id, &mut actual))
    }
    .map_err(|_| failure("cannot identify the selected application session", false))?;
    validate_session(expected, actual)
}

fn validate_session(expected: u32, actual: u32) -> Result<(), AgentError> {
    if expected == 0 || actual != expected {
        return Err(failure(
            "the selected application is outside the interactive session",
            false,
        ));
    }
    Ok(())
}

/// Enumerate only window metadata; do not project unselected application contents.
pub(crate) fn running_applications() -> Result<(Vec<WindowsForegroundApplication>, bool), AgentError>
{
    super::super::native_ui_identity::run(|| {
        let mut windows = WindowEnumeration::default();
        unsafe {
            EnumWindows(
                Some(collect_window),
                LPARAM((&mut windows as *mut WindowEnumeration) as isize),
            )
        }
        .map_err(|_| failure("cannot enumerate application windows", false))?;
        let mut applications = Vec::new();
        let deadline = Instant::now() + HARD_DEADLINE;
        for window in windows.handles {
            if Instant::now() >= deadline {
                windows.truncated = true;
                break;
            }
            if let Ok(application) = application_by_window(window) {
                applications.push(application);
            }
        }
        applications.sort_by(|a, b| {
            (&a.image_path, a.process_id, a.window_handle).cmp(&(
                &b.image_path,
                b.process_id,
                b.window_handle,
            ))
        });
        Ok((applications, windows.truncated))
    })
}

pub(crate) fn application_for_element(
    process_id: u32,
    image_path: &str,
    fingerprint: &str,
) -> Result<WindowsForegroundApplication, AgentError> {
    let image_path = image_path.to_owned();
    let fingerprint = fingerprint.to_owned();
    super::super::native_ui_identity::run(move || {
        require_current_session(process_id)?;
        let started = process_start(process_id)
            .ok_or_else(|| failure("the selected UI process is unavailable", false))?;
        let mut element = retained_element(&fingerprint, process_id, started)?;
        let _com = ComGuard::initialize()?;
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| failure("Windows UI Automation is unavailable", false))?;
        let walker = unsafe { automation.ControlViewWalker() }
            .map_err(|_| failure("cannot resolve the selected element window", false))?;
        for _ in 0..64 {
            if let Ok(hwnd) = unsafe { element.CurrentNativeWindowHandle() }
                && !hwnd.0.is_null()
            {
                let root = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetAncestor(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::GA_ROOT,
                    )
                };
                let application = application_by_window(root.0 as isize)?;
                if application.process_id == process_id
                    && application.process_started_at == started
                    && path_eq(&application.image_path, &image_path)
                {
                    return Ok(application);
                }
                return Err(failure(
                    "the selected UI element no longer belongs to its application",
                    false,
                ));
            }
            element = unsafe { walker.GetParentElement(&element) }
                .map_err(|_| failure("the selected UI element has no live window", false))?;
        }
        Err(failure(
            "the selected element window search exceeded its bounds",
            false,
        ))
    })
}

#[derive(Default)]
struct WindowEnumeration {
    handles: Vec<isize>,
    truncated: bool,
}

impl WindowEnumeration {
    fn push(&mut self, handle: isize) {
        if self.handles.len() < 4096 {
            self.handles.push(handle);
        } else {
            self.truncated = true;
        }
    }
}

unsafe extern "system" fn collect_window(hwnd: HWND, context: LPARAM) -> BOOL {
    // EnumWindows invokes this callback synchronously while the vector is alive.
    let windows = unsafe { &mut *(context.0 as *mut WindowEnumeration) };
    if unsafe { IsWindowVisible(hwnd) }.as_bool() {
        windows.push(hwnd.0 as isize);
    }
    BOOL(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_reports_omitted_windows() {
        let mut enumeration = WindowEnumeration::default();
        for handle in 1..=4096 {
            enumeration.push(handle);
        }
        assert!(!enumeration.truncated);
        enumeration.push(4097);
        assert!(enumeration.truncated);
        assert_eq!(enumeration.handles.len(), 4096);
    }

    #[test]
    fn application_session_rejects_service_and_other_user_sessions() {
        assert!(validate_session(0, 0).is_err());
        assert!(validate_session(1, 0).is_err());
        assert!(validate_session(1, 2).is_err());
        assert!(validate_session(2, 2).is_ok());
    }

    #[test]
    fn missing_window_never_falls_back_to_foreground() {
        assert!(application_by_window(0).is_err());
    }
}

fn executable_name(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

#[allow(clippy::too_many_arguments)]
fn collect_hosted_window_processes(
    element: &IUIAutomationElement,
    walker: &IUIAutomationTreeWalker,
    host_process_id: u32,
    depth: u16,
    deadline: Instant,
    visited: &mut usize,
    candidates: &mut Vec<u32>,
) {
    if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
        return;
    }
    *visited += 1;
    let process_id = unsafe { element.CurrentProcessId() }
        .unwrap_or_default()
        .max(0) as u32;
    let control_type = unsafe { element.CurrentControlType() }
        .map(|value| value.0)
        .unwrap_or_default();
    if process_id != 0 && process_id != host_process_id && control_type == UIA_WindowControlTypeId.0
    {
        candidates.push(process_id);
    }
    if depth >= ACTION_MAX_DEPTH {
        return;
    }
    let Ok(mut child) = (unsafe { walker.GetFirstChildElement(element) }) else {
        return;
    };
    loop {
        collect_hosted_window_processes(
            &child,
            walker,
            host_process_id,
            depth + 1,
            deadline,
            visited,
            candidates,
        );
        if *visited >= ACTION_MAX_NODES || Instant::now() >= deadline {
            return;
        }
        let Ok(next) = (unsafe { walker.GetNextSiblingElement(&child) }) else {
            return;
        };
        child = next;
    }
}
