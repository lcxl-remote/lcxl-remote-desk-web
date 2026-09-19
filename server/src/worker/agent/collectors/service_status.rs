//! Native service discovery, metadata enrichment and result-based pagination.
use desk_agent_protocol::native_diagnostic::{DiagnosticStage, NativeDiagnostic};
use desk_agent_protocol::{
    AgentError, AgentErrorKind, ServiceEntry, ServiceStatusOutput, ServiceStatusParams,
};
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod command;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod pagination;
#[cfg(windows)]
mod windows;

#[derive(Default)]
struct Enumeration {
    services: Vec<ServiceEntry>,
    errors: Vec<NativeDiagnostic>,
    scope: String,
}

fn diagnostic(stage: DiagnosticStage, operation: &str, message: &str) -> NativeDiagnostic {
    NativeDiagnostic::new(stage, operation, "application", None, message)
}
fn invalid(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
pub fn ready() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/run/systemd/system").is_dir() && which::which("systemctl").is_ok()
    }
    #[cfg(any(windows, target_os = "macos"))]
    {
        true
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

pub fn collect(params: &ServiceStatusParams) -> Result<ServiceStatusOutput, AgentError> {
    params
        .validate_platform(std::env::consts::OS)
        .map_err(invalid)?;
    #[cfg(target_os = "linux")]
    use linux as backend;
    #[cfg(target_os = "macos")]
    use macos as backend;
    #[cfg(windows)]
    use windows as backend;
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut found = backend::enumerate(params, deadline);
        found.services.retain(|s| {
            params.queries.is_empty()
                || !desk_agent_protocol::matching_search_terms(
                    &params.queries,
                    &[&s.name, s.display_name.as_deref().unwrap_or("")],
                )
                .is_empty()
        });
        pagination::page_with(found, params, |service| {
            if Instant::now() >= deadline {
                return Err(diagnostic(
                    DiagnosticStage::Query,
                    "service metadata",
                    "Query incomplete: collection deadline exceeded",
                ));
            }
            backend::enrich(service, deadline);
            if Instant::now() >= deadline {
                return Err(diagnostic(
                    DiagnosticStage::Query,
                    "service metadata",
                    "Query incomplete: metadata deadline exceeded",
                ));
            }
            Ok(())
        })
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Err(invalid("service.status is unsupported on this platform"))
    }
}

/// Map a Win32 `SERVICE_STATUS_CURRENT_STATE` code to a stable lowercase
/// label. Shared by the single-service and bulk-enumeration paths so both
/// report identical strings. Platform-agnostic for unit testing.
#[cfg(any(windows, test))]
fn state_label(raw: u32) -> String {
    match raw {
        1 => "stopped",
        2 => "start_pending",
        3 => "stop_pending",
        4 => "running",
        5 => "continue_pending",
        6 => "pause_pending",
        7 => "paused",
        other => return format!("unknown({other})"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn live_windows_start_types_and_paging() {
        let p = ServiceStatusParams {
            allow_unfiltered: true,
            limit: 2,
            ..Default::default()
        };
        let first = collect(&p).unwrap();
        assert_eq!(first.services.len(), 2);
        assert!(
            first
                .services
                .iter()
                .all(|s| s.start_type.is_some() || s.metadata_error.is_some())
        );
        let second = collect(&ServiceStatusParams {
            cursor: first.next_cursor.clone(),
            ..p
        })
        .unwrap();
        assert!(
            second
                .services
                .iter()
                .all(|s| !first.services.iter().any(|f| f.name == s.name))
        );
        let one = collect(&ServiceStatusParams {
            queries: vec!["EventLog".into()],
            start_types: vec!["auto".into()],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(one.services.len(), 1);
        assert_eq!(one.services[0].start_type.as_deref(), Some("auto"));
    }
}
