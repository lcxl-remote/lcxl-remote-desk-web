//! Resource-limited child process for every Typst compilation and rendering.
//!
//! Typst source is a program, so evaluating an approved `.typ` file (or the
//! Typst generated from Markdown/TXT) may allocate or loop without bound, and a
//! failed allocation aborts the whole process. Default and DeskServer run the
//! worker inside the desktop server, and ServiceDaemon runs it in the session
//! worker, so the work runs in a private subcommand of the same executable
//! instead. The child:
//!
//! - is dispatched before any runtime, logging or configuration starts, and
//!   writes nothing to stdout except one response frame;
//! - has its memory, CPU time and file-size limits applied before it reads its
//!   request (`setrlimit` in the forked child on Unix; a job object assigned
//!   before the request is written on Windows);
//! - is killed after a wall-clock deadline, and killed and reaped when the
//!   parent drops it for any reason;
//! - runs with the worker's own user token. It is a resource boundary, not a
//!   privilege boundary: the source is already an approved file of the user.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use desk_document_conversion::ConversionError;
use desk_document_conversion::sandbox::{self, SandboxRequest, SandboxResponse};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

/// Private first argument that turns the executable into a sandbox child.
pub const SUBCOMMAND: &str = "__lcxl-typst-sandbox";

/// Limits for one sandbox child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxLimits {
    /// Address space (Unix) or committed memory (Windows) of the child.
    pub memory_bytes: u64,
    /// CPU time across all threads of the child.
    pub cpu_seconds: u64,
    /// Wall-clock time from spawn to response.
    pub wall_clock: Duration,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            memory_bytes: 2 * 1024 * 1024 * 1024,
            cpu_seconds: 60,
            wall_clock: Duration::from_secs(60),
        }
    }
}

/// Why a sandboxed operation produced no response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxFailure {
    /// The child could not be started or contained. Nothing ran.
    Unavailable(String),
    /// The child exited without a response: a resource limit or a crash.
    ResourceLimit,
    /// The wall-clock deadline passed; the child was killed.
    Timeout,
    /// The child answered with a malformed frame.
    Protocol(String),
}

impl SandboxFailure {
    /// Every sandbox failure happens before any output exists, so it is always
    /// a definite "nothing was published".
    pub fn into_conversion_error(self) -> ConversionError {
        match self {
            Self::Unavailable(detail) => ConversionError::new(
                "document_sandbox_unavailable",
                format!("the document sandbox could not start: {detail}"),
            ),
            Self::ResourceLimit => ConversionError::new(
                "document_resource_limit_exceeded",
                "the document exceeded the conversion memory or CPU limit",
            ),
            Self::Timeout => ConversionError::new(
                "document_conversion_timeout",
                "the document did not finish within the conversion time limit",
            ),
            Self::Protocol(detail) => ConversionError::new(
                "document_sandbox_unavailable",
                format!("the document sandbox returned an invalid result: {detail}"),
            ),
        }
    }
}

/// Serves one request and returns the process exit code when the executable
/// was started as a sandbox child; returns `None` otherwise. Call it first in
/// `main`, before anything can write to stdout.
pub fn run_child_if_requested() -> Option<i32> {
    let mut args = std::env::args_os();
    let _ = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(SUBCOMMAND)) {
        return None;
    }
    if args.next().is_some() {
        return Some(2);
    }
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let result = sandbox::serve(&mut stdin.lock(), &mut stdout.lock());
    Some(if result.is_ok() { 0 } else { 1 })
}

/// Runs one request in a child of the current executable.
pub fn run(
    request: &SandboxRequest,
    source: &[u8],
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    let program =
        std::env::current_exe().map_err(|error| SandboxFailure::Unavailable(error.to_string()))?;
    run_with(
        &program,
        &[SUBCOMMAND],
        request,
        source,
        SandboxLimits::default(),
    )
}

/// Kills and reaps the child on every exit path, including unwinding.
struct ChildGuard {
    child: Child,
    _containment: platform::Containment,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs one request in `program args...`. Exposed for tests, which point it
/// at the built server binary or at stand-in programs.
pub fn run_with(
    program: &Path,
    args: &[&str],
    request: &SandboxRequest,
    source: &[u8],
    limits: SandboxLimits,
) -> Result<(SandboxResponse, Vec<u8>), SandboxFailure> {
    let started = Instant::now();
    let mut command = Command::new(PathBuf::from(program));
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Fewer allocator arenas and render threads keep the reserved address
        // space well below the memory limit for legitimate documents.
        .env("MALLOC_ARENA_MAX", "2")
        .env("RAYON_NUM_THREADS", "2");
    platform::before_spawn(&mut command, limits);
    let mut child = command
        .spawn()
        .map_err(|error| SandboxFailure::Unavailable(error.to_string()))?;
    let containment = match platform::after_spawn(&child, limits) {
        Ok(containment) => containment,
        Err(detail) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SandboxFailure::Unavailable(detail));
        }
    };
    let mut guard = ChildGuard {
        child,
        _containment: containment,
    };
    let mut stdin = guard
        .child
        .stdin
        .take()
        .ok_or_else(|| SandboxFailure::Unavailable("child stdin is unavailable".into()))?;
    let stdout = guard
        .child
        .stdout
        .take()
        .ok_or_else(|| SandboxFailure::Unavailable("child stdout is unavailable".into()))?;
    let (sender, receiver) = mpsc::channel();
    std::thread::Builder::new()
        .name("typst-sandbox-reader".into())
        .spawn(move || {
            let mut stdout = stdout;
            let _ = sender.send(sandbox::read_frame::<SandboxResponse>(&mut stdout));
        })
        .map_err(|error| SandboxFailure::Unavailable(error.to_string()))?;
    // The limits are already in force: the child blocks on this frame before
    // it evaluates anything. A child that died early closes the pipe.
    let written = sandbox::write_frame(&mut stdin, request, source).and_then(|()| stdin.flush());
    drop(stdin);
    let remaining = limits.wall_clock.saturating_sub(started.elapsed());
    match receiver.recv_timeout(remaining) {
        Ok(Ok(frame)) => {
            let _ = guard.child.wait();
            Ok(frame)
        }
        Ok(Err(error)) => {
            let status = guard.child.wait().ok();
            if written.is_err() || status.is_some_and(|status| !status.success()) {
                Err(SandboxFailure::ResourceLimit)
            } else {
                Err(SandboxFailure::Protocol(error.to_string()))
            }
        }
        Err(_) => {
            let _ = guard.child.kill();
            Err(SandboxFailure::Timeout)
        }
    }
}
