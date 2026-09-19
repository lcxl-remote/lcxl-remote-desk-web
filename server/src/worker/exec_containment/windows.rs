use super::*;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::core::HSTRING;

/// A job object. Every process the child creates joins it automatically, and
/// terminating the job terminates all of them.
///
/// `KILL_ON_JOB_CLOSE` is set as a backstop: if this process dies without
/// reclaiming, the kernel closes the last handle and tears the tree down
/// anyway. That covers the crash case no user-space cleanup can.
pub struct Platform {
    job: Option<HANDLE>,
}

// SAFETY: `job` is a handle to a job object, a process-wide kernel object with
// no thread affinity (unlike GDI or window handles). The Tokio runtime may
// move, poll, and drop this containment on any worker thread, so the handle
// must cross threads. The calls we make on it — `AssignProcessToJobObject`,
// `TerminateJobObject`, `CloseHandle` — are all thread-agnostic kernel calls,
// and ownership is only ever moved, never shared, so `Send` is sound and
// `Sync` is neither needed nor claimed.
unsafe impl Send for Platform {}

impl Platform {
    pub fn prepare(generation: &str) -> Result<(Self, Option<String>), ContainmentError> {
        // Named so a leaked job is identifiable in a diagnostic tool; the name
        // plays no part in reclamation.
        let name = format!("Local\\LcxlExec-{generation}");
        let job = unsafe { CreateJobObjectW(None, &HSTRING::from(&name)) }.map_err(|e| {
            ContainmentError(
                desk_agent_protocol::native_diagnostic::NativeDiagnostic::new(
                    desk_agent_protocol::native_diagnostic::DiagnosticStage::ProcessContainment,
                    "CreateJobObjectW",
                    "hresult",
                    Some(i64::from(e.code().0)),
                    &e.message(),
                ),
            )
        })?;

        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { core::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|e| {
            unsafe {
                let _ = CloseHandle(job);
            }
            ContainmentError(
                desk_agent_protocol::native_diagnostic::NativeDiagnostic::new(
                    desk_agent_protocol::native_diagnostic::DiagnosticStage::ProcessContainment,
                    "SetInformationJobObject",
                    "hresult",
                    Some(i64::from(e.code().0)),
                    &e.message(),
                ),
            )
        })?;

        Ok((Self { job: Some(job) }, Some(name)))
    }

    pub fn apply(&self, _cmd: &mut Command) {
        // Nothing to set before the spawn: assignment needs a process handle.
    }

    pub fn adopt(&mut self, child: &Child) -> Result<(), ContainmentError> {
        let Some(job) = self.job else {
            return Err(ContainmentError::message("the job object is gone"));
        };
        let handle = child.raw_handle().ok_or_else(|| {
            ContainmentError::message("the child exited before it could be contained")
        })?;
        // Assignment happens just after the spawn rather than before it, because
        // a process handle is required. A grandchild created in that instant
        // would escape; since Windows 8 a process may belong to nested jobs, so
        // the child itself always joins even if something else already placed it
        // in a job.
        unsafe { AssignProcessToJobObject(job, HANDLE(handle as *mut core::ffi::c_void)) }
            .map_err(|e| {
                ContainmentError(
                    desk_agent_protocol::native_diagnostic::NativeDiagnostic::new(
                        desk_agent_protocol::native_diagnostic::DiagnosticStage::ProcessContainment,
                        "AssignProcessToJobObject",
                        "hresult",
                        Some(i64::from(e.code().0)),
                        &e.message(),
                    ),
                )
            })?;
        Ok(())
    }

    pub fn identity_after_adopt(&self) -> Option<String> {
        // The job name was known before the spawn and has not changed.
        None
    }

    pub fn reclaim(
        &mut self,
    ) -> Result<(), desk_agent_protocol::native_diagnostic::NativeDiagnostic> {
        let Some(job) = self.job.take() else {
            return Ok(());
        };
        unsafe {
            // Terminate first: closing the handle alone relies on this being the
            // last reference, which the child having a handle can violate.
            let terminated = TerminateJobObject(job, 1);
            let closed = CloseHandle(job);
            terminated.and(closed).map_err(|error| {
                desk_agent_protocol::native_diagnostic::NativeDiagnostic::new(
                    desk_agent_protocol::native_diagnostic::DiagnosticStage::ProcessCleanup,
                    "TerminateJobObject/CloseHandle",
                    "hresult",
                    Some(i64::from(error.code().0)),
                    &error.message(),
                )
            })
        }
    }
}
