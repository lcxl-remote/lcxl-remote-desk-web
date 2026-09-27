//! Windows limits: a job object with a per-process memory cap, a CPU-time cap,
//! a single active process and kill-on-close. The job is assigned right after
//! the spawn, before the parent writes the request the child blocks on, so no
//! Typst work runs outside it.
use super::SandboxLimits;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Owns the job handle; closing it kills the child if it is still running.
pub struct Containment {
    job: HANDLE,
}

// SAFETY: a job object handle has no thread affinity and is only closed once,
// by its single owner.
unsafe impl Send for Containment {}

impl Drop for Containment {
    fn drop(&mut self) {
        // SAFETY: the handle was created by `after_spawn` and is closed once.
        unsafe {
            let _ = CloseHandle(self.job);
        }
    }
}

pub fn before_spawn(command: &mut Command, _limits: SandboxLimits) {
    command.creation_flags(CREATE_NO_WINDOW);
}

pub fn after_spawn(child: &Child, limits: SandboxLimits) -> Result<Containment, String> {
    // SAFETY: an anonymous job with default security attributes.
    let job = unsafe { CreateJobObjectW(None, windows::core::PCWSTR::null()) }
        .map_err(|error| error.message())?;
    let containment = Containment { job };
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    info.BasicLimitInformation.ActiveProcessLimit = 1;
    // Per-process user-mode time, in 100-nanosecond units.
    info.BasicLimitInformation.PerProcessUserTimeLimit =
        (limits.cpu_seconds.saturating_mul(10_000_000)).min(i64::MAX as u64) as i64;
    info.ProcessMemoryLimit = usize::try_from(limits.memory_bytes).unwrap_or(usize::MAX);
    // SAFETY: `info` is a fully initialized structure of the declared size.
    unsafe {
        SetInformationJobObject(
            containment.job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|error| error.message())?;
    // SAFETY: the process handle stays valid while `child` is borrowed.
    unsafe {
        AssignProcessToJobObject(
            containment.job,
            HANDLE(child.as_raw_handle() as *mut core::ffi::c_void),
        )
    }
    .map_err(|error| error.message())?;
    Ok(containment)
}
