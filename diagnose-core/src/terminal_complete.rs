//! Terminal AI command completion: model-agnostic prompt assembly, response
//! parsing with the server-authoritative per-candidate execution decision, and
//! the subject-namespaced cache key.
//!
//! Shared by the web portable runtime and the manager central orchestrator so the
//! two can never drift. Completion is non-agentic and latency-sensitive: a single
//! tool-free model call turns the operator's command prefix into a short list of
//! full command lines, and this module parses them, keeps only those that truly
//! extend the prefix, and — crucially — stamps each candidate's `risk` /
//! `decision` itself via the shared [`crate::exec_classify`] classifier. The
//! model's own output never carries those fields, so a prompt-injected model
//! cannot mark a command as safe to run, and a `Blocked` candidate is dropped
//! before it can ever reach the operator as ghost text.

use desk_agent_protocol::exec::ExecDecision;
use desk_agent_protocol::terminal_complete::{CommandCompletion, TerminalCompleteAsk};
use desk_agent_protocol::{ExecInput, ExecTarget};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::chat::{ChatMessage, ChatRole};
use crate::exec_classify::classify_command;
use crate::parser::{extract_json_object, truncate_on_char_boundary};
use crate::prompt::ResponseFormatSpec;
use crate::redaction::Redactor;
use crate::seam::ModelRequest;

/// Max command-line candidates kept from one completion turn. Ghost-text shows the
/// best one inline; the rest back a cycle-through affordance.
pub const MAX_COMPLETIONS: usize = 5;

/// Upper bound on the typed prefix forwarded to the model (after redaction). A
/// longer prefix is truncated from the front so the tail the operator is actively
/// typing is preserved.
pub const MAX_PREFIX_BYTES: usize = 1_024;

/// Raw browser-input limits enforced before redaction or any cache/governance work.
pub const MAX_RAW_OS_BYTES: usize = 256;
pub const MAX_RAW_SHELL_BYTES: usize = 256;
pub const MAX_RAW_CWD_BYTES: usize = 4_096;
pub const MAX_RAW_RECENT_OUTPUT_BYTES: usize = 8_192;

/// Provider-bound environment limits applied only after full-field redaction.
pub const MAX_PROVIDER_OS_BYTES: usize = 64;
pub const MAX_PROVIDER_SHELL_BYTES: usize = 64;
pub const MAX_PROVIDER_CWD_BYTES: usize = 1_024;

/// Max bytes of recent terminal output forwarded to the model (after the runtime
/// has redacted it). Caps prompt size / latency.
pub const MAX_RECENT_OUTPUT_BYTES: usize = 2_048;

/// Max bytes kept for a single completion suffix. A candidate longer than this is
/// dropped rather than truncated (a half command is worse than none).
pub const MAX_COMPLETION_BYTES: usize = 512;

/// Hard output cap for a latency-sensitive, non-agentic completion turn.
pub const COMPLETION_MAX_OUTPUT_TOKENS: u32 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionRawField {
    Os,
    Shell,
    Cwd,
    RecentOutput,
    Prefix,
}

/// Reject oversized raw fields before redaction, normalization, cache lookup or
/// model governance. This bounds the work performed on attacker-controlled text.
pub fn validate_completion_raw_input(ask: &TerminalCompleteAsk) -> Result<(), CompletionRawField> {
    for (field, len, max) in [
        (
            CompletionRawField::Os,
            ask.context.os.len(),
            MAX_RAW_OS_BYTES,
        ),
        (
            CompletionRawField::Shell,
            ask.context.shell.len(),
            MAX_RAW_SHELL_BYTES,
        ),
        (
            CompletionRawField::Cwd,
            ask.context.cwd.as_deref().map(str::len).unwrap_or(0),
            MAX_RAW_CWD_BYTES,
        ),
        (
            CompletionRawField::RecentOutput,
            ask.context.recent_output.len(),
            MAX_RAW_RECENT_OUTPUT_BYTES,
        ),
        (
            CompletionRawField::Prefix,
            ask.prefix.len(),
            MAX_PREFIX_BYTES,
        ),
    ] {
        if len > max {
            return Err(field);
        }
    }
    Ok(())
}

/// Build the completion system prompt. Not persisted (completion is stateless):
/// re-sent on every call.
pub fn build_completion_system_message() -> ChatMessage {
    let body = "You are a shell command completion engine embedded in a remote \
         terminal session.\n\
         The operator has typed a partial command line; propose the most likely \
         full command lines that complete it.\n\n\
         Rules:\n\
         - You only advise. You never execute anything; the operator alone decides \
         whether to accept a completion. Never claim a command has been run.\n\
         - Each candidate MUST be a full command line that BEGINS with the exact \
         prefix the operator typed (you are extending it, not rewriting it).\n\
         - Use the operator's OS and shell (given in the request). Prefer common, \
         safe, single commands; avoid destructive operations.\n\
         - Order candidates best-first. Return at most a handful.\n\n\
         Reply with a SINGLE JSON object and nothing else:\n\
         {\"completions\": [{\"command\": \"<full command line starting with the \
         prefix>\", \"note\": \"<one line: what it does>\"}]}\n\
         Do not include risk or approval fields — the server computes those. \
         `completions` may be empty when nothing sensible applies.";
    ChatMessage::text("complete-system", ChatRole::System, body)
}

/// Build the user turn from the ask: the (non-authoritative) environment hints
/// plus the typed prefix, with recent output length-capped. The runtime must
/// redact `context` and `prefix` before calling this.
pub fn build_completion_user_message(ask: &TerminalCompleteAsk) -> ChatMessage {
    let ctx = &ask.context;
    let mut body = String::new();
    body.push_str(&format!("OS: {}\nShell: {}\n", ctx.os, ctx.shell));
    if let Some(cwd) = &ctx.cwd {
        body.push_str(&format!("CWD: {cwd}\n"));
    }
    let recent = truncate_on_char_boundary(ctx.recent_output.trim(), MAX_RECENT_OUTPUT_BYTES);
    if !recent.is_empty() {
        body.push_str(&format!("\nRecent terminal output:\n{recent}\n"));
    }
    body.push_str(&format!("\nPrefix to complete:\n{}\n", ask.prefix));
    ChatMessage::text("complete-user", ChatRole::User, body)
}

/// Build the shared model request so manager and OSS signal apply the same output
/// cap and tool-free response contract.
pub fn build_completion_model_request(ask: &TerminalCompleteAsk) -> ModelRequest {
    let mut request = ModelRequest::text_only(
        vec![
            build_completion_system_message(),
            build_completion_user_message(ask),
        ],
        ResponseFormatSpec::None,
    );
    request.use_case = crate::model_profile::ModelUseCase::Completion;
    request.caller_output_hard_cap = Some(i64::from(COMPLETION_MAX_OUTPUT_TOKENS));
    request
}

/// Outcome of redacting a completion ask before any model dial. Shared by both
/// runtimes so the fail-closed / decline policy can never drift.
#[derive(Debug, PartialEq, Eq)]
pub enum CompletionRedaction {
    /// The ask was cleaned in place and is safe to send to the model.
    Ready,
    /// The typed prefix carried content the redactor would scrub. Because the
    /// prefix is echoed verbatim into the ghost text, scrubbing it would either
    /// leak the secret to the model or mis-attach the suffix — so the turn is
    /// declined with no candidates (no error: the operator just gets no ghost
    /// text for that keystroke).
    DeclineSensitivePrefix,
    /// The redactor itself failed; the turn must abort fail-closed (the
    /// content-free reason is carried for logging).
    Failed(String),
}

/// Redact a completion ask fail-closed before any model dial.
///
/// Every environment field is scrubbed and capped in place. The `prefix` is then checked: if
/// redacting it would change it, the prefix carries sensitive content and the
/// turn is declined ([`CompletionRedaction::DeclineSensitivePrefix`]) rather than
/// dialled — the prefix must reach the model verbatim (it is the suffix anchor),
/// so a sensitive prefix is never sent at all. Any redactor error aborts
/// ([`CompletionRedaction::Failed`]).
pub fn redact_completion_ask(
    redactor: &dyn Redactor,
    ask: &mut TerminalCompleteAsk,
) -> CompletionRedaction {
    let redact = |value: &str| {
        redactor
            .redact(value)
            .map(|result| result.text)
            .map_err(|error| CompletionRedaction::Failed(error.reason))
    };
    ask.context.os = match redact(&ask.context.os) {
        Ok(value) => truncate_on_char_boundary(&value, MAX_PROVIDER_OS_BYTES).to_string(),
        Err(outcome) => return outcome,
    };
    ask.context.shell = match redact(&ask.context.shell) {
        Ok(value) => truncate_on_char_boundary(&value, MAX_PROVIDER_SHELL_BYTES).to_string(),
        Err(outcome) => return outcome,
    };
    if let Some(cwd) = ask.context.cwd.take() {
        ask.context.cwd = match redact(&cwd) {
            Ok(value) => {
                Some(truncate_on_char_boundary(&value, MAX_PROVIDER_CWD_BYTES).to_string())
            }
            Err(outcome) => return outcome,
        };
    }
    ask.context.recent_output = match redact(&ask.context.recent_output) {
        Ok(value) => truncate_on_char_boundary(&value, MAX_RECENT_OUTPUT_BYTES).to_string(),
        Err(outcome) => return outcome,
    };
    match redactor.redact(&ask.prefix) {
        Ok(r) if r.text == ask.prefix => CompletionRedaction::Ready,
        Ok(_) => CompletionRedaction::DeclineSensitivePrefix,
        Err(e) => CompletionRedaction::Failed(e.reason),
    }
}

/// The model's raw completion list. The candidate shape carries no `risk` /
/// `decision`, so a model cannot self-report them — the classifier computes them.
#[derive(Deserialize)]
struct RawCompletions {
    #[serde(default)]
    completions: Vec<RawCompletion>,
}

#[derive(Deserialize)]
struct RawCompletion {
    command: String,
    #[serde(default)]
    note: String,
}

/// Parse the model's answer into completion candidates, computing the
/// server-authoritative `risk` / `decision` for each over the full command line.
///
/// A candidate is kept only when its command genuinely extends `prefix` (so the
/// ghost text is a true suffix), the suffix is non-empty and within
/// [`MAX_COMPLETION_BYTES`], and the command is not [`ExecDecision::Blocked`] —
/// a blocked command offers nothing as ghost text and is dropped at the source.
/// `default_shell` is the operator's shell, used to classify the command. Returns
/// at most [`MAX_COMPLETIONS`], de-duplicated, order preserved.
pub fn parse_completions(
    content: &str,
    prefix: &str,
    default_shell: &str,
) -> Vec<CommandCompletion> {
    let Some(raw) =
        extract_json_object(content).and_then(|j| serde_json::from_str::<RawCompletions>(j).ok())
    else {
        return Vec::new();
    };

    let mut out: Vec<CommandCompletion> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for cand in raw.completions {
        let full = cand.command.trim_end();
        // The candidate must extend exactly what the operator typed; otherwise the
        // suffix would not be valid ghost text.
        let Some(suffix) = full.strip_prefix(prefix) else {
            continue;
        };
        if suffix.is_empty() || suffix.len() > MAX_COMPLETION_BYTES {
            continue;
        }
        let classification = classify_full_command(full, default_shell);
        // A blocked command is never offered — drop it before it can be shown.
        if classification.decision == ExecDecision::Blocked {
            continue;
        }
        if seen.iter().any(|s| s == suffix) {
            continue;
        }
        seen.push(suffix.to_string());
        out.push(CommandCompletion {
            completion: suffix.to_string(),
            note: cand.note,
            risk: classification.risk,
            decision: classification.decision,
        });
        if out.len() >= MAX_COMPLETIONS {
            break;
        }
    }
    out
}

/// The risk / decision the shared classifier computes for one full command line.
struct FullClassification {
    risk: desk_agent_protocol::RiskLevel,
    decision: ExecDecision,
}

/// Classify a full command line through the shared exec classifier.
fn classify_full_command(command: &str, shell: &str) -> FullClassification {
    let input = ExecInput {
        target: ExecTarget::Shell {
            shell: shell.to_string(),
        },
        command: command.to_string(),
        cwd: None,
        timeout_ms: 0,
        max_stdout_bytes: 0,
        max_stderr_bytes: 0,
    };
    let c = classify_command(&input).classification;
    FullClassification {
        risk: c.risk,
        decision: c.decision,
    }
}

/// Append one length-prefixed field (u32 little-endian length + raw bytes) to the
/// hash. The length prefix makes the concatenation unambiguous, so distinct
/// subjects / environments can never alias onto one cache entry.
fn absorb_field(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u32).to_le_bytes());
    hasher.update(field);
}

/// Derive the subject- and environment-namespaced completion cache key.
///
/// The cache is the cross-instance store for completion results; this key folds in
/// the trusted subject (`actor` / `device`) **and** the environment the completion
/// is specific to (`os` / `shell`) **and** the typed `prefix`, all length-prefixed.
/// Two different subjects (or OS / shell / prefix) therefore never share an entry,
/// so a cached completion derived from one operator's session can never leak into
/// another's (security item: cross-subject cache isolation). The result is a
/// lowercase hex SHA-256 digest, safe to use as a Redis key segment.
pub fn derive_completion_cache_key(
    actor_id: &str,
    device_id: &str,
    ask: &TerminalCompleteAsk,
) -> String {
    let mut hasher = Sha256::new();
    absorb_field(&mut hasher, actor_id.as_bytes());
    absorb_field(&mut hasher, device_id.as_bytes());
    absorb_field(&mut hasher, ask.context.os.as_bytes());
    absorb_field(&mut hasher, ask.context.shell.as_bytes());
    absorb_field(
        &mut hasher,
        ask.context.cwd.as_deref().unwrap_or_default().as_bytes(),
    );
    absorb_field(&mut hasher, ask.context.recent_output.as_bytes());
    absorb_field(&mut hasher, ask.prefix.as_bytes());

    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redaction::RegexRedactor;
    use desk_agent_protocol::RiskLevel;
    use desk_agent_protocol::terminal_complete::TerminalCompletionContext;

    fn ask(prefix: &str) -> TerminalCompleteAsk {
        TerminalCompleteAsk {
            prefix: prefix.into(),
            context: TerminalCompletionContext {
                os: "linux".into(),
                shell: "bash".into(),
                cwd: Some("/srv".into()),
                recent_output: "$ systemctl status".into(),
            },
            model_id: None,
            org_id: None,
        }
    }

    #[test]
    fn system_prompt_states_suffix_and_no_run_constraints() {
        let p = build_completion_system_message().text;
        assert!(p.contains("BEGINS with the exact prefix"));
        assert!(p.contains("never execute"));
        assert!(p.contains("SINGLE JSON object"));
    }

    #[test]
    fn user_message_caps_recent_output_and_keeps_prefix() {
        let mut a = ask("systemctl ");
        a.context.recent_output = "x".repeat(MAX_RECENT_OUTPUT_BYTES * 2);
        let msg = build_completion_user_message(&a).text;
        assert!(msg.contains("OS: linux"));
        assert!(msg.contains("Prefix to complete:\nsystemctl"));
        assert!(msg.len() < MAX_RECENT_OUTPUT_BYTES + 512);
    }

    #[test]
    fn keeps_only_candidates_that_extend_the_prefix() {
        let content = r#"{"completions": [
            {"command": "systemctl status nginx", "note": "status"},
            {"command": "journalctl -u nginx", "note": "logs (does not extend prefix)"}
        ]}"#;
        let got = parse_completions(content, "systemctl ", "bash");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].completion, "status nginx");
        assert_eq!(got[0].note, "status");
    }

    #[test]
    fn blocked_candidate_is_dropped_not_self_reported() {
        // The model self-reports a benign decision; the classifier overrides it and
        // the blocked command is dropped entirely (never shown as ghost text).
        let content = r#"{"completions": [
            {"command": "cat /etc/shadow", "note": "read", "decision": "not_executable", "risk": "low"}
        ]}"#;
        let got = parse_completions(content, "cat ", "bash");
        assert!(got.is_empty());
    }

    #[test]
    fn off_template_read_is_suggest_only_not_executable() {
        let content = r#"{"completions": [{"command": "ss -ltnp", "note": "listeners"}]}"#;
        let got = parse_completions(content, "ss ", "bash");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].decision, ExecDecision::NotExecutable);
    }

    #[test]
    fn empty_suffix_and_duplicates_and_overlong_are_dropped() {
        let long = format!("ls {}", "a".repeat(MAX_COMPLETION_BYTES + 1));
        let content = format!(
            r#"{{"completions": [
                {{"command": "ls", "note": "exact prefix, empty suffix"}},
                {{"command": "ls -la", "note": "first"}},
                {{"command": "ls -la", "note": "dup suffix"}},
                {{"command": "{long}", "note": "overlong"}}
            ]}}"#
        );
        let got = parse_completions(&content, "ls", "bash");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].completion, " -la");
    }

    #[test]
    fn malformed_json_yields_no_candidates() {
        assert!(parse_completions("sorry, no JSON", "ls", "bash").is_empty());
    }

    #[test]
    fn caps_candidate_count() {
        let items: Vec<String> = (0..MAX_COMPLETIONS + 3)
            .map(|i| format!(r#"{{"command": "echo {i}", "note": "n"}}"#))
            .collect();
        let content = format!(r#"{{"completions": [{}]}}"#, items.join(","));
        let got = parse_completions(&content, "echo ", "bash");
        assert_eq!(got.len(), MAX_COMPLETIONS);
    }

    #[test]
    fn risk_is_classifier_computed_not_model_reported() {
        // The model reports nothing about risk/decision; the kept candidate still
        // carries a classifier-stamped, non-blocked decision (a blocked one would
        // have been dropped). This proves the fields come from the classifier.
        let content = r#"{"completions": [{"command": "echo hi", "note": "n"}]}"#;
        let got = parse_completions(content, "echo ", "bash");
        assert_eq!(got.len(), 1);
        assert_ne!(got[0].decision, ExecDecision::Blocked);
        assert!(got[0].risk < RiskLevel::Blocked);
    }

    fn key(actor: &str, device: &str, os: &str, shell: &str, prefix: &str) -> String {
        let mut ask = ask(prefix);
        ask.context.os = os.to_string();
        ask.context.shell = shell.to_string();
        derive_completion_cache_key(actor, device, &ask)
    }

    #[test]
    fn cache_key_is_stable_hex_sha256() {
        let a = key("u", "d", "linux", "bash", "ls -");
        assert_eq!(a, key("u", "d", "linux", "bash", "ls -"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn cache_key_isolates_subject_and_environment() {
        let base = key("actorA", "dev1", "linux", "bash", "ls -");
        // Different actor / device never share a cache entry.
        assert_ne!(base, key("actorB", "dev1", "linux", "bash", "ls -"));
        assert_ne!(base, key("actorA", "dev2", "linux", "bash", "ls -"));
        // Different OS / shell / prefix never alias.
        assert_ne!(base, key("actorA", "dev1", "windows", "bash", "ls -"));
        assert_ne!(base, key("actorA", "dev1", "linux", "pwsh", "ls -"));
        assert_ne!(base, key("actorA", "dev1", "linux", "bash", "ls"));
    }

    #[test]
    fn redaction_scrubs_recent_output_and_keeps_clean_prefix() {
        use crate::redaction::RegexRedactor;
        let mut a = ask("systemctl ");
        a.context.recent_output = "AWS key AKIAIOSFODNN7EXAMPLE here".into();
        let outcome = redact_completion_ask(&RegexRedactor::new(), &mut a);
        assert_eq!(outcome, CompletionRedaction::Ready);
        assert!(!a.context.recent_output.contains("AKIAIOSFODNN7EXAMPLE"));
        assert_eq!(a.prefix, "systemctl ");
    }

    #[test]
    fn redaction_declines_a_sensitive_prefix_instead_of_dialling() {
        use crate::redaction::RegexRedactor;
        // A secret in the prefix itself: completing it would either leak it to the
        // model or mis-attach the suffix, so the turn is declined (no candidates).
        let mut a = ask("aws configure set secret AKIAIOSFODNN7EXAMPLE");
        let outcome = redact_completion_ask(&RegexRedactor::new(), &mut a);
        assert_eq!(outcome, CompletionRedaction::DeclineSensitivePrefix);
    }

    #[test]
    fn raw_gate_rejects_each_field_before_processing() {
        let mut cases = Vec::new();
        let mut value = ask("ok");
        value.context.os = "x".repeat(MAX_RAW_OS_BYTES + 1);
        cases.push((value, CompletionRawField::Os));
        let mut value = ask("ok");
        value.context.shell = "x".repeat(MAX_RAW_SHELL_BYTES + 1);
        cases.push((value, CompletionRawField::Shell));
        let mut value = ask("ok");
        value.context.cwd = Some("x".repeat(MAX_RAW_CWD_BYTES + 1));
        cases.push((value, CompletionRawField::Cwd));
        let mut value = ask("ok");
        value.context.recent_output = "x".repeat(MAX_RAW_RECENT_OUTPUT_BYTES + 1);
        cases.push((value, CompletionRawField::RecentOutput));
        let value = ask(&"x".repeat(MAX_PREFIX_BYTES + 1));
        cases.push((value, CompletionRawField::Prefix));

        for (ask, expected) in cases {
            assert_eq!(validate_completion_raw_input(&ask), Err(expected));
        }
    }

    #[test]
    fn redaction_caps_all_provider_bound_context_and_request_output() {
        let mut ask = ask("echo ");
        ask.context.os = "o".repeat(MAX_PROVIDER_OS_BYTES + 10);
        ask.context.shell = "s".repeat(MAX_PROVIDER_SHELL_BYTES + 10);
        ask.context.cwd = Some("c".repeat(MAX_PROVIDER_CWD_BYTES + 10));
        ask.context.recent_output = "r".repeat(MAX_RECENT_OUTPUT_BYTES + 10);
        assert_eq!(
            redact_completion_ask(&RegexRedactor::new(), &mut ask),
            CompletionRedaction::Ready
        );
        assert_eq!(ask.context.os.len(), MAX_PROVIDER_OS_BYTES);
        assert_eq!(ask.context.shell.len(), MAX_PROVIDER_SHELL_BYTES);
        assert_eq!(
            ask.context.cwd.as_ref().unwrap().len(),
            MAX_PROVIDER_CWD_BYTES
        );
        assert_eq!(ask.context.recent_output.len(), MAX_RECENT_OUTPUT_BYTES);
        assert_eq!(
            build_completion_model_request(&ask).caller_output_hard_cap,
            Some(i64::from(COMPLETION_MAX_OUTPUT_TOKENS))
        );
    }

    #[test]
    fn cache_key_includes_cwd_and_recent_output() {
        let base = ask("echo ");
        let mut changed_cwd = base.clone();
        changed_cwd.context.cwd = Some("/other".into());
        let mut changed_output = base.clone();
        changed_output.context.recent_output = "different".into();
        let key = derive_completion_cache_key("actor", "device", &base);
        assert_ne!(
            key,
            derive_completion_cache_key("actor", "device", &changed_cwd)
        );
        assert_ne!(
            key,
            derive_completion_cache_key("actor", "device", &changed_output)
        );
    }

    #[test]
    fn cache_key_length_prefix_prevents_field_aliasing() {
        // Without length prefixing, ("ab","c") and ("a","bc") could collide.
        assert_ne!(
            key("ab", "c", "linux", "bash", "p"),
            key("a", "bc", "linux", "bash", "p"),
        );
    }
}
