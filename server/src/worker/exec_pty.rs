//! Worker-side one-shot PTY execution for an approved [`ExecPlan`].
//!
//! This is intentionally separate from both non-interactive exec and the
//! long-lived user terminal feature. It accepts exact argv only, retains no
//! input bytes, and owns one bounded live-output relay for the lifetime of one
//! approved execution.

use std::sync::Arc;

use desk_agent_protocol::exec::ExecPlan;
use desk_agent_protocol::{AgentError, AgentErrorKind, AgentOutcome};
use desk_ipc_protocol::dual_transport::EventSender;
use desk_ipc_protocol::message::{ExecSpawnReport, WorkerToService};
use tokio::sync::watch;

use crate::model::settings::AiExecutionPolicy;
use crate::worker::exec_registry::ExecRegistry;

/// A small queue is deliberate: terminal bytes are ordered and may not be
/// dropped. If the event transport cannot keep up, the reader cancels the
/// command instead of buffering an unbounded transcript in the worker.
const LIVE_OUTPUT_QUEUE_CAP: usize = 8;
const LIVE_OUTPUT_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecPtyCapabilities {
    pub exec_pty: bool,
    pub exec_pty_elevation: bool,
}

/// Capabilities compiled into this host. Interactive elevation remains false
/// until the Linux ServiceDaemon owns the root containment and restart path.
pub const fn runtime_support() -> ExecPtyCapabilities {
    ExecPtyCapabilities {
        exec_pty: cfg!(target_os = "linux"),
        exec_pty_elevation: false,
    }
}

/// Effective capabilities after intersecting runtime support with the
/// machine-wide local policy. Elevation can never be advertised without the
/// ordinary PTY transport it depends on.
pub fn effective_capabilities(policy: &AiExecutionPolicy) -> ExecPtyCapabilities {
    let support = runtime_support();
    let exec_pty = support.exec_pty && policy.exec_pty_enabled;
    ExecPtyCapabilities {
        exec_pty,
        exec_pty_elevation: exec_pty
            && support.exec_pty_elevation
            && policy.interactive_elevation_enabled,
    }
}

pub async fn execute_pty_plan_cancellable(
    plan: &ExecPlan,
    exec_pty_enabled: bool,
    exec_pty_elevation_enabled: bool,
    stream_id: String,
    session_target_id: String,
    registration_generation: u64,
    worker_incarnation: u64,
    registry: ExecRegistry,
    cancel: watch::Receiver<bool>,
    event_sender: Arc<dyn EventSender<WorkerToService>>,
    on_spawn: impl FnOnce(ExecSpawnReport) + Send + 'static,
) -> AgentOutcome {
    if !exec_pty_enabled {
        on_spawn(ExecSpawnReport::Failed {
            reason: "interactive execution is not enabled on this host".to_string(),
        });
        return AgentOutcome::Err(agent_error(
            AgentErrorKind::UnsupportedCapability,
            "interactive execution is not enabled on this host".to_string(),
        ));
    }
    if plan.requires_root_pty_containment() && !exec_pty_elevation_enabled {
        on_spawn(ExecSpawnReport::Failed {
            reason: "interactive elevation is not enabled on this host".to_string(),
        });
        return AgentOutcome::Err(agent_error(
            AgentErrorKind::UnsupportedCapability,
            "interactive elevation is not enabled on this host".to_string(),
        ));
    }
    let (live_tx, mut live_rx) = tokio::sync::mpsc::channel(LIVE_OUTPUT_QUEUE_CAP);
    let relay_registry = registry.clone();
    let relay_generation = plan.execution_generation.clone();
    let relay = tokio::spawn(async move {
        while let Some(message) = live_rx.recv().await {
            match tokio::time::timeout(LIVE_OUTPUT_SEND_TIMEOUT, event_sender.send(message)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    log::warn!(
                        "PTY event transport closed; cancelling generation={} error={error}",
                        relay_generation
                    );
                    relay_registry.cancel(&relay_generation);
                    break;
                }
                Err(_) => {
                    log::warn!(
                        "PTY event transport was a slow consumer; cancelling generation={}",
                        relay_generation
                    );
                    relay_registry.cancel(&relay_generation);
                    break;
                }
            }
        }
    });

    let owned_plan = plan.clone();
    let blocking_registry = registry.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        platform::run(
            owned_plan,
            stream_id,
            session_target_id,
            registration_generation,
            worker_incarnation,
            blocking_registry,
            cancel,
            live_tx,
            on_spawn,
        )
    })
    .await;

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => AgentOutcome::Err(agent_error(
            AgentErrorKind::Internal,
            format!("PTY executor task failed: {error}"),
        )),
    };
    // `run` owns and drops the final sender. Draining here preserves frame order:
    // Opened -> Output* -> Closed before the execution result is emitted by the
    // caller on the ordinary lifecycle lane.
    let _ = relay.await;
    outcome
}

fn agent_error(kind: AgentErrorKind, message: String) -> AgentError {
    AgentError {
        kind,
        message,
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod gate_tests {
    use super::{effective_capabilities, runtime_support};
    use crate::model::settings::AiExecutionPolicy;

    #[test]
    fn local_policy_only_narrows_runtime_support() {
        let support = runtime_support();
        let mut policy = AiExecutionPolicy::default();

        assert_eq!(effective_capabilities(&policy).exec_pty, support.exec_pty);
        assert!(!effective_capabilities(&policy).exec_pty_elevation);

        policy.exec_pty_enabled = false;
        policy.interactive_elevation_enabled = true;
        assert!(!effective_capabilities(&policy).exec_pty);
        assert!(!effective_capabilities(&policy).exec_pty_elevation);
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::io::Read;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::{Duration, Instant};

    use desk_agent_protocol::exec::{ExecIoMode, ExecPlan};
    use desk_agent_protocol::exec_pty::{
        PtyCloseReason, PtyOutputFrame, PtyStreamClosed, PtyStreamOpened,
    };
    use desk_agent_protocol::{AgentErrorKind, AgentOutcome, ExecOutput, ExecOutputStreams};
    use desk_agent_protocol::{OperationOutput, exec_pty::MAX_PTY_DATA_FRAME_BYTES};
    use desk_ipc_protocol::message::{ExecSpawnReport, WorkerToService};
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use tokio::sync::{mpsc, watch};

    use crate::agent_adapter::redaction::{Redactor, RegexRedactor};
    use crate::worker::exec_pty::agent_error;
    use crate::worker::exec_registry::ExecRegistry;

    const REDACTION_MARGIN: usize = 8 * 1024;
    const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(15);
    const STREAM_OK: u8 = 0;
    const STREAM_SLOW: u8 = 1;
    const STREAM_CLOSED: u8 = 2;

    #[allow(clippy::too_many_arguments)]
    pub(super) fn run(
        plan: ExecPlan,
        stream_id: String,
        session_target_id: String,
        registration_generation: u64,
        worker_incarnation: u64,
        registry: ExecRegistry,
        cancel: watch::Receiver<bool>,
        live_tx: mpsc::Sender<WorkerToService>,
        on_spawn: impl FnOnce(ExecSpawnReport),
    ) -> AgentOutcome {
        let (rows, cols) = match plan.io_mode {
            ExecIoMode::Pty {
                initial_rows,
                initial_cols,
            } => (initial_rows, initial_cols),
            ExecIoMode::NonInteractive => {
                on_spawn(ExecSpawnReport::Failed {
                    reason: "non-interactive plans require the pipe executor".to_string(),
                });
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::InvalidInput,
                    "non-interactive plans require the pipe executor".to_string(),
                ));
            }
        };
        if let Err(error) = plan.io_mode.validate() {
            on_spawn(ExecSpawnReport::Failed {
                reason: error.to_string(),
            });
            return AgentOutcome::Err(agent_error(AgentErrorKind::InvalidInput, error.to_string()));
        }
        // Interactive elevation has a separate root-daemon/systemd-scope
        // executor. Letting a session worker run the wrapper would provide a
        // prompt without the containment/restart guarantees the approval means.
        if plan.requires_root_pty_containment() {
            on_spawn(ExecSpawnReport::Failed {
                reason: "interactive elevation requires Linux ServiceDaemon containment"
                    .to_string(),
            });
            return AgentOutcome::Err(agent_error(
                AgentErrorKind::InvalidInput,
                "interactive elevation requires Linux ServiceDaemon containment".to_string(),
            ));
        }

        let opened = PtyStreamOpened {
            task_id: plan.exec_request_id.0.clone(),
            execution_generation: plan.execution_generation.clone(),
            stream_id: stream_id.clone(),
            session_target_id,
            registration_generation,
            worker_incarnation,
        };
        if let Err(error) = opened.validate() {
            on_spawn(ExecSpawnReport::Failed {
                reason: error.to_string(),
            });
            return AgentOutcome::Err(agent_error(AgentErrorKind::InvalidInput, error.to_string()));
        }

        let pair = match native_pty_system().openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(pair) => pair,
            Err(error) => {
                on_spawn(ExecSpawnReport::Failed {
                    reason: format!("failed to allocate PTY: {error}"),
                });
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    format!("failed to allocate PTY: {error}"),
                ));
            }
        };
        let mut reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => {
                on_spawn(ExecSpawnReport::Failed {
                    reason: format!("failed to open PTY output: {error}"),
                });
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    format!("failed to open PTY output: {error}"),
                ));
            }
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => {
                on_spawn(ExecSpawnReport::Failed {
                    reason: format!("failed to open PTY input: {error}"),
                });
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    format!("failed to open PTY input: {error}"),
                ));
            }
        };
        let mut command = CommandBuilder::new(&plan.program);
        command.args(&plan.argv);
        command.env("TERM", "xterm-256color");
        if let Some(cwd) = &plan.cwd {
            command.cwd(cwd);
        }
        let started = Instant::now();
        let mut child = match pair.slave.spawn_command(command) {
            Ok(child) => child,
            Err(error) => {
                on_spawn(ExecSpawnReport::Failed {
                    reason: format!("failed to start PTY command: {error}"),
                });
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    format!("failed to start PTY command: {error}"),
                ));
            }
        };
        drop(pair.slave);
        let pgid = pair.master.process_group_leader();

        let containment_identity = pgid.map(|value| format!("pgid:{value}"));
        on_spawn(ExecSpawnReport::Started {
            containment_identity,
        });
        if let Err(error) = registry.attach_pty(
            &plan.execution_generation,
            stream_id.clone(),
            opened.session_target_id.clone(),
            registration_generation,
            worker_incarnation,
            writer,
            pair.master,
        ) {
            reclaim_group(pgid);
            let _ = child.kill();
            let _ = child.wait();
            return AgentOutcome::Err(agent_error(AgentErrorKind::Cancelled, error));
        }

        if live_tx
            .try_send(WorkerToService::ExecPtyOpened(opened.clone()))
            .is_err()
        {
            reclaim_group(pgid);
            let _ = child.kill();
            let _ = child.wait();
            registry.detach_pty(&plan.execution_generation);
            return AgentOutcome::Err(agent_error(
                AgentErrorKind::Cancelled,
                "PTY live-output carrier is unavailable".to_string(),
            ));
        }

        let stream_state = std::sync::Arc::new(AtomicU8::new(STREAM_OK));
        let reader_state = std::sync::Arc::clone(&stream_state);
        let reader_tx = live_tx.clone();
        let reader_stream_id = stream_id.clone();
        let reader_generation = plan.execution_generation.clone();
        let reader_session_target_id = opened.session_target_id.clone();
        let result_cap =
            (plan.max_stdout_bytes as usize).saturating_add(plan.max_stderr_bytes as usize);
        let reader_task = std::thread::spawn(move || {
            read_output(
                &mut reader,
                reader_tx,
                reader_stream_id,
                reader_generation,
                reader_session_target_id,
                registration_generation,
                worker_incarnation,
                result_cap,
                reader_state,
            )
        });

        let timeout = Duration::from_millis(plan.timeout_ms as u64);
        let mut close_reason = PtyCloseReason::Exited;
        let mut status = None;
        loop {
            if *cancel.borrow() {
                close_reason = registry
                    .pty_stop_reason(&plan.execution_generation)
                    .unwrap_or(PtyCloseReason::Cancelled);
                break;
            }
            match stream_state.load(Ordering::Acquire) {
                STREAM_SLOW => {
                    close_reason = PtyCloseReason::SlowConsumer;
                    break;
                }
                STREAM_CLOSED => {
                    close_reason = PtyCloseReason::CarrierDisconnected;
                    break;
                }
                _ => {}
            }
            if started.elapsed() >= timeout {
                close_reason = PtyCloseReason::TimedOut;
                break;
            }
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                Ok(None) => std::thread::sleep(CHILD_POLL_INTERVAL),
                Err(error) => {
                    log::warn!(
                        "PTY child wait failed generation={} error={error}",
                        plan.execution_generation
                    );
                    close_reason = PtyCloseReason::OutcomeUnknown;
                    break;
                }
            }
        }

        // Reclaim the process group on every path, including a successful direct
        // child exit, so a background helper cannot outlive the approved command.
        reclaim_group(pgid);
        if status.is_none() {
            let _ = child.kill();
            status = child.wait().ok();
        }
        let input_stats = registry.detach_pty(&plan.execution_generation);
        let reader_result = reader_task.join().unwrap_or_default();
        let exit_status = status.as_ref().map(|value| {
            if value.signal().is_some() {
                -1
            } else {
                value.exit_code().min(i32::MAX as u32) as i32
            }
        });
        let closed = PtyStreamClosed {
            stream_id,
            execution_generation: plan.execution_generation.clone(),
            session_target_id: opened.session_target_id,
            registration_generation,
            worker_incarnation,
            exit_status,
            reason: close_reason,
            input_frames: input_stats.frames,
            input_bytes: input_stats.bytes,
            output_bytes: reader_result.total_bytes,
        };
        debug_assert!(closed.validate().is_ok());
        let _ = live_tx.try_send(WorkerToService::ExecPtyClosed(closed));

        match close_reason {
            PtyCloseReason::Exited => finish_result(
                exit_status.unwrap_or(-1),
                started.elapsed(),
                reader_result,
                result_cap,
            ),
            PtyCloseReason::TimedOut => AgentOutcome::Err(agent_error(
                AgentErrorKind::Timeout,
                format!("PTY command timed out after {} ms", plan.timeout_ms),
            )),
            PtyCloseReason::Cancelled
            | PtyCloseReason::CarrierDisconnected
            | PtyCloseReason::SlowConsumer
            | PtyCloseReason::SessionStale
            | PtyCloseReason::SequenceViolation => AgentOutcome::Err(agent_error(
                AgentErrorKind::Cancelled,
                format!("PTY command stopped: {close_reason:?}"),
            )),
            PtyCloseReason::OutcomeUnknown | PtyCloseReason::InternalError => {
                AgentOutcome::Err(agent_error(
                    AgentErrorKind::Internal,
                    "PTY command outcome is unknown".to_string(),
                ))
            }
        }
    }

    #[derive(Default)]
    struct ReaderResult {
        retained: Vec<u8>,
        total_bytes: u64,
        overflowed: bool,
    }

    fn read_output(
        reader: &mut Box<dyn Read + Send>,
        live_tx: mpsc::Sender<WorkerToService>,
        stream_id: String,
        generation: String,
        session_target_id: String,
        registration_generation: u64,
        worker_incarnation: u64,
        result_cap: usize,
        stream_state: std::sync::Arc<AtomicU8>,
    ) -> ReaderResult {
        let retain_limit = result_cap.saturating_add(REDACTION_MARGIN);
        let mut result = ReaderResult::default();
        let mut sequence = 0u64;
        let mut chunk = vec![0u8; MAX_PTY_DATA_FRAME_BYTES.min(8192)];
        loop {
            let count = match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            result.total_bytes = result.total_bytes.saturating_add(count as u64);
            if result.retained.len() < retain_limit {
                let keep = (retain_limit - result.retained.len()).min(count);
                result.retained.extend_from_slice(&chunk[..keep]);
            }
            result.overflowed |= result.total_bytes > result_cap as u64;
            let frame = PtyOutputFrame {
                stream_id: stream_id.clone(),
                execution_generation: generation.clone(),
                session_target_id: session_target_id.clone(),
                registration_generation,
                worker_incarnation,
                sequence,
                data: chunk[..count].to_vec(),
            };
            sequence = match sequence.checked_add(1) {
                Some(next) => next,
                None => {
                    stream_state.store(STREAM_SLOW, Ordering::Release);
                    break;
                }
            };
            match live_tx.try_send(WorkerToService::ExecPtyOutput(frame)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    stream_state.store(STREAM_SLOW, Ordering::Release);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    stream_state.store(STREAM_CLOSED, Ordering::Release);
                    break;
                }
            }
        }
        result
    }

    fn finish_result(
        exit_code: i32,
        duration: Duration,
        reader: ReaderResult,
        cap: usize,
    ) -> AgentOutcome {
        let projected = project_terminal_text(&reader.retained);
        let redacted = match RegexRedactor::new().redact(&projected) {
            Ok(redacted) => redacted,
            Err(_) => {
                return AgentOutcome::Err(agent_error(
                    AgentErrorKind::RedactionFailed,
                    "PTY output withheld: redaction failed".to_string(),
                ));
            }
        };
        let (terminal, truncated) = finalize(redacted.text, cap, reader.overflowed);
        AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
            exit_code,
            streams: ExecOutputStreams::PtyCombined {
                terminal,
                truncated,
            },
            duration_ms: duration.as_millis().min(u32::MAX as u128) as u32,
            redactions: redacted.kinds,
        }))
    }

    fn finalize(text: String, cap: usize, overflowed: bool) -> (String, bool) {
        if text.len() <= cap && !overflowed {
            return (text, false);
        }
        let mut end = cap.min(text.len());
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        (text[..end].to_string(), true)
    }

    fn reclaim_group(pgid: Option<libc::pid_t>) {
        if let Some(pgid) = pgid {
            // SAFETY: a negative pid addresses exactly the child-created process
            // group returned by portable-pty. ESRCH is the normal already-gone
            // case and needs no retry.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }

    /// Produce inert, plain text for result backfill. Live rendering still sees
    /// the original PTY bytes, but model-visible output cannot contain OSC 52,
    /// DCS/APC/PM payloads, CSI effects, or clickable terminal escape sequences.
    fn project_terminal_text(bytes: &[u8]) -> String {
        #[derive(Clone, Copy)]
        enum State {
            Ground,
            Escape,
            Csi,
            String,
            StringEscape,
        }

        let mut state = State::Ground;
        let mut plain = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            state = match state {
                State::Ground => match byte {
                    0x1b => State::Escape,
                    0x9b => State::Csi,
                    0x90 | 0x98 | 0x9d | 0x9e | 0x9f => State::String,
                    b'\n' | b'\r' | b'\t' => {
                        plain.push(byte);
                        State::Ground
                    }
                    0x08 => {
                        plain.pop();
                        State::Ground
                    }
                    0x00..=0x1f | 0x7f => State::Ground,
                    _ => {
                        plain.push(byte);
                        State::Ground
                    }
                },
                State::Escape => match byte {
                    b'[' => State::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => State::String,
                    0x20..=0x2f => State::Escape,
                    _ => State::Ground,
                },
                State::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        State::Ground
                    } else {
                        State::Csi
                    }
                }
                State::String => match byte {
                    0x07 => State::Ground,
                    0x1b => State::StringEscape,
                    _ => State::String,
                },
                State::StringEscape => {
                    if byte == b'\\' {
                        State::Ground
                    } else if byte == 0x1b {
                        State::StringEscape
                    } else {
                        State::String
                    }
                }
            };
        }
        String::from_utf8_lossy(&plain).into_owned()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use desk_agent_protocol::exec::{
            ApprovalId, ExecContainmentSnapshot, ExecExecutionBasis, ExecIoMode, ExecRequestId,
            ExecShellKind,
        };
        use desk_agent_protocol::{OperationOutput, RiskLevel};

        #[test]
        fn projector_removes_terminal_side_effect_sequences() {
            let input = b"safe\x1b]52;c;c2VjcmV0\x07 text\x1b[31mred\x1b[0m\x1bPignored\x1b\\!";
            assert_eq!(project_terminal_text(input), "safe textred!");
        }

        #[tokio::test]
        async fn echo_off_input_is_absent_from_live_and_model_visible_output() {
            let generation = format!("pty-test-{}", uuid::Uuid::new_v4());
            let plan = ExecPlan {
                exec_request_id: ExecRequestId("pty-test-request".into()),
                execution_generation: generation.clone(),
                program: "sh".into(),
                argv: vec![
                    "-c".into(),
                    "stty -echo; printf 'READY\\n'; IFS= read -r value; printf 'accepted\\n'"
                        .into(),
                ],
                cwd: None,
                shell: ExecShellKind::Native,
                risk: RiskLevel::Low,
                io_mode: ExecIoMode::Pty {
                    initial_rows: 24,
                    initial_cols: 80,
                },
                execution_basis: ExecExecutionBasis::Template,
                template_id: "pty-test".into(),
                approval_id: ApprovalId("pty-test-approval".into()),
                fingerprint: "pty-test-fingerprint".into(),
                timeout_ms: 5_000,
                max_stdout_bytes: 4096,
                max_stderr_bytes: 4096,
                containment: ExecContainmentSnapshot::default(),
            };
            let registry = ExecRegistry::new();
            let (cancel, _registration) = registry.register(&generation);
            let (live_tx, mut live_rx) = mpsc::channel(8);
            let task_plan = plan.clone();
            let task_registry = registry.clone();
            let task = tokio::task::spawn_blocking(move || {
                run(
                    task_plan,
                    "pty-test-stream".into(),
                    "pty-test-session".into(),
                    7,
                    11,
                    task_registry,
                    cancel.subscribe(),
                    live_tx,
                    |_| {},
                )
            });

            let canary = b"lcxl-pty-input-canary-93f1\n";
            let mut live_output = Vec::new();
            let opened = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match live_rx.recv().await.expect("PTY event stream closed") {
                        WorkerToService::ExecPtyOpened(opened) => break opened,
                        WorkerToService::ExecPtyOutput(output) => {
                            live_output.extend_from_slice(&output.data);
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("PTY did not open");

            tokio::time::timeout(Duration::from_secs(3), async {
                while !live_output
                    .windows(b"READY".len())
                    .any(|part| part == b"READY")
                {
                    if let WorkerToService::ExecPtyOutput(output) =
                        live_rx.recv().await.expect("PTY event stream closed")
                    {
                        live_output.extend_from_slice(&output.data);
                    }
                }
            })
            .await
            .expect("PTY command did not disable echo");

            registry
                .write_pty_input(desk_agent_protocol::exec_pty::PtyInputFrame {
                    stream_id: opened.stream_id.clone(),
                    execution_generation: opened.execution_generation.clone(),
                    session_target_id: opened.session_target_id.clone(),
                    registration_generation: opened.registration_generation,
                    worker_incarnation: opened.worker_incarnation,
                    sequence: 0,
                    data: canary.to_vec(),
                })
                .unwrap();

            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match live_rx.recv().await.expect("PTY event stream closed") {
                        WorkerToService::ExecPtyOutput(output) => {
                            live_output.extend_from_slice(&output.data);
                        }
                        WorkerToService::ExecPtyClosed(_) => break,
                        _ => {}
                    }
                }
            })
            .await
            .expect("PTY command did not close");

            let outcome = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .expect("PTY executor did not finish")
                .expect("PTY executor task panicked");
            assert!(
                !live_output
                    .windows(canary.len() - 1)
                    .any(|part| part == &canary[..canary.len() - 1])
            );
            let terminal = match outcome {
                AgentOutcome::Ok(OperationOutput::Exec(ExecOutput {
                    streams: ExecOutputStreams::PtyCombined { terminal, .. },
                    ..
                })) => terminal,
                other => panic!("expected PTY result, got {other:?}"),
            };
            assert!(terminal.contains("accepted"), "{terminal:?}");
            assert!(!terminal.contains("lcxl-pty-input-canary-93f1"));
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use desk_agent_protocol::exec::ExecPlan;
    use desk_agent_protocol::{AgentErrorKind, AgentOutcome};
    use desk_ipc_protocol::message::{ExecSpawnReport, WorkerToService};
    use tokio::sync::{mpsc, watch};

    use crate::worker::exec_pty::agent_error;
    use crate::worker::exec_registry::ExecRegistry;

    #[allow(clippy::too_many_arguments)]
    pub(super) fn run(
        _plan: ExecPlan,
        _stream_id: String,
        _session_target_id: String,
        _registration_generation: u64,
        _worker_incarnation: u64,
        _registry: ExecRegistry,
        _cancel: watch::Receiver<bool>,
        _live_tx: mpsc::Sender<WorkerToService>,
        on_spawn: impl FnOnce(ExecSpawnReport),
    ) -> AgentOutcome {
        on_spawn(ExecSpawnReport::Failed {
            reason: "AI exec PTY is not available on this platform".to_string(),
        });
        AgentOutcome::Err(agent_error(
            AgentErrorKind::InvalidInput,
            "AI exec PTY is not available on this platform".to_string(),
        ))
    }
}
