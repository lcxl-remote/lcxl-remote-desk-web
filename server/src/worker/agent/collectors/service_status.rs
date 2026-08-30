//! `service.status` collector — native service-manager state.
//!
//! Platform-dispatched:
//! - **Windows** queries the Service Control Manager. A `name` filter opens
//!   that one service (full state + start type via the `windows-service`
//!   crate); without a filter, all `SERVICE_WIN32` services are enumerated via
//!   `EnumServicesStatusExW` (state only — per-service config is skipped for
//!   bulk enumeration, so `start_type` is `None`).
//! - **macOS** lists launchd jobs via `launchctl list`. A `name` filter selects
//!   the job with that exact label; without a filter, all loaded jobs are
//!   returned. State is `running` (numeric PID) or `stopped` (`-`); launchd
//!   exposes no per-job start type, so `start_type` is always `None`.
//! - **Linux** queries loaded systemd service units through structured
//!   `systemctl show` properties.
//! - Other platforms return `UnsupportedPlatform`.

use desk_agent_protocol::{AgentError, ServiceStatusOutput, ServiceStatusParams};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Hard cap on enumerated services; a host with more sets `truncated`.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
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

/// Collect service status. With `params.name` set, returns that single
/// service; otherwise enumerates all services.
pub fn collect(params: &ServiceStatusParams) -> Result<ServiceStatusOutput, AgentError> {
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    {
        #[cfg(target_os = "linux")]
        use linux as backend;
        #[cfg(target_os = "macos")]
        use macos as backend;
        #[cfg(windows)]
        use windows as backend;

        let mut services = match params.name.as_deref() {
            Some(name) => vec![backend::query_one(name)?],
            None => backend::enumerate_all()?,
        };
        // Stable, case-insensitive ordering by service name.
        services.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        let truncated = services.len() > MAX_SERVICES;
        services.truncate(MAX_SERVICES);
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
    /// well-formed, and a `name` filter narrows to a single known service.
    /// Exercises both the SCM enumerate and single-query FFI paths.
    #[cfg(windows)]
    #[test]
    fn live_windows_enumerate_and_query() {
        let all = collect(&ServiceStatusParams::default()).expect("enumerate must succeed");
        assert!(!all.services.is_empty());
        assert!(all.services.iter().all(|s| !s.name.is_empty()));

        // The Event Log service exists on every Windows host.
        let one = collect(&ServiceStatusParams {
            name: Some("EventLog".to_string()),
        })
        .expect("single query must succeed");
        assert_eq!(one.services.len(), 1);
        assert_eq!(one.services[0].name, "EventLog");
        assert!(one.services[0].start_type.is_some());

        // A non-existent service is an input error, not a crash.
        let missing = collect(&ServiceStatusParams {
            name: Some("definitely-not-a-real-service-xyz".to_string()),
        })
        .expect_err("missing service must reject");
        assert_eq!(
            missing.kind,
            desk_agent_protocol::AgentErrorKind::InvalidInput
        );
    }

    /// Live enumeration on macOS: launchd jobs exist, every entry is
    /// well-formed, and a `name` filter narrows to that single job. Exercises
    /// the `launchctl list` parse path end to end.
    #[cfg(target_os = "macos")]
    #[test]
    fn live_macos_enumerate_and_query() {
        let all = collect(&ServiceStatusParams::default()).expect("enumerate must succeed");
        assert!(!all.services.is_empty());
        assert!(all.services.iter().all(|s| !s.name.is_empty()));
        assert!(all.services.iter().all(|s| s.start_type.is_none()));

        // Pick any enumerated job and confirm the single-query path finds it.
        let label = all.services[0].name.clone();
        let one = collect(&ServiceStatusParams {
            name: Some(label.clone()),
        })
        .expect("single query must succeed");
        assert_eq!(one.services.len(), 1);
        assert_eq!(one.services[0].name, label);

        // A non-existent label is an input error, not a crash.
        let missing = collect(&ServiceStatusParams {
            name: Some("definitely-not-a-real-launchd-job-xyz".to_string()),
        })
        .expect_err("missing service must reject");
        assert_eq!(
            missing.kind,
            desk_agent_protocol::AgentErrorKind::InvalidInput
        );
    }

    /// On platforms without a backend, the collector degrades rather than
    /// failing the transport.
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    #[test]
    fn unsupported_off_windows() {
        let err = collect(&ServiceStatusParams::default()).expect_err("must be unsupported");
        assert_eq!(
            err.kind,
            desk_agent_protocol::AgentErrorKind::UnsupportedPlatform
        );
    }
}
