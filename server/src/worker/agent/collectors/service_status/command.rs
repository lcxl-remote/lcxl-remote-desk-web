//! Bounded execution of fixed native service-manager queries on Unix.
use super::*;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

pub(super) fn run(
    program: &str,
    args: &[&str],
    deadline: Instant,
) -> Result<String, NativeDiagnostic> {
    let fail = |e: &std::io::Error| {
        NativeDiagnostic::from_io(DiagnosticStage::ServiceEnumeration, program, e)
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| fail(&e))?;
    // The guard always reclaims descendants, including after output/timeout errors.
    struct Guard(std::process::Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut child = Guard(child);
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(fail(&std::io::Error::last_os_error()));
        }
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_done = false;
    let mut err_done = false;
    let mut status = None;
    loop {
        if Instant::now() >= deadline {
            return Err(diagnostic(
                DiagnosticStage::Query,
                program,
                "Query incomplete: native query deadline exceeded",
            ));
        }
        out_done |= drain(&mut stdout, &mut out).map_err(|e| fail(&e))?;
        err_done |= drain(&mut stderr, &mut err).map_err(|e| fail(&e))?;
        if status.is_none() {
            status = child.0.try_wait().map_err(|e| fail(&e))?;
        }
        if out_done && err_done && status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let status = status.expect("observed exit");
    if !status.success() {
        return Err(NativeDiagnostic::new(
            DiagnosticStage::ServiceEnumeration,
            program,
            "process_exit",
            status.code().map(i64::from),
            &String::from_utf8_lossy(&err),
        ));
    }
    String::from_utf8(out).map_err(|_| {
        diagnostic(
            DiagnosticStage::OutputRead,
            program,
            "Invalid UTF-8 native output",
        )
    })
}

fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> std::io::Result<bool> {
    let mut buffer = [0u8; 8192];
    // Bound each pass so a continuously writing child cannot starve the deadline.
    for _ in 0..32 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if bytes.len() + n > 8 * 1024 * 1024 {
                    return Err(std::io::Error::other(
                        "Query incomplete: native output size budget exceeded",
                    ));
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(false)
}
