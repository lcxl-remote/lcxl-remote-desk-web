//! Validate the selected UIA window before and after independent capture.
use super::*;
use desk_capture_engine::image_capture::windows_window_capture::WindowsWindowCaptureTarget;
use windows::Win32::Foundation::HWND;

pub(crate) fn resolve_window_capture_target(
    process_id: u32,
    image_path: &str,
    fingerprint: &str,
) -> Result<WindowsWindowCaptureTarget, AgentError> {
    let image_path = image_path.to_owned();
    let fingerprint = fingerprint.to_owned();
    super::super::native_ui_identity::run(move || {
        let application = application_for_element(process_id, &image_path, &fingerprint)?;
        let hwnd = HWND(application.window_handle as *mut std::ffi::c_void);
        let mut host_pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut host_pid));
        }
        let host_started = process_start(host_pid)
            .ok_or_else(|| failure("selected window host is unavailable", false))?;
        let _com = ComGuard::initialize()?;
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|_| failure("UI Automation is unavailable", false))?;
        let walker = unsafe { automation.ControlViewWalker() }
            .map_err(|_| failure("cannot validate the selected window root", false))?;
        let root = unsafe { automation.ElementFromHandle(hwnd) }
            .map_err(|_| failure("selected native window disappeared", false))?;
        let mut visited = 0;
        let logical_root = find_process_root(
            &root,
            &walker,
            process_id,
            0,
            Instant::now() + HARD_DEADLINE,
            &mut visited,
        )
        .ok_or_else(|| failure("selected window has no application root", false))?;
        let selected = retained_element(&fingerprint, process_id, application.process_started_at)?;
        if runtime_key(&logical_root)? != runtime_key(&selected)? {
            return Err(failure_with_kind(
                AgentErrorKind::InvalidInput,
                "the selected reference is not the complete native window; select its window root",
                false,
            ));
        }
        if protection::scan(&automation, root)? {
            return Err(failure_with_kind(
                AgentErrorKind::PermissionDenied,
                "the selected window contains a visible protected UI control",
                false,
            ));
        }
        let mut current_host = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut current_host));
        }
        if current_host != host_pid || process_start(host_pid) != Some(host_started) {
            return Err(failure(
                "selected window host changed during validation",
                false,
            ));
        }
        // The retained UIA identity must still resolve after the protection scan.
        retained_element(&fingerprint, process_id, application.process_started_at)?;
        Ok(WindowsWindowCaptureTarget {
            window_handle: application.window_handle,
            host_process_id: host_pid,
            host_process_started_at: host_started,
        })
    })
}
