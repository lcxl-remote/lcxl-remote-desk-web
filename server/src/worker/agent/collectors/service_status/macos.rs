//! macOS backend for `service.status`: launchd jobs via `launchctl list`.
//!
//! `launchctl list` (no argument) prints one tab-separated row per loaded job —
//! `PID<TAB>Status<TAB>Label` — under a header row. A numeric PID means the job
//! is currently running; `-` means it is loaded but not running. launchd exposes
//! no per-job start type comparable to a Windows service start type, so
//! `start_type` and `display_name` are always `None`.

use std::process::Command;

use desk_agent_protocol::{AgentError, AgentErrorKind, ServiceEntry};

/// Enumerate every loaded launchd job.
pub(super) fn enumerate_all() -> Result<Vec<ServiceEntry>, AgentError> {
    let output = Command::new("launchctl")
        .arg("list")
        .output()
        .map_err(|e| internal(format!("failed to run launchctl: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(internal(format!(
            "launchctl list failed: {}",
            stderr.trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_launchctl_list(&text))
}

/// Query a single launchd job by exact label. A label that matches no loaded
/// job is a caller input error, not a backend failure.
pub(super) fn query_one(name: &str) -> Result<ServiceEntry, AgentError> {
    let entry = enumerate_all()?.into_iter().find(|e| e.name == name);
    entry.ok_or_else(|| AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: format!("service {name:?} not found"),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    })
}

fn internal(message: String) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message,
        retryable: true,
        safe_for_model: true,
        error_code: None,
    }
}

/// Parse `launchctl list` output into one [`ServiceEntry`] per job row. The
/// header row (`PID\tStatus\tLabel`) and any malformed line are skipped.
/// Platform-agnostic so the tab parsing is unit tested on any host.
#[cfg(any(target_os = "macos", test))]
fn parse_launchctl_list(text: &str) -> Vec<ServiceEntry> {
    text.lines().filter_map(parse_launchctl_line).collect()
}

/// Parse one `PID\tStatus\tLabel` row. Returns `None` for the header row and
/// any line without all three tab-separated columns.
#[cfg(any(target_os = "macos", test))]
fn parse_launchctl_line(line: &str) -> Option<ServiceEntry> {
    let mut cols = line.split('\t');
    let pid = cols.next()?.trim();
    let _status = cols.next()?;
    let label = cols.next()?.trim();
    if label.is_empty() {
        return None;
    }
    // The header row has a non-numeric, non-"-" PID column ("PID").
    let state = match pid {
        "-" => "stopped",
        p if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => "running",
        _ => return None,
    };
    Some(ServiceEntry {
        name: label.to_string(),
        display_name: None,
        state: state.to_string(),
        start_type: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_running_and_stopped_jobs() {
        let text = "PID\tStatus\tLabel\n\
                    501\t0\tcom.apple.running.job\n\
                    -\t0\tcom.apple.loaded.job\n";
        let entries = parse_launchctl_list(text);
        assert_eq!(entries.len(), 2);

        let running = &entries[0];
        assert_eq!(running.name, "com.apple.running.job");
        assert_eq!(running.state, "running");
        assert!(running.display_name.is_none());
        assert!(running.start_type.is_none());

        let stopped = &entries[1];
        assert_eq!(stopped.name, "com.apple.loaded.job");
        assert_eq!(stopped.state, "stopped");
    }

    #[test]
    fn skips_header_and_malformed_lines() {
        let text = "PID\tStatus\tLabel\n\
                    not-a-row-without-tabs\n\
                    \n\
                    42\t0\tcom.apple.ok\n";
        let entries = parse_launchctl_list(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "com.apple.ok");
    }
}
