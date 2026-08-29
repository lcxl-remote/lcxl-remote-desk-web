//! Pure projection from the edge Browser Provider heartbeat into the shared
//! discoverable/callable capability readiness contract.

use desk_agent_protocol::{
    browser_control::{BrowserReadiness, BrowserReadinessReason},
    capability_provider::{
        CAPABILITY_PROVIDER_SCHEMA_VERSION, CapabilityBlockedReason, CapabilityReadinessReport,
    },
};

pub const MAX_BROWSER_READINESS_TTL_MS: u64 = 30_000;

pub fn browser_readiness_report(
    provider_id: &str,
    capability_id: &str,
    readiness: &BrowserReadiness,
    expires_at_unix_ms: u64,
    local_ceiling_revision: u64,
) -> Result<CapabilityReadinessReport, String> {
    readiness
        .validate()
        .map_err(|error| format!("invalid browser readiness: {error}"))?;
    if expires_at_unix_ms <= readiness.observed_at_unix_ms
        || expires_at_unix_ms - readiness.observed_at_unix_ms > MAX_BROWSER_READINESS_TTL_MS
    {
        return Err("browser readiness expiry is outside the accepted window".into());
    }
    let reason = readiness.reason.map(map_blocked_reason);
    let report = CapabilityReadinessReport {
        schema_version: CAPABILITY_PROVIDER_SCHEMA_VERSION,
        provider_id: provider_id.into(),
        capability_id: capability_id.into(),
        adapter_id: Some(readiness.adapter.adapter_id.clone()),
        adapter_version: Some(readiness.adapter.adapter_version.clone()),
        revision: readiness.adapter.connection_revision,
        observed_at_unix_ms: readiness.observed_at_unix_ms,
        expires_at_unix_ms,
        local_ceiling_revision,
        compiled: true,
        enabled: true,
        connected: readiness.connected,
        ready: readiness.connected,
        reason,
    };
    report
        .validate()
        .map_err(|error| format!("invalid projected browser readiness: {error}"))?;
    Ok(report)
}

fn map_blocked_reason(reason: BrowserReadinessReason) -> CapabilityBlockedReason {
    match reason {
        BrowserReadinessReason::UnsupportedBrowserVersion => {
            CapabilityBlockedReason::VersionMismatch
        }
        BrowserReadinessReason::ExtensionUnavailable => CapabilityBlockedReason::AdapterUnavailable,
        BrowserReadinessReason::PairingRequired => CapabilityBlockedReason::BrowserApprovalRequired,
        BrowserReadinessReason::HostPermissionMissing => CapabilityBlockedReason::PermissionMissing,
        BrowserReadinessReason::RemoteDebuggingDisabled => {
            CapabilityBlockedReason::RemoteDebuggingDisabled
        }
        BrowserReadinessReason::UserApprovalRequired | BrowserReadinessReason::UserDenied => {
            CapabilityBlockedReason::BrowserApprovalRequired
        }
        BrowserReadinessReason::McpUnavailable
        | BrowserReadinessReason::Disconnected
        | BrowserReadinessReason::ProfileChanged => CapabilityBlockedReason::BrowserDisconnected,
        BrowserReadinessReason::InteractiveSessionLocked => {
            CapabilityBlockedReason::NoInteractiveSession
        }
        BrowserReadinessReason::Busy => CapabilityBlockedReason::Busy,
    }
}

#[cfg(test)]
mod tests {
    use desk_agent_protocol::browser_control::{
        BROWSER_CONTROL_SCHEMA_VERSION, BrowserAdapterRef, BrowserEngineKind, BrowserToolKind,
    };

    use super::*;

    fn readiness(connected: bool) -> BrowserReadiness {
        BrowserReadiness {
            schema_version: BROWSER_CONTROL_SCHEMA_VERSION,
            adapter: BrowserAdapterRef {
                engine: BrowserEngineKind::ChromeDevtoolsMcp,
                device_id: "device-1".into(),
                os_session_id: "session-1".into(),
                browser_major_version: 144,
                browser_version: "144.0.7559.0".into(),
                adapter_id: "browser.chrome_devtools_mcp.edge".into(),
                adapter_version: "chrome-devtools-mcp/1.7.0".into(),
                profile_incarnation: "profile-incarnation-1".into(),
                connection_revision: 7,
            },
            adapter_enabled: connected,
            user_authorized: connected,
            connected,
            interactive_session_unlocked: true,
            tools: if connected {
                vec![BrowserToolKind::TakeSnapshot]
            } else {
                Vec::new()
            },
            reason: (!connected).then_some(BrowserReadinessReason::RemoteDebuggingDisabled),
            observed_at_unix_ms: 100,
        }
    }

    #[test]
    fn connected_browser_is_callable_only_for_short_lived_revision() {
        let report = browser_readiness_report(
            "browser.devtools",
            "browser.page.snapshot",
            &readiness(true),
            30_100,
            9,
        )
        .unwrap();
        assert!(report.ready);
        assert!(report.connected);
        assert_eq!(report.revision, 7);
        assert_eq!(
            report.adapter_id.as_deref(),
            Some("browser.chrome_devtools_mcp.edge")
        );
    }

    #[test]
    fn disabled_remote_debugging_is_discoverable_but_not_callable() {
        let report = browser_readiness_report(
            "browser.devtools",
            "browser.page.snapshot",
            &readiness(false),
            30_100,
            9,
        )
        .unwrap();
        assert!(!report.ready);
        assert!(!report.connected);
        assert_eq!(
            report.reason,
            Some(CapabilityBlockedReason::RemoteDebuggingDisabled)
        );
    }

    #[test]
    fn stale_browser_heartbeat_is_rejected() {
        assert!(
            browser_readiness_report(
                "browser.devtools",
                "browser.page.snapshot",
                &readiness(true),
                30_101,
                9,
            )
            .is_err()
        );
    }
}
