//! Host-side evidence redaction (security model §9).
//!
//! Before any collected evidence is sent to the model, the orchestrator scrubs
//! the free-text fields (log messages, container logs, container inspect JSON)
//! for credentials and other secrets. Redaction is **server-authoritative**:
//! the control end (a remote operator) can already see the screen, but the
//! structured evidence carries data beyond the screen (env, tokens, paths), so
//! it must never reach an external model unredacted.
//!
//! Redaction is **fail-closed**: if the redactor cannot run, the orchestrator
//! refuses to send to the model, audits `ai.redaction.failed`, and degrades the
//! UI — it never falls back to sending raw evidence. The [`Redactor`] trait
//! returns a `Result` so a failing implementation (or a panicking one, wrapped
//! by the caller) forces that path.

use std::fmt;

use desk_agent_protocol::{AgentOutcome, OperationOutput, ReadContextOutput};
use regex::Regex;

use crate::worker::agent::eval::EvidenceSnapshot;

/// The result of redacting one string: the scrubbed text plus one kind tag per
/// redaction occurrence (so the tag count equals the number of secrets removed,
/// matching the `redactions` lists carried by the protocol outputs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Redacted {
    pub text: String,
    pub kinds: Vec<String>,
}

/// A redactor failed to run. Carries a short, content-free reason for the
/// `ai.redaction.failed` audit event — never the unredacted data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactionError {
    pub reason: String,
}

impl fmt::Display for RedactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "redaction failed: {}", self.reason)
    }
}

impl std::error::Error for RedactionError {}

/// Scrubs secrets from evidence strings. Object-safe so the orchestrator holds
/// `Arc<dyn Redactor>` and tests can substitute a failing mock to exercise the
/// fail-closed path.
pub trait Redactor: Send + Sync {
    /// Redact a single string. Returns `Err` to force fail-closed.
    fn redact(&self, input: &str) -> Result<Redacted, RedactionError>;
}

/// How a pattern's match is rewritten.
enum Rewrite {
    /// Replace the whole match with `<redacted:kind>`.
    Whole,
    /// Keep capture group 1 (a non-secret prefix) and replace the rest with
    /// `<redacted:kind>`.
    KeepPrefix,
}

struct Pattern {
    kind: &'static str,
    re: Regex,
    rewrite: Rewrite,
}

impl Pattern {
    fn replacement(&self) -> String {
        match self.rewrite {
            Rewrite::Whole => format!("<redacted:{}>", self.kind),
            Rewrite::KeepPrefix => format!("${{1}}<redacted:{}>", self.kind),
        }
    }
}

/// Regex-based redactor implementing the security model §9 first-version list.
///
/// Patterns are applied in a fixed order over the accumulating text; each match
/// is rewritten and counted. The order puts multi-line / highly-specific
/// patterns (PEM blocks, cloud key formats) before the generic
/// `key = value` shapes so a specific secret is tagged with its precise kind.
pub struct RegexRedactor {
    patterns: Vec<Pattern>,
}

impl Default for RegexRedactor {
    fn default() -> Self {
        Self::new()
    }
}

impl RegexRedactor {
    /// Build the redactor with the §9 pattern set. The patterns are static and
    /// known-valid; a construction failure is a programming error (covered by a
    /// unit test), so this panics rather than returning a `Result`.
    pub fn new() -> Self {
        let specs: Vec<(&'static str, &'static str, Rewrite)> = vec![
            // PEM private key block (multi-line). Must run first so its body is
            // not partially matched by the generic value patterns.
            (
                "private_key",
                r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
                Rewrite::Whole,
            ),
            // Cloud provider key formats.
            ("aws_access_key", r"\bAKIA[0-9A-Z]{16}\b", Rewrite::Whole),
            ("gcp_api_key", r"\bAIza[0-9A-Za-z_\-]{35}\b", Rewrite::Whole),
            // HTTP credentials.
            (
                "bearer",
                r"(?i)(bearer\s+)[A-Za-z0-9\-._~+/=]{8,}",
                Rewrite::KeepPrefix,
            ),
            (
                "cookie",
                r"(?i)((?:set-)?cookie:\s*)[^\r\n]+",
                Rewrite::KeepPrefix,
            ),
            // Generic `key = value` / `key: value` shapes. `[^\s;"']+` stops at
            // a `;` so connection strings (`...;Password=secret;...`) redact
            // only the value.
            (
                "api_key",
                r#"(?i)(api[-_]?key\s*[=:]\s*["']?)[^\s;"']+"#,
                Rewrite::KeepPrefix,
            ),
            (
                "token",
                r#"(?i)((?:access[-_]?|refresh[-_]?)?token\s*[=:]\s*["']?)[^\s;"']+"#,
                Rewrite::KeepPrefix,
            ),
            (
                "password",
                r#"(?i)((?:password|passwd|pwd)\s*[=:]\s*["']?)[^\s;"']+"#,
                Rewrite::KeepPrefix,
            ),
            // Windows user directory: keep `C:\Users\`, redact the account name
            // segment (stops at the next path separator).
            (
                "windows_user_dir",
                r#"(?i)([A-Z]:\\Users\\)[^\\/:*?"<>|\r\n]+"#,
                Rewrite::KeepPrefix,
            ),
            // SSH private key paths (`~/.ssh/id_rsa`, `C:\Users\x\.ssh\id_ed25519`).
            (
                "ssh_key_path",
                r"(?i)[\w./\\~-]*[\\/]\.ssh[\\/]id_[a-z0-9_]+",
                Rewrite::Whole,
            ),
        ];

        let patterns = specs
            .into_iter()
            .map(|(kind, src, rewrite)| Pattern {
                kind,
                re: Regex::new(src)
                    .unwrap_or_else(|e| panic!("redaction pattern `{kind}` is invalid: {e}")),
                rewrite,
            })
            .collect();

        Self { patterns }
    }
}

impl Redactor for RegexRedactor {
    fn redact(&self, input: &str) -> Result<Redacted, RedactionError> {
        let mut text = input.to_string();
        let mut kinds = Vec::new();
        for pattern in &self.patterns {
            // Non-overlapping left-to-right matches == replace_all's behaviour,
            // so the count and the rewrite agree.
            let n = pattern.re.find_iter(&text).count();
            if n == 0 {
                continue;
            }
            text = pattern
                .re
                .replace_all(&text, pattern.replacement())
                .into_owned();
            for _ in 0..n {
                kinds.push(pattern.kind.to_string());
            }
        }
        Ok(Redacted { text, kinds })
    }
}

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

    fn gcp_key() -> String {
        // AIza + exactly 35 trailing chars.
        format!("AIza{}", "a".repeat(35))
    }

    /// Every §9 secret kind in the corpus is caught (zero false-negatives): the
    /// secret value is gone and the kind tag is recorded.
    #[test]
    fn corpus_redacts_every_secret_kind() {
        let r = RegexRedactor::new();
        let cases: Vec<(&str, &str, &str)> = vec![
            (
                "bearer",
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig",
                "eyJhbGciOiJIUzI1NiJ9.payload.sig",
            ),
            (
                "api_key",
                "api_key=sk-1234567890abcdef",
                "sk-1234567890abcdef",
            ),
            (
                "token",
                "access_token: ghp_ABCDEFG1234567890abcdef",
                "ghp_ABCDEFG1234567890abcdef",
            ),
            (
                "password",
                "Server=db;Password=SuperSecret123;Db=x",
                "SuperSecret123",
            ),
            (
                "aws_access_key",
                "key AKIAIOSFODNN7EXAMPLE here",
                "AKIAIOSFODNN7EXAMPLE",
            ),
            (
                "cookie",
                "Cookie: session=abc123; theme=dark",
                "session=abc123",
            ),
            (
                "windows_user_dir",
                r"path C:\Users\alice\AppData\Local",
                r"\alice\",
            ),
            (
                "ssh_key_path",
                "key at /home/bob/.ssh/id_rsa now",
                "/home/bob/.ssh/id_rsa",
            ),
        ];
        for (kind, input, secret) in cases {
            let out = r.redact(input).expect("redact must not fail");
            assert!(
                !out.text.contains(secret),
                "kind `{kind}` left the secret in: {}",
                out.text
            );
            assert!(
                out.kinds.iter().any(|k| k == kind),
                "kind `{kind}` was not tagged; tags = {:?}",
                out.kinds
            );
        }

        // GCP key and PEM block (built separately).
        let gcp = format!("token {} end", gcp_key());
        let out = r.redact(&gcp).expect("redact");
        assert!(!out.text.contains(&gcp_key()));
        assert!(out.kinds.iter().any(|k| k == "gcp_api_key"));

        let pem = "head\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKC\nAAA=\n-----END RSA PRIVATE KEY-----\ntail";
        let out = r.redact(pem).expect("redact");
        assert!(!out.text.contains("MIIEpAIBAAKC"));
        assert!(out.text.contains("head"));
        assert!(out.text.contains("tail"));
        assert!(out.kinds.iter().any(|k| k == "private_key"));
    }

    /// A clean string is returned untouched with no redaction tags.
    #[test]
    fn clean_text_is_unchanged() {
        let r = RegexRedactor::new();
        let input = "CPU at 98% on process ffmpeg.exe, 8 logical cores.";
        let out = r.redact(input).expect("redact");
        assert_eq!(out.text, input);
        assert!(out.kinds.is_empty());
    }

    /// Multiple secrets in one string are all redacted and counted (the tag
    /// count equals the number of secrets, not the number of distinct kinds).
    #[test]
    fn multiple_secrets_are_all_counted() {
        let r = RegexRedactor::new();
        let input = "api_key=AAA password=BBB token=CCC";
        let out = r.redact(input).expect("redact");
        assert!(!out.text.contains("AAA"));
        assert!(!out.text.contains("BBB"));
        assert!(!out.text.contains("CCC"));
        assert_eq!(out.kinds.len(), 3);
    }

    /// The construction of the static pattern set never panics.
    #[test]
    fn redactor_constructs() {
        let _ = RegexRedactor::new();
    }

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
