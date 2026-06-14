//! Worker-side execution of a sealed [`ExecPlan`].
//!
//! The daemon has already classified the command, matched a whitelist template,
//! rendered the argv, and obtained explicit user approval. The worker's only job
//! is to run `program` + `argv` **verbatim** inside the user session and report
//! the result. It never re-parses a command string, never spawns a shell to
//! interpret one, never reads stdin, and never elevates.
//!
//! Guard rails (security model §9):
//! - no stdin (`Stdio::null`) — non-interactive;
//! - argv executed directly (no `cmd /c` / `bash -c` wrapping);
//! - `timeout_ms` hard cap, killed on expiry;
//! - stdout / stderr read streaming and retained only up to `max_*_bytes` (the
//!   excess is drained, so the cap bounds worker memory, not just the payload);
//! - output scrubbed by the redactor before leaving the worker (fail-closed),
//!   so raw secrets never cross the IPC boundary.

use std::process::Stdio;
use std::time::{Duration, Instant};

use desk_agent_protocol::exec::ExecPlan;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome, ExecOutput, OperationOutput};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::diagnose::redaction::{Redactor, RegexRedactor};

/// Execute a sealed plan and return the outcome. Execution failures (spawn
/// error, timeout, fail-closed redaction) surface as [`AgentOutcome::Err`]; a
/// process that ran (any exit code) surfaces as [`AgentOutcome::Ok`] with the
/// scrubbed, capped output.
///
/// stdout/stderr are read **streaming** with a hard per-stream cap so a runaway
/// command cannot balloon worker memory (only `max_*_bytes` are ever retained;
/// the rest is drained so the process still completes), and the captured text is
/// scrubbed by the redactor before it leaves the worker — raw secrets never
/// cross the IPC boundary. Redaction is fail-closed: if the redactor errors, no
/// output is returned.
pub async fn execute_plan(plan: &ExecPlan) -> AgentOutcome {
    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &plan.cwd {
        cmd.current_dir(cwd);
    }

    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return AgentOutcome::Err(err(
                AgentErrorKind::Internal,
                format!("failed to start command: {e}"),
            ));
        }
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_cap = plan.max_stdout_bytes as usize;
    let err_cap = plan.max_stderr_bytes as usize;

    let timeout = Duration::from_millis(plan.timeout_ms as u64);
    // Drive both pipe readers and the process wait together so a process that
    // fills one pipe while we read the other cannot deadlock. The whole thing is
    // bounded by the timeout; on expiry the child is killed.
    let run = async {
        let read_out = read_capped(&mut stdout_pipe, out_cap);
        let read_err = read_capped(&mut stderr_pipe, err_cap);
        let ((out_bytes, out_trunc), (err_bytes, err_trunc)) = tokio::join!(read_out, read_err);
        let status = child.wait().await;
        (out_bytes, out_trunc, err_bytes, err_trunc, status)
    };

    let (out_bytes, stdout_truncated, err_bytes, stderr_truncated, status) =
        match tokio::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => {
                let _ = child.start_kill();
                return AgentOutcome::Err(err(
                    AgentErrorKind::Timeout,
                    format!("command timed out after {} ms", plan.timeout_ms),
                ));
            }
        };

    let status = match status {
        Ok(status) => status,
        Err(e) => {
            return AgentOutcome::Err(err(
                AgentErrorKind::Internal,
                format!("command failed: {e}"),
            ));
        }
    };

    let duration_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;

    // Scrub before the output leaves the worker (fail-closed).
    let redactor = RegexRedactor::new();
    let stdout = match redactor.redact(&String::from_utf8_lossy(&out_bytes)) {
        Ok(r) => r,
        Err(_) => return AgentOutcome::Err(redaction_failed()),
    };
    let stderr = match redactor.redact(&String::from_utf8_lossy(&err_bytes)) {
        Ok(r) => r,
        Err(_) => return AgentOutcome::Err(redaction_failed()),
    };
    let mut redactions = stdout.kinds;
    redactions.extend(stderr.kinds);

    AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
        // `None` (terminated by a signal on Unix) maps to -1.
        exit_code: status.code().unwrap_or(-1),
        stdout: stdout.text,
        stderr: stderr.text,
        stdout_truncated,
        stderr_truncated,
        duration_ms,
        redactions,
    }))
}

/// Read a pipe streaming, retaining at most `cap` bytes (the rest is drained so
/// the process does not block on a full pipe). Returns the retained bytes and
/// whether anything beyond the cap was seen. Reading a `None` pipe yields empty.
async fn read_capped<R: AsyncRead + Unpin>(reader: &mut Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let Some(reader) = reader.as_mut() else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < cap {
                    let take = (cap - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    // Past the cap: drain and discard so the child keeps running.
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

fn redaction_failed() -> AgentError {
    err(
        AgentErrorKind::RedactionFailed,
        "command output withheld: redaction failed".to_string(),
    )
}

fn err(kind: AgentErrorKind, message: String) -> AgentError {
    AgentError {
        kind,
        message,
        retryable: false,
        safe_for_model: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::exec::ExecShellKind;

    /// Build a plan that runs a shell snippet through the OS default shell.
    /// These tests exercise the executor mechanics directly (not the classifier),
    /// so using the shell here is fine.
    fn plan(snippet: &str, timeout_ms: u32, max_out: u32) -> ExecPlan {
        #[cfg(windows)]
        let (program, argv) = (
            "cmd".to_string(),
            vec!["/C".to_string(), snippet.to_string()],
        );
        #[cfg(not(windows))]
        let (program, argv) = (
            "sh".to_string(),
            vec!["-c".to_string(), snippet.to_string()],
        );
        ExecPlan {
            exec_request_id: desk_agent_protocol::exec::ExecRequestId("exec_t".into()),
            program,
            argv,
            cwd: None,
            shell: ExecShellKind::Native,
            risk: RiskLevel::Low,
            template_id: "test".into(),
            approval_id: desk_agent_protocol::exec::ApprovalId("appr_t".into()),
            fingerprint: "fp".into(),
            timeout_ms,
            max_stdout_bytes: max_out,
            max_stderr_bytes: max_out,
        }
    }

    fn exec_output(outcome: AgentOutcome) -> ExecOutput {
        match outcome {
            AgentOutcome::Ok(OperationOutput::Exec(o)) => o,
            other => panic!("expected exec output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn runs_and_captures_stdout() {
        let out = exec_output(execute_plan(&plan("echo hello", 10_000, 65_536)).await);
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("hello"), "stdout was {:?}", out.stdout);
        assert!(!out.stdout_truncated);
    }

    #[tokio::test]
    async fn reports_nonzero_exit_code() {
        let out = exec_output(execute_plan(&plan("exit 3", 10_000, 65_536)).await);
        assert_eq!(out.exit_code, 3);
    }

    #[tokio::test]
    async fn truncates_oversized_stdout() {
        // Emit far more than the 8-byte cap.
        let out = exec_output(execute_plan(&plan("echo abcdefghijklmnop", 10_000, 8)).await);
        assert!(out.stdout_truncated);
        assert!(out.stdout.len() <= 8);
    }

    #[tokio::test]
    async fn redacts_secrets_in_output() {
        // An AWS access key id in stdout must be scrubbed before it leaves the
        // worker, and counted in `redactions`.
        let out =
            exec_output(execute_plan(&plan("echo AKIAIOSFODNN7EXAMPLE", 10_000, 65_536)).await);
        assert!(
            !out.stdout.contains("AKIAIOSFODNN7EXAMPLE"),
            "raw secret leaked: {:?}",
            out.stdout
        );
        assert!(
            out.stdout.contains("<redacted"),
            "no redaction marker: {:?}",
            out.stdout
        );
        assert!(!out.redactions.is_empty(), "redaction not counted");
    }

    #[tokio::test]
    async fn caps_retained_output_to_limit() {
        // Far more than the cap is emitted; only `max_stdout_bytes` are retained
        // (the rest is drained), bounding worker memory regardless of volume.
        let big = "x".repeat(5000);
        let out = exec_output(execute_plan(&plan(&format!("echo {big}"), 10_000, 256)).await);
        assert!(out.stdout_truncated);
        assert!(
            out.stdout.len() <= 256,
            "retained {} bytes",
            out.stdout.len()
        );
    }

    #[tokio::test]
    async fn times_out_and_kills_long_command() {
        #[cfg(windows)]
        let snippet = "ping -n 6 127.0.0.1";
        #[cfg(not(windows))]
        let snippet = "sleep 5";
        let outcome = execute_plan(&plan(snippet, 300, 65_536)).await;
        match outcome {
            AgentOutcome::Err(e) => assert_eq!(e.kind, AgentErrorKind::Timeout),
            AgentOutcome::Ok(o) => panic!("expected timeout, ran to completion: {o:?}"),
        }
    }

    #[tokio::test]
    async fn missing_program_is_an_error() {
        let mut p = plan("echo hi", 10_000, 65_536);
        p.program = "lcxl-definitely-not-a-real-binary".into();
        p.argv = Vec::new();
        match execute_plan(&p).await {
            AgentOutcome::Err(e) => assert_eq!(e.kind, AgentErrorKind::Internal),
            AgentOutcome::Ok(o) => panic!("expected spawn error, got {o:?}"),
        }
    }
}
