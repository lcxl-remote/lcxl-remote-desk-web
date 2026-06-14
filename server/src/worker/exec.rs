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
//! - `timeout_ms` hard cap, killed on expiry (`kill_on_drop`);
//! - stdout / stderr captured and truncated to `max_*_bytes`.

use std::process::Stdio;
use std::time::{Duration, Instant};

use desk_agent_protocol::exec::ExecPlan;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome, ExecOutput, OperationOutput};
use tokio::process::Command;

/// Execute a sealed plan and return the outcome. Execution failures (spawn
/// error, timeout) surface as [`AgentOutcome::Err`]; a process that ran (any
/// exit code) surfaces as [`AgentOutcome::Ok`] with the captured output.
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
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return AgentOutcome::Err(err(
                AgentErrorKind::Internal,
                format!("failed to start command: {e}"),
            ));
        }
    };

    let timeout = Duration::from_millis(plan.timeout_ms as u64);
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return AgentOutcome::Err(err(
                AgentErrorKind::Internal,
                format!("command failed: {e}"),
            ));
        }
        Err(_) => {
            // The `wait_with_output` future was dropped on timeout; `kill_on_drop`
            // terminates the child.
            return AgentOutcome::Err(err(
                AgentErrorKind::Timeout,
                format!("command timed out after {} ms", plan.timeout_ms),
            ));
        }
    };

    let duration_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    let (stdout, stdout_truncated) = truncate(output.stdout, plan.max_stdout_bytes as usize);
    let (stderr, stderr_truncated) = truncate(output.stderr, plan.max_stderr_bytes as usize);

    AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
        // `None` (terminated by a signal on Unix) maps to -1.
        exit_code: output.status.code().unwrap_or(-1),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        duration_ms,
        // Output scrubbing is applied at the daemon's outbound boundary.
        redactions: Vec::new(),
    }))
}

/// Truncate captured bytes to `max` and decode lossily. Cutting mid-codepoint is
/// safe — `from_utf8_lossy` replaces the partial tail rather than panicking.
fn truncate(mut bytes: Vec<u8>, max: usize) -> (String, bool) {
    let truncated = bytes.len() > max;
    if truncated {
        bytes.truncate(max);
    }
    (String::from_utf8_lossy(&bytes).into_owned(), truncated)
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
