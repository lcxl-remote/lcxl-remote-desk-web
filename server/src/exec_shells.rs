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
        .filter_map(|candidate| {
            let outcome = probe(candidate.program, candidate.args);
            if candidate_is_available(candidate, outcome) {
                if outcome == ProbeOutcome::TimedOut {
                    log::warn!(
                        "[agent-exec] shell probe for {} exceeded {} ms after the process \
                         started; treating the interpreter as available",
                        candidate.name,
                        PROBE_TIMEOUT.as_millis()
                    );
                }
                Some(candidate.name.to_string())
            } else {
                log::warn!(
                    "[agent-exec] shell probe for {} did not establish availability: {:?}",
                    candidate.name,
                    outcome
                );
                None
            }
        })
        .collect()
}

fn candidate_is_available(candidate: &Candidate, outcome: ProbeOutcome) -> bool {
    outcome == ProbeOutcome::Success
        || (outcome == ProbeOutcome::TimedOut && candidate.spawn_proves_availability)
}

struct Candidate {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    /// PowerShell startup can exceed the smoke-test deadline on the overloaded
    /// machines this feature is specifically meant to diagnose. Once
    /// `CreateProcess` succeeds, the interpreter itself is present and runnable;
    /// timing out only means its trivial command has not completed yet.
    ///
    /// Bash remains strict on Windows because the WSL launcher can start even
    /// when no usable distribution exists.
    spawn_proves_availability: bool,
}

#[cfg(target_os = "windows")]
fn candidates() -> &'static [Candidate] {
    &[
        Candidate {
            name: "powershell",
            program: "powershell.exe",
            args: &["-NoProfile", "-NonInteractive", "-Command", "exit 0"],
            spawn_proves_availability: true,
        },
        Candidate {
            name: "pwsh",
            program: "pwsh.exe",
            args: &["-NoProfile", "-NonInteractive", "-Command", "exit 0"],
            spawn_proves_availability: true,
        },
        Candidate {
            name: "bash",
            program: "bash.exe",
            args: &["-lc", "exit 0"],
            spawn_proves_availability: false,
        },
        Candidate {
            name: "sh",
            program: "sh.exe",
            args: &["-lc", "exit 0"],
            spawn_proves_availability: false,
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
            spawn_proves_availability: false,
        },
        Candidate {
            name: "sh",
            program: "sh",
            args: &["-lc", "exit 0"],
            spawn_proves_availability: false,
        },
        Candidate {
            name: "pwsh",
            program: "pwsh",
            args: &["-NoProfile", "-NonInteractive", "-Command", "exit 0"],
            spawn_proves_availability: true,
        },
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    Success,
    Failed,
    TimedOut,
}

fn probe(program: &str, args: &[&str]) -> ProbeOutcome {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return ProbeOutcome::Failed;
    };

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    ProbeOutcome::Success
                } else {
                    ProbeOutcome::Failed
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeOutcome::TimedOut;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return ProbeOutcome::Failed;
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
        assert_eq!(
            probe(
                "lcxl-shell-probe-program-that-does-not-exist",
                &["exit", "0"]
            ),
            ProbeOutcome::Failed
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn only_powershell_candidates_remain_available_after_probe_timeout() {
        let candidates = candidates();
        let accepts_timeout = |name| {
            candidate_is_available(
                candidates
                    .iter()
                    .find(|candidate| candidate.name == name)
                    .unwrap(),
                ProbeOutcome::TimedOut,
            )
        };
        assert!(accepts_timeout("powershell"));
        assert!(accepts_timeout("pwsh"));
        assert!(!accepts_timeout("bash"));
        assert!(!accepts_timeout("sh"));
    }
}
