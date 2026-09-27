//! Unix limits, applied in the forked child before `exec`, so they are in
//! force before the sandbox reads its request.
use super::SandboxLimits;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command};

/// Unix limits live in the child itself; nothing is held by the parent.
pub struct Containment;

pub fn before_spawn(command: &mut Command, limits: SandboxLimits) {
    // SAFETY: the closure runs between fork and exec and only calls
    // `setrlimit`, which is async-signal-safe, on plain integers.
    unsafe {
        command.pre_exec(move || {
            limit(libc::RLIMIT_CPU, limits.cpu_seconds)?;
            // No core dumps and no regular-file writes: the child only uses its
            // pipes.
            limit(libc::RLIMIT_CORE, 0)?;
            limit(libc::RLIMIT_FSIZE, 0)?;
            // macOS accepts but does not enforce an address-space limit; the
            // CPU and wall-clock limits still bound the child there.
            let memory = limit(libc::RLIMIT_AS, limits.memory_bytes);
            if cfg!(target_os = "linux") {
                memory?;
            }
            Ok(())
        });
    }
}

pub fn after_spawn(_child: &Child, _limits: SandboxLimits) -> Result<Containment, String> {
    Ok(Containment)
}

#[allow(clippy::unnecessary_cast)]
fn limit(resource: LimitResource, value: u64) -> std::io::Result<()> {
    let value = value as libc::rlim_t;
    let bound = libc::rlimit {
        rlim_cur: value,
        rlim_max: value,
    };
    // SAFETY: `bound` is a valid, initialized rlimit for the duration of the call.
    if unsafe { libc::setrlimit(resource, &bound) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
type LimitResource = libc::__rlimit_resource_t;
#[cfg(not(target_os = "linux"))]
type LimitResource = libc::c_int;
