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
//! - stdout / stderr read streaming and retained only up to `max_*_bytes` plus a
//!   small redaction margin (the excess is drained, so the cap bounds worker
//!   memory, not just the payload);
//! - output scrubbed by the redactor **before** the final cut to `max_*_bytes`
//!   (fail-closed), so raw secrets never cross the IPC boundary and a secret that
//!   straddles the cap is matched whole rather than leaking a truncated prefix.

use std::process::Stdio;
use std::time::{Duration, Instant};

use desk_agent_protocol::exec::ExecPlan;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome, ExecOutput, OperationOutput};
use log::warn;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::watch;

use crate::diagnose::redaction::{Redactor, RegexRedactor};
use crate::worker::exec_containment::Containment;
use desk_ipc_protocol::message::ExecSpawnReport;

/// Extra bytes read and retained past the payload cap so a secret that straddles
/// the cap boundary is still seen *in full* by the redactor before the final cut
/// — the fixed-length cloud-key patterns (`AKIA…`, `AIza…`) require the whole
/// token to match, so a prefix split at the cap would otherwise leak. Large
/// enough to also cover a typical PEM private-key block. Worker memory stays
/// bounded at `cap + REDACTION_MARGIN` per stream.
const REDACTION_MARGIN: usize = 8 * 1024;

/// Execute a sealed plan and return the outcome. Execution failures (spawn
/// error, timeout, fail-closed redaction) surface as [`AgentOutcome::Err`]; a
/// process that ran (any exit code) surfaces as [`AgentOutcome::Ok`] with the
/// scrubbed, capped output.
///
/// stdout/stderr are read **streaming** with a hard per-stream cap so a runaway
/// command cannot balloon worker memory (only `max_*_bytes + REDACTION_MARGIN`
/// are ever retained; the rest is drained so the process still completes). The
/// retained text is scrubbed by the redactor and only then cut to `max_*_bytes`,
/// so a secret straddling the cap is matched whole — raw secrets never cross the
/// IPC boundary. Redaction is fail-closed: if the redactor errors, no output is
/// returned.
pub async fn execute_plan(plan: &ExecPlan) -> AgentOutcome {
    execute_plan_reporting(plan, |_| {}).await
}

/// A running execution's stop switch, held by whoever may need to stop it.
///
/// Cancelling reclaims the whole process tree rather than asking the command to
/// stop, so it works on a command that ignores signals or has stopped reading its
/// input. That is why there is no "this cannot be cancelled" answer: containment
/// does not need the command's cooperation.
#[derive(Clone, Debug)]
pub struct ExecCancel(watch::Sender<bool>);

impl ExecCancel {
    pub fn new() -> Self {
        Self(watch::channel(false).0)
    }

    /// Ask the execution to stop. Idempotent, and safe to call after it finished.
    ///
    /// Uses `send_replace` rather than `send` deliberately: `send` refuses to
    /// update the value while nothing is listening, which would silently discard a
    /// cancel issued in the gap between accepting an execution and it beginning to
    /// watch — the exact race the retained value is here to win.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// A view for the execution itself to watch.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

impl Default for ExecCancel {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve once cancellation has been requested, or never if it cannot be.
///
/// A cancel that arrived before the execution began waiting is not missed: the
/// channel retains its value, so the first check already observes it. If every
/// sender is dropped without a cancel, this waits for ever rather than resolving
/// — resolving would read as "cancelled" and kill a healthy command.
async fn cancellation_requested(cancel: Option<watch::Receiver<bool>>) {
    let Some(mut rx) = cancel else {
        std::future::pending::<()>().await;
        unreachable!()
    };
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// As [`execute_plan`], but calls `on_spawn` the moment the outcome of the spawn
/// itself is known — before the command has finished.
///
/// The daemon reserved this execution before handing it over, and until the spawn
/// is reported it cannot tell "still starting" from "started and then lost". That
/// ambiguity is why a crash in the gap has to be recorded as indeterminate; this
/// callback closes the gap and hands over the containment identity, which only
/// exists on Unix once there is a pid.
pub async fn execute_plan_reporting(
    plan: &ExecPlan,
    on_spawn: impl FnOnce(ExecSpawnReport),
) -> AgentOutcome {
    execute_plan_cancellable(plan, on_spawn, None).await
}

/// As [`execute_plan_reporting`], but stoppable through `cancel`.
///
/// Cancelling reclaims the process tree exactly as a timeout does. The outcome is
/// [`AgentErrorKind::Cancelled`] rather than a success or a failure, because the
/// command did start: how much of its effect landed before it was stopped is not
/// something the host can know, and reporting either extreme would be a guess.
pub async fn execute_plan_cancellable(
    plan: &ExecPlan,
    on_spawn: impl FnOnce(ExecSpawnReport),
    cancel: Option<watch::Receiver<bool>>,
) -> AgentOutcome {
    // Establish the container before the spawn. Failing here refuses the command:
    // an execution that cannot be reclaimed is precisely what containment exists
    // to prevent, so running it anyway would defeat the purpose.
    let mut containment = match Containment::prepare(&plan.execution_generation) {
        Ok(c) => c,
        Err(e) => {
            warn!(
                "exec refused, no containment: template={} program={} error={e}",
                plan.template_id, plan.program,
            );
            on_spawn(ExecSpawnReport::Failed {
                reason: format!("the host cannot contain this command: {e}"),
            });
            return AgentOutcome::Err(err(
                AgentErrorKind::Internal,
                format!("the host cannot contain this command, so it was not run: {e}"),
            ));
        }
    };

    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &plan.cwd {
        cmd.current_dir(cwd);
    }
    containment.apply(&mut cmd);

    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            // The control end only sees a generic `internal` kind; log the real
            // OS error here so the cause (e.g. a missing program) is traceable.
            warn!(
                "exec spawn failed: template={} program={} error={e}",
                plan.template_id, plan.program,
            );
            on_spawn(ExecSpawnReport::Failed {
                reason: format!("failed to start command: {e}"),
            });
            return AgentOutcome::Err(err(
                AgentErrorKind::Internal,
                format!("failed to start command: {e}"),
            ));
        }
    };

    // The child exists but nothing yet ties its descendants to us. Until this
    // succeeds the tree is unreclaimable, so a failure kills the child outright
    // rather than letting it run loose.
    if let Err(e) = containment.adopt(&child) {
        let _ = child.start_kill();
        warn!(
            "exec containment failed after spawn: template={} program={} error={e}",
            plan.template_id, plan.program,
        );
        // The process did start, however briefly, so this is not reported as a
        // failed spawn — saying "never ran" about something that did would be worse
        // than saying nothing.
        on_spawn(ExecSpawnReport::Started {
            containment_identity: containment.identity().map(str::to_string),
        });
        return AgentOutcome::Err(err(
            AgentErrorKind::Internal,
            format!("the command was started but could not be contained: {e}"),
        ));
    }

    on_spawn(ExecSpawnReport::Started {
        containment_identity: containment.identity().map(str::to_string),
    });

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
        let ((out_bytes, out_over), (err_bytes, err_over)) = tokio::join!(read_out, read_err);
        let status = child.wait().await;
        (out_bytes, out_over, err_bytes, err_over, status)
    };

    // Race the command against both its deadline and a stop request. Losing to
    // either reclaims the tree; the difference is only what the caller is told.
    let raced = tokio::select! {
        result = tokio::time::timeout(timeout, run) => Some(result),
        _ = cancellation_requested(cancel) => None,
    };

    let Some(finished) = raced else {
        // The tree goes, not just the process we can see. `child` is still borrowed
        // by the abandoned future, but reclaiming the container already covers it —
        // the child is a member of its own group.
        containment.reclaim();
        warn!(
            "exec cancelled: template={} program={} generation={}",
            plan.template_id, plan.program, plan.execution_generation,
        );
        return AgentOutcome::Err(err(
            AgentErrorKind::Cancelled,
            "the command was cancelled and its process tree reclaimed".to_string(),
        ));
    };

    let (out_bytes, stdout_overflowed, err_bytes, stderr_overflowed, status) = match finished {
        Ok(result) => result,
        Err(_) => {
            // Reclaim the whole tree, not just the process we can see: killing
            // the direct child alone is what let a timed-out command's helpers
            // keep running past their deadline.
            containment.reclaim();
            let _ = child.start_kill();
            warn!(
                "exec timed out: template={} program={} timeout_ms={}",
                plan.template_id, plan.program, plan.timeout_ms,
            );
            return AgentOutcome::Err(err(
                AgentErrorKind::Timeout,
                format!("command timed out after {} ms", plan.timeout_ms),
            ));
        }
    };

    let status = match status {
        Ok(status) => status,
        Err(e) => {
            warn!(
                "exec wait failed: template={} program={} error={e}",
                plan.template_id, plan.program,
            );
            return AgentOutcome::Err(err(
                AgentErrorKind::Internal,
                format!("command failed: {e}"),
            ));
        }
    };

    let duration_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;

    // Scrub the retained text (cap + margin) *before* the final cut to the cap,
    // so a secret straddling the cap is matched whole rather than leaking a
    // truncated prefix. Fail-closed: a redactor error withholds all output.
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

    let (stdout_text, stdout_truncated) = finalize(stdout.text, out_cap, stdout_overflowed);
    let (stderr_text, stderr_truncated) = finalize(stderr.text, err_cap, stderr_overflowed);

    AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
        // `None` (terminated by a signal on Unix) maps to -1.
        exit_code: status.code().unwrap_or(-1),
        stdout: stdout_text,
        stderr: stderr_text,
        stdout_truncated,
        stderr_truncated,
        duration_ms,
        redactions,
    }))
}

/// Read a pipe streaming, retaining at most `cap + REDACTION_MARGIN` bytes (the
/// rest is drained so the process does not block on a full pipe). The margin is
/// kept so the redactor can match a secret that straddles the payload cap; the
/// caller redacts the retained text and then cuts it back to `cap`. Returns the
/// retained bytes and whether the process produced **more than `cap`** bytes
/// (i.e. the payload will be truncated). Reading a `None` pipe yields empty.
async fn read_capped<R: AsyncRead + Unpin>(reader: &mut Option<R>, cap: usize) -> (Vec<u8>, bool) {
    let Some(reader) = reader.as_mut() else {
        return (Vec::new(), false);
    };
    let read_limit = cap.saturating_add(REDACTION_MARGIN);
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut total: usize = 0;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                total = total.saturating_add(n);
                if buf.len() < read_limit {
                    let take = (read_limit - buf.len()).min(n);
                    buf.extend_from_slice(&chunk[..take]);
                }
                // Past the read limit: drain and discard so the child keeps
                // running while worker memory stays bounded at the read limit.
            }
            Err(_) => break,
        }
    }
    (buf, total > cap)
}

/// Cut already-redacted text back to the payload `cap` and report truncation.
///
/// The redactor has already run over the retained text (cap + margin), so every
/// bounded secret is gone. If the output still exceeds the cap we cut to a char
/// boundary and, because a cut can land mid-run, drop the trailing unterminated
/// run back to the last whitespace — this removes any partial token the cut
/// would otherwise expose (a single run longer than `cap + REDACTION_MARGIN`,
/// e.g. an oversized PEM block, is the residual edge).
fn finalize(text: String, cap: usize, overflowed: bool) -> (String, bool) {
    if text.len() <= cap && !overflowed {
        return (text, false);
    }
    let mut end = cap.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = text[..end].to_string();
    if end < text.len() {
        if let Some(pos) = out.rfind(char::is_whitespace) {
            out.truncate(pos);
        }
    }
    (out, true)
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
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::exec::ExecShellKind;

    /// A successful spawn is reported before the command finishes, carrying the
    /// containment identity the daemon needs to reclaim the tree if it later loses
    /// track of it.
    #[tokio::test]
    async fn a_successful_spawn_is_reported_with_its_containment_identity() {
        let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = reports.clone();
        let outcome = execute_plan_reporting(&plan("echo hi", 5_000, 4096), move |r| {
            sink.lock().unwrap().push(r)
        })
        .await;
        assert!(matches!(outcome, AgentOutcome::Ok(_)), "{outcome:?}");

        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 1, "exactly one spawn report per execution");
        match &reports[0] {
            ExecSpawnReport::Started {
                containment_identity,
            } => {
                #[cfg(unix)]
                assert!(
                    containment_identity
                        .as_deref()
                        .is_some_and(|id| id.starts_with("pgid:")),
                    "expected a process-group identity, got {containment_identity:?}"
                );
                #[cfg(not(unix))]
                let _ = containment_identity;
            }
            other => panic!("expected Started, got {other:?}"),
        }
    }

    /// A command that never starts is reported as a failed spawn, not as an unknown
    /// outcome — the caller can safely retry this one, and only this one.
    #[tokio::test]
    async fn a_failed_spawn_is_reported_as_such() {
        let reports = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = reports.clone();
        let mut p = plan("irrelevant", 5_000, 4096);
        p.program = "definitely-not-a-real-program-xyz".into();
        p.argv.clear();

        let outcome = execute_plan_reporting(&p, move |r| sink.lock().unwrap().push(r)).await;
        assert!(matches!(outcome, AgentOutcome::Err(_)), "{outcome:?}");

        let reports = reports.lock().unwrap();
        assert!(
            matches!(reports.as_slice(), [ExecSpawnReport::Failed { .. }]),
            "expected a single Failed report, got {reports:?}"
        );
    }

    /// The point of containment: a command that backgrounds a helper and then
    /// hangs must not leave that helper running once the command is reclaimed.
    /// Killing only the direct child — the behaviour before containment — left the
    /// descendant alive past the deadline it was supposed to be bounded by.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_timed_out_command_takes_its_descendants_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant-alive");
        let marker_path = marker.to_string_lossy().to_string();

        // A grandchild that outlives its parent: it keeps re-creating the marker
        // long past the command's own timeout, so a surviving process is visible
        // as a marker that reappears after the command was reclaimed.
        let outcome = execute_plan(&plan(
            &format!("(while true; do touch '{marker_path}'; sleep 0.05; done) & sleep 30"),
            300,
            4096,
        ))
        .await;
        assert!(
            matches!(&outcome, AgentOutcome::Err(e) if e.kind == AgentErrorKind::Timeout),
            "expected a timeout, got {outcome:?}"
        );

        // Give any survivor a generous chance to prove it is still running.
        let _ = std::fs::remove_file(&marker);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !marker.exists(),
            "a descendant outlived the reclaimed command"
        );
    }

    /// Cancelling a long-running command stops it well before its own deadline
    /// and reclaims its descendants, exactly as a timeout does.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_command_takes_its_descendants_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant-alive");
        let marker_path = marker.to_string_lossy().to_string();

        let cancel = ExecCancel::new();
        let watcher = cancel.subscribe();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
        });

        let started = Instant::now();
        // A 30-second command with a 30-second budget: only the cancel can end it
        // this quickly, so a fast return cannot be a timeout in disguise.
        let outcome = execute_plan_cancellable(
            &plan(
                &format!("(while true; do touch '{marker_path}'; sleep 0.05; done) & sleep 30"),
                30_000,
                4096,
            ),
            |_| {},
            Some(watcher),
        )
        .await;

        assert!(
            matches!(&outcome, AgentOutcome::Err(e) if e.kind == AgentErrorKind::Cancelled),
            "expected a cancellation, got {outcome:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the cancel did not take effect promptly"
        );

        let _ = std::fs::remove_file(&marker);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !marker.exists(),
            "a descendant outlived the cancelled command"
        );
    }

    /// A cancel that arrives before the command starts waiting is not lost — the
    /// switch retains its state, so the race between requesting a stop and
    /// beginning to watch for one has no losing side.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancel_requested_up_front_is_not_missed() {
        let cancel = ExecCancel::new();
        cancel.cancel();
        let outcome = execute_plan_cancellable(
            &plan("sleep 30", 30_000, 4096),
            |_| {},
            Some(cancel.subscribe()),
        )
        .await;
        assert!(
            matches!(&outcome, AgentOutcome::Err(e) if e.kind == AgentErrorKind::Cancelled),
            "expected a cancellation, got {outcome:?}"
        );
    }

    /// Dropping every stop switch is not a cancellation. A command whose canceller
    /// went away must run to its own conclusion rather than being killed by the
    /// disappearance of something that never asked for anything.
    #[cfg(unix)]
    #[tokio::test]
    async fn losing_the_stop_switch_does_not_stop_the_command() {
        let cancel = ExecCancel::new();
        let watcher = cancel.subscribe();
        drop(cancel);
        let outcome =
            execute_plan_cancellable(&plan("echo hi", 5_000, 4096), |_| {}, Some(watcher)).await;
        assert!(matches!(outcome, AgentOutcome::Ok(_)), "{outcome:?}");
    }

    /// A command that exits on its own is reclaimed too, so a helper it left
    /// behind does not survive the call.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_completed_command_does_not_leak_a_background_helper() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("helper-alive");
        let marker_path = marker.to_string_lossy().to_string();

        // The helper's own output is redirected away: a background process that
        // keeps the inherited stdout pipe open holds the executor's reader until
        // the timeout, so the command would not "complete" at all.
        let outcome = execute_plan(&plan(
            &format!(
                "(while true; do touch '{marker_path}'; sleep 0.05; done) >/dev/null 2>&1 & echo started"
            ),
            5_000,
            4096,
        ))
        .await;
        assert!(matches!(outcome, AgentOutcome::Ok(_)), "{outcome:?}");

        let _ = std::fs::remove_file(&marker);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!marker.exists(), "a background helper outlived the command");
    }

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
            // Windows containment uses this value as the job-object name. Keep
            // concurrently running test commands isolated from one another.
            execution_generation: format!("gen_t_{}", uuid::Uuid::new_v4()),
            program,
            argv,
            cwd: None,
            shell: ExecShellKind::Native,
            risk: RiskLevel::Low,
            execution_basis: desk_agent_protocol::exec::ExecExecutionBasis::Template,
            template_id: "test".into(),
            approval_id: desk_agent_protocol::exec::ApprovalId("appr_t".into()),
            fingerprint: "fp".into(),
            timeout_ms,
            max_stdout_bytes: max_out,
            max_stderr_bytes: max_out,
            containment: Default::default(),
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
    async fn redacts_secret_straddling_the_cap_boundary() {
        // The AWS key begins before the payload cap but extends past it. Cutting
        // to the cap before redaction would leave its (pattern-unmatchable)
        // prefix in the output; the redaction margin lets the redactor see the
        // whole token and scrub it before the final cut.
        let secret = "AKIAIOSFODNN7EXAMPLE"; // 20 chars, fixed-length pattern
        let snippet = format!("echo head {secret}");
        // "head " = 5 chars; cap = 12 falls in the middle of the key.
        let out = exec_output(execute_plan(&plan(&snippet, 10_000, 12)).await);
        assert!(
            !out.stdout.contains("AKIA"),
            "partial secret leaked across the cap: {:?}",
            out.stdout
        );
        assert!(
            out.redactions.iter().any(|k| k == "aws_access_key"),
            "straddling secret was not matched: {:?}",
            out.redactions
        );
        assert!(out.stdout_truncated);
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
        let snippet = "for /L %i in (1,1,2147483647) do @rem";
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
