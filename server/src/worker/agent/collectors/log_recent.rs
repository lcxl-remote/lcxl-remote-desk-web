//! `log.recent` collector — recent log events.
//!
//! Windows reads the Event Log via `Get-WinEvent` (PowerShell), which resolves
//! publisher metadata into formatted messages for us — matching the codebase's
//! existing pattern of shelling structured queries to PowerShell (driver ops).
//! The severity / source / since / limit filters map onto a `Get-WinEvent`
//! `FilterHashtable`. macOS reads the unified log via `log show --style ndjson`,
//! parsing each entry and filtering severity in Rust (the unified log has no
//! native "warning" level). Other platforms return `UnsupportedPlatform`
//! (journald integration is deferred).

use desk_agent_protocol::{AgentError, LogEvent, LogRecentOutput, LogRecentParams, LogSeverity};
// `AgentErrorKind` is only referenced by the Windows JSON parser and the
// unsupported-platform fallback; on macOS (outside tests) neither is compiled.
#[cfg(not(all(target_os = "macos", not(test))))]
use desk_agent_protocol::AgentErrorKind;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Returned-event count when the caller does not specify a limit.
const DEFAULT_LIMIT: u32 = 100;
/// Hard ceiling on returned events regardless of the requested limit.
const MAX_LIMIT: u32 = 500;

/// Collect recent log events, filtered by the request. The effective limit is
/// the requested one clamped to `[1, MAX_LIMIT]` (default `DEFAULT_LIMIT`).
pub fn collect(params: &LogRecentParams) -> Result<LogRecentOutput, AgentError> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    #[cfg(windows)]
    {
        let json = windows::query(params, limit)?;
        let mut events = parse_events_json(&json)?;
        // Hitting the cap means the source likely held more matching events.
        let truncated = events.len() as u32 >= limit;
        events.truncate(limit as usize);
        Ok(LogRecentOutput { events, truncated })
    }
    #[cfg(target_os = "macos")]
    {
        let ndjson = macos::query(params, limit)?;
        let mut events = parse_ndjson(&ndjson);
        // The unified log cannot filter severity via predicate, so apply the
        // requested severities here (empty means "any").
        if !params.severity.is_empty() {
            events.retain(|e| params.severity.contains(&e.severity));
        }
        let truncated = events.len() as u32 >= limit;
        events.truncate(limit as usize);
        Ok(LogRecentOutput { events, truncated })
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (params, limit);
        Err(AgentError {
            kind: AgentErrorKind::UnsupportedPlatform,
            message: "log.recent is not supported on this platform".to_string(),
            retryable: false,
            safe_for_model: true,
        })
    }
}

/// Map a macOS unified-log `messageType` to a protocol severity. The unified
/// log levels are Debug / Info / Default / Error / Fault; with no native
/// "warning" tier, `Default` and `Info` both fold into `Info`. Platform-agnostic
/// for unit testing.
#[cfg(any(target_os = "macos", test))]
fn message_type_to_severity(message_type: &str) -> LogSeverity {
    match message_type.to_ascii_lowercase().as_str() {
        "error" | "fault" => LogSeverity::Error,
        "debug" => LogSeverity::Debug,
        // "info", "default", and any unknown type.
        _ => LogSeverity::Info,
    }
}

/// Parse `log show --style ndjson` output into log events, one per line. Lines
/// that are not valid JSON, or that carry no `eventMessage` (activity / signpost
/// entries), are skipped. The source is the entry's subsystem when present,
/// otherwise its process name. Platform-agnostic so the parse is unit tested on
/// any host.
#[cfg(any(target_os = "macos", test))]
fn parse_ndjson(text: &str) -> Vec<LogEvent> {
    #[derive(serde::Deserialize)]
    struct RawEntry {
        timestamp: Option<String>,
        #[serde(rename = "messageType")]
        message_type: Option<String>,
        subsystem: Option<String>,
        process: Option<String>,
        #[serde(rename = "eventMessage")]
        event_message: Option<String>,
    }

    let mut events = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawEntry>(line) else {
            continue;
        };
        let Some(message) = raw.event_message.filter(|m| !m.is_empty()) else {
            continue;
        };
        let source = raw
            .subsystem
            .filter(|s| !s.is_empty())
            .or(raw.process)
            .unwrap_or_default();
        events.push(LogEvent {
            timestamp: raw.timestamp.unwrap_or_default(),
            source,
            severity: message_type_to_severity(raw.message_type.as_deref().unwrap_or("default")),
            message,
            // Redaction is applied downstream by the diagnose pipeline.
            redactions: Vec::new(),
        });
    }
    events
}

/// Map requested protocol severities to the Windows Event Log numeric levels
/// for a `Get-WinEvent` `Level` filter. Empty input means "no level filter".
#[cfg(any(windows, test))]
fn severity_to_levels(severities: &[LogSeverity]) -> Vec<u8> {
    let mut levels = Vec::new();
    for severity in severities {
        match severity {
            // Critical + Error.
            LogSeverity::Error => levels.extend([1u8, 2]),
            LogSeverity::Warning => levels.push(3),
            // LogAlways + Information.
            LogSeverity::Info => levels.extend([0u8, 4]),
            // Verbose.
            LogSeverity::Debug => levels.push(5),
        }
    }
    levels.sort_unstable();
    levels.dedup();
    levels
}

/// Map a Windows Event Log numeric level to a protocol severity.
#[cfg(any(windows, test))]
fn level_to_severity(level: u32) -> LogSeverity {
    match level {
        // Critical, Error.
        1 | 2 => LogSeverity::Error,
        3 => LogSeverity::Warning,
        // Verbose.
        5 => LogSeverity::Debug,
        // 0 (LogAlways), 4 (Information), and any unknown level.
        _ => LogSeverity::Info,
    }
}

/// Parse the `Get-WinEvent | ConvertTo-Json` output into log events. Tolerates
/// the three shapes PowerShell emits: an array, a single object (PS unwraps a
/// one-element array), and empty/`null` (no matches).
#[cfg(any(windows, test))]
fn parse_events_json(json: &str) -> Result<Vec<LogEvent>, AgentError> {
    #[derive(serde::Deserialize)]
    struct RawEvent {
        timestamp: Option<String>,
        level: Option<u32>,
        source: Option<String>,
        message: Option<String>,
    }

    let trimmed = json.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    let parse_err = |e: serde_json::Error| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to parse Get-WinEvent output: {e}"),
        retryable: true,
        safe_for_model: true,
    };

    let value: serde_json::Value = serde_json::from_str(trimmed).map_err(parse_err)?;
    let raw_events = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Null => Vec::new(),
        single => vec![single],
    };

    raw_events
        .into_iter()
        .map(|item| {
            let raw: RawEvent = serde_json::from_value(item).map_err(parse_err)?;
            Ok(LogEvent {
                timestamp: raw.timestamp.unwrap_or_default(),
                source: raw.source.unwrap_or_default(),
                severity: level_to_severity(raw.level.unwrap_or(4)),
                message: raw.message.unwrap_or_default(),
                // The redaction pipeline lands in M1b; nothing is scrubbed yet.
                redactions: Vec::new(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_maps_to_levels_deduped() {
        assert_eq!(severity_to_levels(&[LogSeverity::Error]), vec![1, 2]);
        assert_eq!(severity_to_levels(&[LogSeverity::Warning]), vec![3]);
        assert_eq!(severity_to_levels(&[LogSeverity::Info]), vec![0, 4]);
        assert_eq!(severity_to_levels(&[LogSeverity::Debug]), vec![5]);
        // Union is sorted and deduped.
        assert_eq!(
            severity_to_levels(&[LogSeverity::Error, LogSeverity::Warning]),
            vec![1, 2, 3]
        );
        assert!(severity_to_levels(&[]).is_empty());
    }

    #[test]
    fn level_maps_to_severity() {
        assert_eq!(level_to_severity(1), LogSeverity::Error);
        assert_eq!(level_to_severity(2), LogSeverity::Error);
        assert_eq!(level_to_severity(3), LogSeverity::Warning);
        assert_eq!(level_to_severity(4), LogSeverity::Info);
        assert_eq!(level_to_severity(5), LogSeverity::Debug);
        assert_eq!(level_to_severity(0), LogSeverity::Info);
        assert_eq!(level_to_severity(99), LogSeverity::Info);
    }

    #[test]
    fn parses_json_array() {
        let json = r#"[
            {"timestamp":"2026-06-12T10:00:00.0000000Z","level":2,"source":"Service Control Manager","message":"boom"},
            {"timestamp":"2026-06-12T10:01:00.0000000Z","level":3,"source":"DCOM","message":"warn"}
        ]"#;
        let events = parse_events_json(json).expect("parse");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].severity, LogSeverity::Error);
        assert_eq!(events[0].source, "Service Control Manager");
        assert_eq!(events[0].message, "boom");
        assert!(events[0].redactions.is_empty());
        assert_eq!(events[1].severity, LogSeverity::Warning);
    }

    #[test]
    fn parses_single_object_shape() {
        // PowerShell unwraps a one-element array to a bare object.
        let json =
            r#"{"timestamp":"2026-06-12T10:00:00Z","level":4,"source":"App","message":"hi"}"#;
        let events = parse_events_json(json).expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, LogSeverity::Info);
    }

    #[test]
    fn empty_and_null_yield_no_events() {
        assert!(parse_events_json("").expect("empty").is_empty());
        assert!(parse_events_json("   ").expect("blank").is_empty());
        assert!(parse_events_json("null").expect("null").is_empty());
    }

    #[test]
    fn missing_fields_default_gracefully() {
        let json = r#"[{"level":null,"source":null,"message":null,"timestamp":null}]"#;
        let events = parse_events_json(json).expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].severity, LogSeverity::Info);
        assert_eq!(events[0].message, "");
        assert_eq!(events[0].timestamp, "");
    }

    #[test]
    fn message_type_maps_to_severity() {
        assert_eq!(message_type_to_severity("Error"), LogSeverity::Error);
        assert_eq!(message_type_to_severity("Fault"), LogSeverity::Error);
        assert_eq!(message_type_to_severity("Debug"), LogSeverity::Debug);
        assert_eq!(message_type_to_severity("Info"), LogSeverity::Info);
        assert_eq!(message_type_to_severity("Default"), LogSeverity::Info);
        // Unknown / absent types fall back to Info.
        assert_eq!(message_type_to_severity("whatever"), LogSeverity::Info);
    }

    #[test]
    fn parses_ndjson_entries() {
        let text = "{\"timestamp\":\"2026-06-21 10:00:00.000000-0700\",\"messageType\":\"Error\",\"subsystem\":\"com.apple.network\",\"process\":\"networkd\",\"eventMessage\":\"link down\"}\n\
                    {\"timestamp\":\"2026-06-21 10:01:00.000000-0700\",\"messageType\":\"Default\",\"subsystem\":\"\",\"process\":\"kernel\",\"eventMessage\":\"boot\"}\n";
        let events = parse_ndjson(text);
        assert_eq!(events.len(), 2);

        assert_eq!(events[0].severity, LogSeverity::Error);
        // Subsystem wins as the source when present.
        assert_eq!(events[0].source, "com.apple.network");
        assert_eq!(events[0].message, "link down");
        assert!(events[0].redactions.is_empty());

        assert_eq!(events[1].severity, LogSeverity::Info);
        // Falls back to the process name when subsystem is empty.
        assert_eq!(events[1].source, "kernel");
    }

    #[test]
    fn ndjson_skips_non_log_and_malformed_lines() {
        let text = "not json at all\n\
                    {\"timestamp\":\"t\",\"messageType\":\"activityCreateEvent\"}\n\
                    \n\
                    {\"timestamp\":\"t\",\"messageType\":\"Info\",\"process\":\"p\",\"eventMessage\":\"keep me\"}\n";
        let events = parse_ndjson(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message, "keep me");
    }

    /// Live read on macOS: the unified log always holds recent entries. Verifies
    /// the `log show` round trip produces well-formed, capped output. `log show`
    /// can take several seconds to walk the store, so this is `#[ignore]`d by
    /// default and run explicitly.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "log show can take several seconds to scan the unified log store"]
    fn live_macos_reads_unified_log() {
        let out = collect(&LogRecentParams {
            source: None,
            since_minutes: Some(60),
            limit: Some(5),
            severity: Vec::new(),
        })
        .expect("unified log read must succeed");
        assert!(out.events.len() <= 5);
        for event in &out.events {
            assert!(!event.message.is_empty());
        }
    }

    /// Live read on Windows: the System log always has recent events. Verifies
    /// the PowerShell round trip produces well-formed, capped output.
    #[cfg(windows)]
    #[test]
    fn live_windows_reads_system_log() {
        let out = collect(&LogRecentParams {
            source: Some("System".to_string()),
            since_minutes: None,
            limit: Some(5),
            severity: Vec::new(),
        })
        .expect("system log read must succeed");
        assert!(out.events.len() <= 5);
        for event in &out.events {
            assert!(!event.timestamp.is_empty());
            assert!(!event.source.is_empty());
        }
    }
}
