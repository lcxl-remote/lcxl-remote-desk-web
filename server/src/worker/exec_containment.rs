//! Process-tree reclamation for an executed command.
//!
//! Killing the process we spawned is not the same as stopping what it started.
//! A command that forks, launches a helper, or backgrounds a task leaves those
//! descendants running when the direct child is killed — so a timeout would
//! bound the command we can see while the work it started continues
//! indefinitely. Every execution therefore runs inside a container the whole
//! tree belongs to, and reclaiming the container reclaims the tree.
//!
//! # Two-stage identity
//!
//! Where the platform allows it, the container is created and named *before* the
//! spawn, so a host that crashes mid-spawn can still find and clean up what it
//! started. Where it does not, the identity only exists once there is a pid.
//!
//! | Platform | Named before spawn | Identity |
//! |---|---|---|
//! | Windows | yes — the job object is created first | job name |
//! | Linux / macOS | no — a process group is named by its leader's pid | `pgid:<n>` |
//!
//! On Unix that leaves a window between reserving an execution and knowing how
//! to reclaim it. A host that dies inside that window cannot say what it started,
//! which is why the ledger records such an execution as indeterminate rather than
//! claiming either outcome.
//!
//! # Fail closed
//!
//! If a container cannot be established the command does not run. An execution
//! that cannot be reclaimed is exactly the thing this exists to prevent, and
//! running it anyway would leave a process the host has no way to stop.
//!
//! # Verification status
//!
//! The Unix backend is covered by tests that spawn a real descendant and assert
//! it dies with the command. The Windows backend has been checked against the
//! bound `windows` crate's actual signatures but has **not been compiled or run**
//! — it was written on a host with no Windows target installed. Treat its first
//! Windows build as unverified.

use tokio::process::{Child, Command};

/// Why containment could not be established. The command must not be spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentError(pub String);

impl std::fmt::Display for ContainmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A container holding one execution's process tree.
///
/// Dropping it reclaims the tree, so every exit path — normal return, timeout,
/// error, panic — leaves nothing behind. A command that needs to outlive its own
/// invocation must hand the work to the platform's service manager rather than
/// background a child, which is the distinction this enforces.
pub struct Containment {
    inner: Platform,
    /// Set once the container can be named: before the spawn where the platform
    /// allows, otherwise after.
    identity: Option<String>,
}

impl Containment {
    /// Establish a container for an execution identified by `generation`.
    ///
    /// Called before the spawn. `generation` only names the container for
    /// diagnosis; reclamation never depends on it.
    pub fn prepare(generation: &str) -> Result<Self, ContainmentError> {
        let (inner, identity) = Platform::prepare(generation)?;
        Ok(Self { inner, identity })
    }

    /// How to find this container again, if it can be named yet. `None` on a
    /// platform that cannot name one until the child exists.
    pub fn identity(&self) -> Option<&str> {
        self.identity.as_deref()
    }

    /// Configure the command so its children land inside the container.
    pub fn apply(&self, cmd: &mut Command) {
        self.inner.apply(cmd);
    }

    /// Bind the spawned child to the container and fill in the identity where it
    /// only becomes knowable now.
    ///
    /// A failure here means the child is running but unreclaimable, so the caller
    /// must kill it rather than proceed.
    pub fn adopt(&mut self, child: &Child) -> Result<(), ContainmentError> {
        self.inner.adopt(child)?;
        if let Some(identity) = self.inner.identity_after_adopt() {
            self.identity = Some(identity);
        }
        Ok(())
    }

    /// Reclaim the whole tree now, rather than waiting for the drop.
    pub fn reclaim(&mut self) {
        self.inner.reclaim();
    }
}

impl Drop for Containment {
    fn drop(&mut self) {
        self.inner.reclaim();
    }
}

// ============================ Unix ============================

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use super::*;

    /// A POSIX process group. The child becomes the group leader, so everything it
    /// spawns inherits the group and a single signal reaches all of it.
    ///
    /// Deliberately not cgroups on Linux: creating one requires a delegated subtree
    /// the host usually does not have when it runs as an ordinary user, so it would
    /// fail closed on exactly the common deployment. A process group works
    /// unprivileged. It is escapable — a descendant may call `setsid` — but nothing
    /// short of cgroups prevents that, and the practical leak this closes is the
    /// ordinary child that simply inherits the group.
    pub struct Platform {
        pgid: Option<libc::pid_t>,
    }

    impl Platform {
        pub fn prepare(_generation: &str) -> Result<(Self, Option<String>), ContainmentError> {
            // A process group is named by its leader, which does not exist yet.
            Ok((Self { pgid: None }, None))
        }

        pub fn apply(&self, cmd: &mut Command) {
            // 0 means "new group led by the child", so the group id is the child pid.
            cmd.process_group(0);
        }

        pub fn adopt(&mut self, child: &Child) -> Result<(), ContainmentError> {
            let pid = child.id().ok_or_else(|| {
                ContainmentError("the child exited before it could be contained".into())
            })?;
            self.pgid = Some(pid as libc::pid_t);
            Ok(())
        }

        pub fn identity_after_adopt(&self) -> Option<String> {
            self.pgid.map(|p| format!("pgid:{p}"))
        }

        pub fn reclaim(&mut self) {
            let Some(pgid) = self.pgid.take() else {
                return;
            };
            // Negative pid addresses the whole group. ESRCH (already gone) is the
            // normal case after a clean exit and is not worth reporting.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

// ============================ Windows ============================

#[cfg(target_os = "windows")]
mod imp {
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

    impl Platform {
        pub fn prepare(generation: &str) -> Result<(Self, Option<String>), ContainmentError> {
            // Named so a leaked job is identifiable in a diagnostic tool; the name
            // plays no part in reclamation.
            let name = format!("Local\\LcxlExec-{generation}");
            let job = unsafe { CreateJobObjectW(None, &HSTRING::from(&name)) }
                .map_err(|e| ContainmentError(format!("could not create a job object: {e}")))?;

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
                ContainmentError(format!("could not configure the job object: {e}"))
            })?;

            Ok((Self { job: Some(job) }, Some(name)))
        }

        pub fn apply(&self, _cmd: &mut Command) {
            // Nothing to set before the spawn: assignment needs a process handle.
        }

        pub fn adopt(&mut self, child: &Child) -> Result<(), ContainmentError> {
            let Some(job) = self.job else {
                return Err(ContainmentError("the job object is gone".into()));
            };
            let handle = child.raw_handle().ok_or_else(|| {
                ContainmentError("the child exited before it could be contained".into())
            })?;
            // Assignment happens just after the spawn rather than before it, because
            // a process handle is required. A grandchild created in that instant
            // would escape; since Windows 8 a process may belong to nested jobs, so
            // the child itself always joins even if something else already placed it
            // in a job.
            unsafe { AssignProcessToJobObject(job, HANDLE(handle as *mut core::ffi::c_void)) }
                .map_err(|e| ContainmentError(format!("could not contain the process: {e}")))?;
            Ok(())
        }

        pub fn identity_after_adopt(&self) -> Option<String> {
            // The job name was known before the spawn and has not changed.
            None
        }

        pub fn reclaim(&mut self) {
            let Some(job) = self.job.take() else {
                return;
            };
            unsafe {
                // Terminate first: closing the handle alone relies on this being the
                // last reference, which the child having a handle can violate.
                let _ = TerminateJobObject(job, 1);
                let _ = CloseHandle(job);
            }
        }
    }
}

// ============================ Unsupported ============================

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod imp {
    use super::*;

    /// No containment primitive is wired for this platform, so execution is
    /// refused rather than run unreclaimably.
    pub struct Platform;

    impl Platform {
        pub fn prepare(_generation: &str) -> Result<(Self, Option<String>), ContainmentError> {
            Err(ContainmentError(
                "this platform has no process-tree containment, so execution is refused".into(),
            ))
        }
        pub fn apply(&self, _cmd: &mut Command) {}
        pub fn adopt(&mut self, _child: &Child) -> Result<(), ContainmentError> {
            Err(ContainmentError("unsupported platform".into()))
        }
        pub fn identity_after_adopt(&self) -> Option<String> {
            None
        }
        pub fn reclaim(&mut self) {}
    }
}

use imp::Platform;
