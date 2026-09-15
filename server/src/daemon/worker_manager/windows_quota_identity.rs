//! Quota identity comes from the daemon-owned worker process, not request data.
use super::*;
use windows::Win32::{
    Foundation::{HANDLE, WAIT_TIMEOUT},
    System::{
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{GetCurrentProcess, GetProcessId, WaitForSingleObject},
    },
};

fn live_session(process: HANDLE, expected_session: u32) -> bool {
    if expected_session == 0 || unsafe { WaitForSingleObject(process, 0) } != WAIT_TIMEOUT {
        return false;
    }
    let pid = unsafe { GetProcessId(process) };
    let mut session = 0;
    pid != 0
        && unsafe { ProcessIdToSessionId(pid, &mut session) }.is_ok()
        && session == expected_session
        // The handle pins the original process object. If it exits during the
        // PID-based session lookup, do not accept a potentially reused PID.
        && unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT
}

pub(super) fn worker_user(worker: &WorkerHandle) -> Option<String> {
    if worker.session_id == 0
        || worker
            .desktop_name
            .as_deref()
            .is_some_and(|name| !name.eq_ignore_ascii_case("Default"))
    {
        return None;
    }
    let process = match worker.process_handle.as_ref() {
        Some(ProcessHandle::WindowsNative(process)) => process.raw_handle(),
        None if worker
            .inprocess_task
            .as_ref()
            .is_some_and(|task| !task.is_finished()) =>
        unsafe { GetCurrentProcess() },
        _ => return None,
    };
    if !live_session(process, worker.session_id) {
        return None;
    }
    let user = desk_file_recovery::windows::process_user_sid(process).ok()?;
    if !live_session(process, worker.session_id) {
        return None;
    }
    // Restricted system workers must never reserve a user's recovery namespace.
    if matches!(user.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        None
    } else {
        Some(user)
    }
}
