//! Windows backend for `log.recent`: read the Event Log via `Get-WinEvent`.
//!
//! Filter values are passed as **environment variables** and referenced inside
//! the script (never interpolated into the command text), so a caller-supplied
//! source or level can never inject PowerShell. `Get-WinEvent` resolves each
//! event's formatted message itself.

use std::process::Command;

use desk_agent_protocol::{AgentError, AgentErrorKind, LogRecentParams};

use super::severity_to_levels;

/// PowerShell script: build a `FilterHashtable` from the env vars, query, and
/// emit a compact JSON array of `{timestamp, level, source, message}`.
const SCRIPT: &str = r"
$ErrorActionPreference = 'Stop'
$filter = @{}
if ($env:LCXL_LOG_SOURCE) { $filter['LogName'] = $env:LCXL_LOG_SOURCE } else { $filter['LogName'] = @('System','Application') }
if ($env:LCXL_LOG_LEVELS) { $filter['Level'] = [int[]]($env:LCXL_LOG_LEVELS -split ',') }
if ($env:LCXL_LOG_SINCE_MIN) { $filter['StartTime'] = (Get-Date).AddMinutes(-[double]$env:LCXL_LOG_SINCE_MIN) }
$max = [int]$env:LCXL_LOG_MAX
$events = Get-WinEvent -FilterHashtable $filter -MaxEvents $max -ErrorAction SilentlyContinue | Select-Object @{N='timestamp';E={$_.TimeCreated.ToUniversalTime().ToString('o')}}, @{N='level';E={[int]$_.Level}}, @{N='source';E={$_.ProviderName}}, @{N='message';E={$_.Message}}
ConvertTo-Json -InputObject @($events) -Depth 3 -Compress
";

/// Run the Event Log query and return the raw JSON stdout.
pub(super) fn query(params: &LogRecentParams, limit: u32) -> Result<String, AgentError> {
    let mut command = Command::new("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT]);

    if let Some(source) = &params.source {
        command.env("LCXL_LOG_SOURCE", source);
    }
    let levels = severity_to_levels(&params.severity);
    if !levels.is_empty() {
        let joined = levels
            .iter()
            .map(|level| level.to_string())
            .collect::<Vec<_>>()
            .join(",");
        command.env("LCXL_LOG_LEVELS", joined);
    }
    if let Some(since) = params.since_minutes {
        command.env("LCXL_LOG_SINCE_MIN", since.to_string());
    }
    command.env("LCXL_LOG_MAX", limit.to_string());

    let output = command.output().map_err(|e| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to run Get-WinEvent: {e}"),
        retryable: true,
        safe_for_model: true,
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AgentError {
            kind: AgentErrorKind::Internal,
            message: format!("Get-WinEvent failed: {}", stderr.trim()),
            retryable: true,
            safe_for_model: true,
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
