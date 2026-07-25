//! Runtime discovery for shells supported by AI free-form execution.
//!
//! Presence on `PATH` is insufficient on Windows: `bash.exe` may be the WSL
//! launcher even when no distribution is installed. Each candidate therefore
//! runs a bounded, side-effect-free `exit 0` probe. Results are cached for the
//! process lifetime because the worker inherits a fixed environment and package
//! installation normally requires a server restart before it can affect it.

use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

static AVAILABLE: OnceLock<Vec<String>> = OnceLock::new();

/// Return canonical shell names verified as runnable by the AI executor.
pub fn available_exec_shells() -> Vec<String> {
    AVAILABLE.get_or_init(probe_available_shells).clone()
}

fn probe_available_shells() -> Vec<String> {
    candidates()
        .iter()
        .filter(|candidate| probe(candidate.program, candidate.args))
        .map(|candidate| candidate.name.to_string())
        .collect()
}

struct Candidate {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

#[cfg(target_os = "windows")]
fn candidates() -> &'static [Candidate] {
    &[
        Candidate {
            name: "powershell",
            program: "powershell.exe",
            args: &["-NoProfile", "-NonInteractive", "-Command", "exit 0"],
        },
        Candidate {
            name: "pwsh",
            program: "pwsh.exe",
            args: &["-NoProfile", "-NonInteractive", "-Command", "exit 0"],
        },
        Candidate {
            name: "bash",
            program: "bash.exe",
            args: &["-lc", "exit 0"],
        },
        Candidate {
            name: "sh",
            program: "sh.exe",
            args: &["-lc", "exit 0"],
        },
    ]
}

#[cfg(not(target_os = "windows"))]
fn candidates() -> &'static [Candidate] {
    &[
        Candidate {
            name: "bash",
            program: "bash",
            args: &["-lc", "exit 0"],
        },
        Candidate {
            name: "sh",
            program: "sh",
            args: &["-lc", "exit 0"],
        },
        Candidate {
            name: "pwsh",
            program: "pwsh",
            args: &["-NoProfile", "-NonInteractive", "-Command", "exit 0"],
        },
    ]
}

fn probe(program: &str, args: &[&str]) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reported_names_are_canonical_and_unique() {
        let shells = available_exec_shells();
        for shell in &shells {
            assert!(matches!(
                shell.as_str(),
                "powershell" | "pwsh" | "bash" | "sh"
            ));
        }
        let mut deduped = shells.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), shells.len());
    }

    #[test]
    fn missing_program_is_not_available() {
        assert!(!probe(
            "lcxl-shell-probe-program-that-does-not-exist",
            &["exit", "0"]
        ));
    }
}
