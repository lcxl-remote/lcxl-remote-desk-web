//! Linux backend for `service.status`: loaded systemd service units.

use std::process::Command;

use desk_agent_protocol::{AgentError, AgentErrorKind, ServiceEntry};

const SHOW_PROPERTIES: &str = "Id,Description,LoadState,ActiveState,SubState,UnitFileState";

pub(super) fn enumerate_all() -> Result<Vec<ServiceEntry>, AgentError> {
    let output = Command::new("systemctl")
        .args([
            "show",
            "--type=service",
            "--all",
            "--no-pager",
            "--property",
            SHOW_PROPERTIES,
        ])
        .output()
        .map_err(|cause| backend_error(format!("failed to run systemctl: {cause}")))?;
    if !output.status.success() {
        return Err(backend_error(format!(
            "systemctl show failed: {}",
            bounded_stderr(&output.stderr)
        )));
    }
    Ok(parse_systemctl_show(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

pub(super) fn query_one(name: &str) -> Result<ServiceEntry, AgentError> {
    validate_unit_name(name)?;
    let output = Command::new("systemctl")
        .args(["show", "--no-pager", "--property", SHOW_PROPERTIES, "--"])
        .arg(name)
        .output()
        .map_err(|cause| backend_error(format!("failed to run systemctl: {cause}")))?;
    if !output.status.success() {
        return Err(backend_error(format!(
            "systemctl show failed: {}",
            bounded_stderr(&output.stderr)
        )));
    }
    parse_systemctl_show(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .next()
        .filter(|entry| entry.state != "not_found")
        .ok_or_else(|| AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: format!("service {name:?} not found"),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
}

fn validate_unit_name(name: &str) -> Result<(), AgentError> {
    if name.is_empty()
        || name.len() > 256
        || name.starts_with('-')
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'@' | b'_' | b'.' | b':' | b'-' | b'\\')
        })
    {
        return Err(AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: "service name is outside the systemd unit-name bounds".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
    }
    Ok(())
}

fn parse_systemctl_show(text: &str) -> Vec<ServiceEntry> {
    text.split("\n\n").filter_map(parse_unit_block).collect()
}

fn parse_unit_block(block: &str) -> Option<ServiceEntry> {
    let mut id = None;
    let mut description = None;
    let mut load_state = None;
    let mut active_state = None;
    let mut sub_state = None;
    let mut unit_file_state = None;
    for line in block.lines() {
        let (key, value) = line.split_once('=')?;
        match key {
            "Id" => id = Some(value),
            "Description" => description = Some(value),
            "LoadState" => load_state = Some(value),
            "ActiveState" => active_state = Some(value),
            "SubState" => sub_state = Some(value),
            "UnitFileState" => unit_file_state = Some(value),
            _ => {}
        }
    }
    let name = id.filter(|value| !value.is_empty())?;
    let state = normalize_state(
        load_state.unwrap_or_default(),
        active_state.unwrap_or_default(),
        sub_state.unwrap_or_default(),
    );
    Some(ServiceEntry {
        name: name.to_string(),
        display_name: description
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        state,
        start_type: unit_file_state
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

fn normalize_state(load_state: &str, active_state: &str, sub_state: &str) -> String {
    if load_state == "not-found" {
        return "not_found".into();
    }
    match active_state {
        "active" if sub_state == "running" => "running".into(),
        "active" => "active".into(),
        "inactive" => "stopped".into(),
        "failed" => "failed".into(),
        "activating" => "start_pending".into(),
        "deactivating" => "stop_pending".into(),
        value if !value.is_empty() => value.to_string(),
        _ => "unknown".into(),
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
    fn parses_loaded_units_and_normalizes_states() {
        let text = "Id=ssh.service\nDescription=OpenSSH server\nLoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\n\nId=timer.service\nDescription=One shot\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=static\n";
        let entries = parse_systemctl_show(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "ssh.service");
        assert_eq!(entries[0].state, "running");
        assert_eq!(entries[0].start_type.as_deref(), Some("enabled"));
        assert_eq!(entries[1].state, "stopped");
    }

    #[test]
    fn rejects_option_like_or_malformed_unit_names() {
        assert!(validate_unit_name("ssh.service").is_ok());
        assert!(validate_unit_name("user@1000.service").is_ok());
        assert!(validate_unit_name("--all").is_err());
        assert!(validate_unit_name("bad unit.service").is_err());
        assert!(validate_unit_name("").is_err());
    }
}
