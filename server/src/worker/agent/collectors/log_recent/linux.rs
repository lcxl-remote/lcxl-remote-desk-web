//! Linux backend for `log.recent`: bounded journald JSON records.

use std::process::Command;

use chrono::{DateTime, Utc};
use desk_agent_protocol::{AgentError, AgentErrorKind, LogEvent, LogRecentParams, LogSeverity};

const DEFAULT_SINCE_MINUTES: u32 = 60;
const MAX_FETCH_LINES: u32 = 2_004;

pub(super) fn query(params: &LogRecentParams, limit: u32) -> Result<Vec<LogEvent>, AgentError> {
    validate_source(params.source.as_deref())?;
    let fetch_lines = limit
        .saturating_mul(4)
        .saturating_add(1)
        .min(MAX_FETCH_LINES);
    let since = params.since_minutes.unwrap_or(DEFAULT_SINCE_MINUTES).max(1);
    let mut command = Command::new("journalctl");
    command.args([
        "--no-pager",
        "--output=json",
        "--reverse",
        &format!("--lines={fetch_lines}"),
        &format!("--since=-{since}min"),
    ]);
    if let Some(priority) = maximum_journal_priority(&params.severity) {
        command.arg(format!("--priority={priority}"));
    }
    if let Some(source) = params.source.as_deref() {
        if source.ends_with(".service") {
            command.args(["--unit", source]);
        } else {
            command.args(["--identifier", source]);
        }
    }
    let output = command
        .output()
        .map_err(|cause| backend_error(format!("failed to run journalctl: {cause}")))?;
    if !output.status.success() {
        return Err(backend_error(format!(
            "journalctl failed: {}",
            bounded_stderr(&output.stderr)
        )));
    }
    let mut events = parse_journal_json(&String::from_utf8_lossy(&output.stdout));
    if !params.severity.is_empty() {
        events.retain(|event| params.severity.contains(&event.severity));
    }
    Ok(events)
}

fn validate_source(source: Option<&str>) -> Result<(), AgentError> {
    if source.is_some_and(|value| {
        value.is_empty()
            || value.len() > 256
            || value.chars().any(|character| character.is_control())
    }) {
        return Err(AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: "log source is outside the journald source bounds".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
    }
    Ok(())
}

fn maximum_journal_priority(severities: &[LogSeverity]) -> Option<u8> {
    severities
        .iter()
        .map(|severity| match severity {
            LogSeverity::Error => 3,
            LogSeverity::Warning => 4,
            LogSeverity::Info => 6,
            LogSeverity::Debug => 7,
        })
        .max()
}

fn parse_journal_json(text: &str) -> Vec<LogEvent> {
    text.lines().filter_map(parse_journal_line).collect()
}

fn parse_journal_line(line: &str) -> Option<LogEvent> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let object = value.as_object()?;
    let message = string_field(object, "MESSAGE")?;
    if message.is_empty() {
        return None;
    }
    let priority = string_field(object, "PRIORITY")
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(6);
    let timestamp = string_field(object, "__REALTIME_TIMESTAMP")
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|micros| DateTime::<Utc>::from_timestamp_micros(micros))
        .map(|value| value.to_rfc3339())
        .unwrap_or_default();
    let source = string_field(object, "_SYSTEMD_UNIT")
        .or_else(|| string_field(object, "SYSLOG_IDENTIFIER"))
        .or_else(|| string_field(object, "_COMM"))
        .unwrap_or_default()
        .chars()
        .take(256)
        .collect();
    Some(LogEvent {
        timestamp,
        source,
        severity: journal_priority_to_severity(priority),
        message: message.chars().take(16 * 1024).collect(),
        redactions: Vec::new(),
    })
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    object.get(key)?.as_str()
}

fn journal_priority_to_severity(priority: u8) -> LogSeverity {
    match priority {
        0..=3 => LogSeverity::Error,
        4 => LogSeverity::Warning,
        7 => LogSeverity::Debug,
        _ => LogSeverity::Info,
    }
}

fn bounded_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .trim()
        .chars()
        .take(512)
        .collect()
}

fn backend_error(message: String) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message,
        retryable: true,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_journal_json_and_maps_priority() {
        let text = concat!(
            r#"{"__REALTIME_TIMESTAMP":"1787072400000000","PRIORITY":"3","_SYSTEMD_UNIT":"ssh.service","MESSAGE":"failed"}"#,
            "\n",
            r#"{"PRIORITY":"7","SYSLOG_IDENTIFIER":"demo","MESSAGE":"trace"}"#,
            "\ninvalid\n"
        );
        let events = parse_journal_json(text);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].source, "ssh.service");
        assert_eq!(events[0].severity, LogSeverity::Error);
        assert!(!events[0].timestamp.is_empty());
        assert_eq!(events[1].severity, LogSeverity::Debug);
    }

    #[test]
    fn derives_a_bounded_priority_query_and_validates_sources() {
        assert_eq!(
            maximum_journal_priority(&[LogSeverity::Error, LogSeverity::Info]),
            Some(6)
        );
        assert_eq!(maximum_journal_priority(&[]), None);
        assert!(validate_source(Some("ssh.service")).is_ok());
        assert!(validate_source(Some("bad\nsource")).is_err());
    }
}
