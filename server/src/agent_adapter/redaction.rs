//! Host-side Provider result redaction (security model §9).
//!
//! The string-level secret scrubber ([`Redactor`] / [`RegexRedactor`]) is shared
//! with the manager via [`desk_diagnose_core::redaction`] so both runtimes scrub
//! with the same §9 pattern set and can never drift. This module re-exports it
//! and adds the edge-only [`redact_snapshot`], which walks the edge's concrete
//! evidence output types and scrubs every free-text field before the snapshot
//! reaches the model.
//!
//! Redaction is **fail-closed**: if the redactor cannot run, the orchestrator
//! refuses to send to the model, audits `ai.redaction.failed`, and degrades the
//! UI — it never falls back to sending raw evidence. The [`Redactor`] trait
//! returns a `Result` so a failing implementation (or a panicking one, wrapped
//! by the caller) forces that path.

use desk_agent_protocol::{AgentOutcome, OperationOutput, ReadContextOutput};
pub use desk_diagnose_core::redaction::{Redacted, RedactionError, Redactor, RegexRedactor};

use crate::worker::agent::eval::EvidenceSnapshot;

/// Redact every free-text field of a collected snapshot in place, recording the
/// kind tags on each entry (and on the protocol output's own `redactions` list
/// where one exists) and returning the total number of secrets removed.
///
/// Only text-heavy read outputs are scanned: log messages, container log lines,
/// and container inspect JSON. The other reads are structured, non-free-text
/// values (core counts, port numbers, service states); process command lines are
/// already dropped at collection time (`command_line_redacted`).
///
/// Returns `Err` on the first redactor failure — the caller treats that as
/// fail-closed.
pub fn redact_snapshot(
    redactor: &dyn Redactor,
    snapshot: &mut EvidenceSnapshot,
) -> Result<u32, RedactionError> {
    let mut total: u32 = 0;
    for entry in &mut snapshot.contexts {
        let mut entry_kinds: Vec<String> = Vec::new();
        if let AgentOutcome::Ok(OperationOutput::ReadContext(read)) = &mut entry.outcome {
            match read {
                ReadContextOutput::LogRecent(out) => {
                    for event in &mut out.events {
                        let redacted = redactor.redact(&event.message)?;
                        event.message = redacted.text;
                        event.redactions.extend(redacted.kinds.iter().cloned());
                        entry_kinds.extend(redacted.kinds);
                    }
                }
                ReadContextOutput::ContainerLogs(out) => {
                    for line in &mut out.lines {
                        let redacted = redactor.redact(line)?;
                        *line = redacted.text;
                        out.redactions.extend(redacted.kinds.iter().cloned());
                        entry_kinds.extend(redacted.kinds);
                    }
                }
                ReadContextOutput::ContainerInspect(out) => {
                    let redacted = redactor.redact(&out.details_json)?;
                    out.details_json = redacted.text;
                    out.redactions.extend(redacted.kinds.iter().cloned());
                    entry_kinds.extend(redacted.kinds);
                }
                _ => {}
            }
        }
        total = total.saturating_add(entry_kinds.len() as u32);
        entry.redactions.extend(entry_kinds);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{
        AgentError, AgentErrorKind, Capability, ContainerInspectOutput, ContainerLogsOutput,
        LogEvent, LogRecentOutput, LogSeverity, OperationOutput, ReadContextOutput,
    };

    fn ok_log(message: &str) -> crate::worker::agent::eval::EvidenceSnapshot {
        let out = OperationOutput::ReadContext(ReadContextOutput::LogRecent(LogRecentOutput {
            events: vec![LogEvent {
                timestamp: "t".into(),
                source: "s".into(),
                severity: LogSeverity::Error,
                message: message.into(),
                redactions: Vec::new(),
            }],
            truncated: false,
        }));
        EvidenceSnapshot::record(
            "live",
            "why?",
            "2026-06-13T00:00:00Z",
            vec![(Capability::LogRecent, AgentOutcome::Ok(out))],
        )
    }

    /// `redact_snapshot` scrubs log message text and backfills the entry's
    /// redactions plus the protocol output's own redactions list.
    #[test]
    fn redact_snapshot_scrubs_log_messages() {
        let r = RegexRedactor::new();
        let mut snap = ok_log("connect failed with token=abc123secret on retry");
        let count = redact_snapshot(&r, &mut snap).expect("redaction must succeed");

        assert_eq!(count, 1);
        let entry = &snap.contexts[0];
        assert!(entry.redactions.iter().any(|k| k == "token"));

        let AgentOutcome::Ok(OperationOutput::ReadContext(ReadContextOutput::LogRecent(out))) =
            &entry.outcome
        else {
            panic!("expected a log.recent outcome");
        };
        assert!(!out.events[0].message.contains("abc123secret"));
        assert!(out.events[0].redactions.iter().any(|k| k == "token"));
    }

    /// Container logs and inspect JSON are scrubbed too.
    #[test]
    fn redact_snapshot_scrubs_container_evidence() {
        let r = RegexRedactor::new();
        let logs =
            OperationOutput::ReadContext(ReadContextOutput::ContainerLogs(ContainerLogsOutput {
                lines: vec!["env DB password=p@ss loaded".into()],
                redactions: Vec::new(),
                truncated: false,
            }));
        let inspect = OperationOutput::ReadContext(ReadContextOutput::ContainerInspect(
            ContainerInspectOutput {
                container_id: "abc123".into(),
                details_json: r#"{"Env":["API_KEY=sk-deadbeef"]}"#.into(),
                redactions: Vec::new(),
                truncated: false,
            },
        ));
        let mut snap = EvidenceSnapshot::record(
            "live",
            "why?",
            "2026-06-13T00:00:00Z",
            vec![
                (Capability::ContainerLogs, AgentOutcome::Ok(logs)),
                (Capability::ContainerInspect, AgentOutcome::Ok(inspect)),
            ],
        );
        let count = redact_snapshot(&r, &mut snap).expect("redaction");
        assert_eq!(count, 2);
        let json = snap.to_json_pretty().unwrap();
        assert!(!json.contains("p@ss"));
        assert!(!json.contains("sk-deadbeef"));
    }

    /// An `Err` outcome in the snapshot is skipped without failing redaction.
    #[test]
    fn redact_snapshot_skips_error_outcomes() {
        let r = RegexRedactor::new();
        let mut snap = EvidenceSnapshot::record(
            "live",
            "why?",
            "2026-06-13T00:00:00Z",
            vec![(
                Capability::ContainerInspect,
                AgentOutcome::Err(AgentError {
                    kind: AgentErrorKind::InvalidInput,
                    message: "no such container".into(),
                    retryable: false,
                    safe_for_model: true,
                    error_code: None,
                }),
            )],
        );
        let count = redact_snapshot(&r, &mut snap).expect("redaction");
        assert_eq!(count, 0);
    }

    /// A failing redactor surfaces an error (the orchestrator's fail-closed
    /// trigger).
    #[test]
    fn failing_redactor_propagates_error() {
        struct FailingRedactor;
        impl Redactor for FailingRedactor {
            fn redact(&self, _input: &str) -> Result<Redacted, RedactionError> {
                Err(RedactionError {
                    reason: "boom".into(),
                })
            }
        }
        let mut snap = ok_log("token=secret");
        let err = redact_snapshot(&FailingRedactor, &mut snap).expect_err("must fail closed");
        assert_eq!(err.reason, "boom");
    }
}
