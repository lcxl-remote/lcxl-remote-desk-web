//! `service.status` collector — native service-manager state.
//!
//! Enumerates native Windows SCM, macOS launchd or Linux systemd service metadata,
//! applies case-insensitive OR substring queries, then enforces the output cap.

use desk_agent_protocol::{AgentError, ServiceStatusOutput, ServiceStatusParams};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Hard cap on enumerated services; a host with more sets `truncated`.
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
const MAX_SERVICES: usize = 1000;

/// Whether the native service manager required by this collector is present.
/// Linux support is intentionally scoped to a booted systemd host; merely
/// finding a `systemctl` binary in a container or chroot must not advertise a
/// ready capability that every request will fail to use.
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

/// Collect services matching any requested name/display-name substring.
pub fn collect(params: &ServiceStatusParams) -> Result<ServiceStatusOutput, AgentError> {
    params.validate_selection().map_err(|message| AgentError {
        kind: desk_agent_protocol::AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    })?;
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        use linux as backend;
        #[cfg(target_os = "macos")]
        use macos as backend;
        #[cfg(windows)]
        use windows as backend;

        #[allow(unused_mut)]
        let ServiceStatusOutput {
            mut services,
            truncated,
        } = select_services(backend::enumerate_all()?, &params.queries);
        #[cfg(windows)]
        if !params.queries.is_empty() {
            for service in &mut services {
                if let Ok(metadata) = backend::read_metadata(&service.name) {
                    *service = metadata;
                }
            }
        }
        Ok(ServiceStatusOutput {
            services,
            truncated,
        })
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        let _ = params;
        Err(AgentError {
            kind: desk_agent_protocol::AgentErrorKind::UnsupportedPlatform,
            message: "service.status is not supported on this platform".to_string(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        })
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
fn select_services(
    mut services: Vec<desk_agent_protocol::ServiceEntry>,
    queries: &[String],
) -> ServiceStatusOutput {
    services.retain(|service| {
        queries.is_empty()
            || !desk_agent_protocol::matching_search_terms(
                queries,
                &[&service.name, service.display_name.as_deref().unwrap_or("")],
            )
            .is_empty()
    });
    // Stable, case-insensitive ordering by service name.
    services.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let truncated = services.len() > MAX_SERVICES;
    services.truncate(MAX_SERVICES);
    ServiceStatusOutput {
        services,
        truncated,
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

    #[test]
    fn service_queries_filter_before_limit_and_deduplicate_or_matches() {
        let entry = |name: &str, display: Option<&str>| desk_agent_protocol::ServiceEntry {
            name: name.into(),
            display_name: display.map(str::to_string),
            state: "running".into(),
            start_type: None,
        };
        let mut entries = (0..MAX_SERVICES + 1)
            .map(|i| entry(&format!("unrelated-{i}"), None))
            .collect::<Vec<_>>();
        entries.push(entry("org.calendar", Some("Calendar Agent")));
        entries.push(entry("org.other", Some("Print Spooler")));
        let found = select_services(
            entries,
            &["CALEND".into(), "calendar agent".into(), "spool".into()],
        );
        assert!(!found.truncated);
        assert_eq!(
            found
                .services
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["org.calendar", "org.other"]
        );
        assert!(
            select_services(found.services, &["missing".into()])
                .services
                .is_empty()
        );
        assert!(collect(&ServiceStatusParams::default()).is_err());
        assert!(
            collect(&ServiceStatusParams {
                queries: vec![" ".into()],
                allow_unfiltered: true
            })
            .is_err()
        );
    }

    #[test]
    fn state_label_maps_known_codes() {
        assert_eq!(state_label(1), "stopped");
        assert_eq!(state_label(4), "running");
        assert_eq!(state_label(7), "paused");
    }

    #[test]
    fn state_label_flags_unknown_code() {
        assert_eq!(state_label(99), "unknown(99)");
    }

    /// Live enumeration on Windows: services exist, every entry is
    /// well-formed, and queries narrow to a known service.
    /// Exercises both the SCM enumerate and single-query FFI paths.
    #[cfg(windows)]
    #[test]
    fn live_windows_enumerate_and_query() {
        let all = collect(&ServiceStatusParams {
            allow_unfiltered: true,
            ..Default::default()
        })
        .expect("enumerate must succeed");
        assert!(!all.services.is_empty());
        assert!(all.services.iter().all(|s| !s.name.is_empty()));

        // The Event Log service exists on every Windows host.
        let one = collect(&ServiceStatusParams {
            queries: vec!["EventLog".to_string()],
            ..Default::default()
        })
        .expect("single query must succeed");
        assert_eq!(one.services.len(), 1);
        assert_eq!(one.services[0].name, "EventLog");
        assert!(one.services[0].start_type.is_some());

        // An unmatched search is an empty result.
        let missing = collect(&ServiceStatusParams {
            queries: vec!["definitely-not-a-real-service-xyz".to_string()],
            ..Default::default()
        })
        .expect("unmatched search succeeds");
        assert!(missing.services.is_empty());
    }

    /// Live enumeration on macOS: launchd jobs exist, every entry is
    /// well-formed, and queries narrow to a known job. Exercises
    /// the `launchctl list` parse path end to end.
    #[cfg(target_os = "macos")]
    #[test]
    fn live_macos_enumerate_and_query() {
        let all = collect(&ServiceStatusParams {
            allow_unfiltered: true,
            ..Default::default()
        })
        .expect("enumerate must succeed");
        assert!(!all.services.is_empty());
        assert!(all.services.iter().all(|s| !s.name.is_empty()));
        assert!(all.services.iter().all(|s| s.start_type.is_none()));

        // Pick an enumerated job and confirm substring matching finds it.
        let label = all.services[0].name.clone();
        let one = collect(&ServiceStatusParams {
            queries: vec![label.clone()],
            ..Default::default()
        })
        .expect("single query must succeed");
        assert_eq!(one.services.len(), 1);
        assert_eq!(one.services[0].name, label);

        // An unmatched search is an empty result.
        let missing = collect(&ServiceStatusParams {
            queries: vec!["definitely-not-a-real-launchd-job-xyz".to_string()],
            ..Default::default()
        })
        .expect("unmatched search succeeds");
        assert!(missing.services.is_empty());
    }

    /// On platforms without a backend, the collector degrades rather than
    /// failing the transport.
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_off_windows() {
        let err = collect(&ServiceStatusParams {
            allow_unfiltered: true,
            ..Default::default()
        })
        .expect_err("must be unsupported");
        assert_eq!(
            err.kind,
            desk_agent_protocol::AgentErrorKind::UnsupportedPlatform
        );
    }
}
