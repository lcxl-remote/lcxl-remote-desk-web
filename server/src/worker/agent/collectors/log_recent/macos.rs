//! macOS backend for `log.recent`: read the unified log via `log show`.
//!
//! `log show --style ndjson` emits one JSON object per log entry, which the
//! collector parses line by line. A `source` filter becomes a `--predicate`
//! matching the entry's `subsystem` or `process`; the source value is validated
//! against a strict charset (`[A-Za-z0-9._-]`) before interpolation so it can
//! never break out of the quoted predicate (and `Command` args bypass the shell
//! entirely). Unified logging has no native "warning" level, so severity
//! filtering is applied in Rust after parsing rather than via the predicate.

use std::process::Command;

use desk_agent_protocol::{AgentError, AgentErrorKind, LogSeverity};

/// Time window when the caller does not specify `since_minutes`. `log show`
/// requires a bounded window or it walks the entire on-disk store, so a default
/// is always supplied.
pub(super) const DEFAULT_SINCE_MIN: u32 = 60;

/// Run `log show` and return the raw ndjson stdout. The time window is the
/// requested `since_minutes` (or [`DEFAULT_SINCE_MIN`]); a `source` that fails
/// charset validation is dropped rather than injected.
pub(super) fn query(params: &super::LogRecentParams, _limit: u32) -> Result<String, AgentError> {
    let since = params.since_minutes.unwrap_or(DEFAULT_SINCE_MIN).max(1);

    let mut command = Command::new("log");
    command.args(["show", "--style", "ndjson", "--info"]);
    // Include debug-level entries only when the caller asks for them; they are
    // voluminous and off by default in the unified log.
    if params.severity.contains(&LogSeverity::Debug) {
        command.arg("--debug");
    }
    command.args(["--last", &format!("{since}m")]);

    if let Some(source) = params.source.as_deref()
        && let Some(predicate) = source_predicate(source)
    {
        command.args(["--predicate", &predicate]);
    }

    let output = command.output().map_err(|e| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to run log show: {e}"),
        retryable: true,
        safe_for_model: true,
        error_code: None,
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: format!("log show failed: {}", stderr.trim()),
            retryable: true,
            safe_for_model: true,
            error_code: None,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Build a `log show` predicate restricting results to entries whose subsystem
/// or process matches `source`. Returns `None` for an empty source or one
/// containing any character outside `[A-Za-z0-9._-]`, so an untrusted value can
/// never alter the predicate's structure. Platform-agnostic for unit testing.
#[cfg(any(target_os = "macos", test))]
fn source_predicate(source: &str) -> Option<String> {
    let valid = !source.is_empty()
        && source
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !valid {
        return None;
    }
    Some(format!(
        "(subsystem == \"{source}\") || (process == \"{source}\")"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_predicate_for_valid_source() {
        let predicate = source_predicate("com.apple.network").expect("valid source");
        assert_eq!(
            predicate,
            "(subsystem == \"com.apple.network\") || (process == \"com.apple.network\")"
        );
    }

    #[test]
    fn rejects_sources_that_could_inject() {
        // Quotes, spaces, and predicate operators are all outside the charset.
        assert!(source_predicate("\" OR 1==1 OR \"").is_none());
        assert!(source_predicate("foo bar").is_none());
        assert!(source_predicate("foo\"").is_none());
        assert!(source_predicate("").is_none());
    }
}
