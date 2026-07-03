//! Capability selection for a diagnosis, threaded with the server policy.
//!
//! The control end can *ask* for a set of read capabilities (or nothing, taking
//! the default set), but what is actually collected is the request intersected
//! with the **server-side** policy:
//!
//! - `allow_logs = false` excludes `log.recent`, `container.logs`, and raw
//!   `container.inspect`. Inspect carries env / mounts / labels, which a
//!   post-hoc regex cannot scrub reliably, so when logs are disallowed it is not
//!   collected at all.
//! - A screenshot requires **both** the request (`include_screen`) and the
//!   policy (`allow_screen`); either alone collects nothing.
//!
//! Selection is a pure function so the policy gate is unit-testable without a
//! device or a live agent. Both the edge (gating what may leave the machine) and
//! the central orchestrator (deciding what to ask for) use it.

use desk_agent_protocol::diagnose::DiagnoseRequestData;
use desk_agent_protocol::{
    Capability, ContainerListParams, ContextKind, LogRecentParams, NetworkPortsParams,
    ProcessListParams, ReadContextInput, ServiceStatusParams, SystemInfoParams,
};

/// Server-side gate on what evidence may leave the host for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionPolicy {
    pub allow_logs: bool,
    pub allow_screen: bool,
}

/// The default read set when the request names no explicit capabilities. These
/// are the parameter-free, broadly-useful reads; `container.inspect` /
/// `container.logs` need a specific container id and so are only collected when
/// explicitly requested.
fn default_read_set() -> Vec<Capability> {
    vec![
        Capability::SystemInfo,
        Capability::ProcessList,
        Capability::NetworkPorts,
        Capability::ServiceStatus,
        Capability::LogRecent,
        Capability::ContainerList,
    ]
}

/// Map a dotted capability name to its [`Capability`]. Unknown / non-collectable
/// names (e.g. `shell.exec.*`) return `None` and are dropped.
fn capability_from_name(name: &str) -> Option<Capability> {
    Some(match name {
        "system.info" => Capability::SystemInfo,
        "process.list" => Capability::ProcessList,
        "network.ports" => Capability::NetworkPorts,
        "service.status" => Capability::ServiceStatus,
        "log.recent" => Capability::LogRecent,
        "container.list" => Capability::ContainerList,
        "container.inspect" => Capability::ContainerInspect,
        "container.logs" => Capability::ContainerLogs,
        "screen.capture.current" => Capability::ScreenCaptureCurrent,
        _ => return None,
    })
}

/// Whether a capability is permitted by the request + policy.
fn is_allowed(cap: Capability, request: &DiagnoseRequestData, policy: &CollectionPolicy) -> bool {
    match cap {
        // Logs and container inspect/logs are gated by `allow_logs`.
        Capability::LogRecent | Capability::ContainerLogs | Capability::ContainerInspect => {
            policy.allow_logs
        }
        // A screenshot needs both the request and the policy.
        Capability::ScreenCaptureCurrent => request.include_screen && policy.allow_screen,
        // Reads with no sensitive free text are always allowed.
        Capability::SystemInfo
        | Capability::ProcessList
        | Capability::NetworkPorts
        | Capability::ServiceStatus
        | Capability::ContainerList => true,
        // Exec capabilities are never collected by diagnose.
        Capability::ShellExecReadonly | Capability::ShellExecConfirmed => false,
    }
}

/// The capability set a request *asks for*, before any policy filter: the
/// explicit `context_kinds` (mapped, unknown/exec names dropped) or the default
/// read set, plus the screenshot when the request toggles it, de-duplicated
/// while preserving order. This is the pre-policy intent both the edge (which
/// then filters by its local [`CollectionPolicy`]) and the central PDP (which
/// intersects it with the granted scope) start from.
pub fn requested_capabilities(request: &DiagnoseRequestData) -> Vec<Capability> {
    let mut requested: Vec<Capability> = if request.context_kinds.is_empty() {
        default_read_set()
    } else {
        request
            .context_kinds
            .iter()
            .filter_map(|name| capability_from_name(name))
            .collect()
    };
    // An explicit screenshot request adds the capability even if it was not in
    // `context_kinds` (the UI exposes it as a separate toggle).
    if request.include_screen && !requested.contains(&Capability::ScreenCaptureCurrent) {
        requested.push(Capability::ScreenCaptureCurrent);
    }
    let mut deduped = Vec::new();
    for cap in requested {
        if !deduped.contains(&cap) {
            deduped.push(cap);
        }
    }
    deduped
}

/// The dotted capability name for a collectable evidence capability, or `None`
/// for the exec capabilities (which a diagnosis never collects). Inverse of the
/// `context_kinds` name → [`Capability`] map.
pub fn capability_name(cap: Capability) -> Option<&'static str> {
    Some(match cap {
        Capability::SystemInfo => "system.info",
        Capability::ProcessList => "process.list",
        Capability::NetworkPorts => "network.ports",
        Capability::ServiceStatus => "service.status",
        Capability::LogRecent => "log.recent",
        Capability::ContainerList => "container.list",
        Capability::ContainerInspect => "container.inspect",
        Capability::ContainerLogs => "container.logs",
        Capability::ScreenCaptureCurrent => "screen.capture.current",
        Capability::ShellExecReadonly | Capability::ShellExecConfirmed => return None,
    })
}

/// Narrow a diagnose request to the capabilities the policy granted (the central
/// PDP step). Returns the request with `context_kinds` rewritten to exactly the
/// granted-and-requested capabilities and `include_screen` cleared unless a
/// screenshot is among them, or `None` when the granted scope permits no evidence
/// at all — the diagnosis must then be refused **before** any collection is asked
/// of the edge. The edge still applies its own [`CollectionPolicy`] afterwards as
/// a second, defence-in-depth gate.
///
/// Capabilities this generic path cannot actually collect (the parameterized
/// `container.inspect` / `container.logs`, which need a container id —
/// [`context_input_for`] returns `None`) are dropped here too: keeping them would
/// let a request that names *only* such a capability pass narrowing, collect an
/// empty snapshot, and still call the model. Dropping them means such a request
/// narrows to `None` and is refused up front.
pub fn narrow_to_granted(
    request: &DiagnoseRequestData,
    granted: &[Capability],
) -> Option<DiagnoseRequestData> {
    let allowed: Vec<Capability> = requested_capabilities(request)
        .into_iter()
        .filter(|cap| granted.contains(cap) && context_input_for(*cap).is_some())
        .collect();
    if allowed.is_empty() {
        return None;
    }
    let mut narrowed = request.clone();
    narrowed.context_kinds = allowed
        .iter()
        .filter_map(|cap| capability_name(*cap).map(str::to_string))
        .collect();
    narrowed.include_screen = allowed.contains(&Capability::ScreenCaptureCurrent);
    Some(narrowed)
}

/// Select the capabilities to collect for a diagnosis: the requested set
/// ([`requested_capabilities`]) filtered by the server policy, de-duplicated
/// while preserving order.
pub fn select_capabilities(
    request: &DiagnoseRequestData,
    policy: &CollectionPolicy,
) -> Vec<Capability> {
    requested_capabilities(request)
        .into_iter()
        .filter(|cap| is_allowed(*cap, request, policy))
        .collect()
}

/// Build the default read-context input for a capability, or `None` when the
/// capability needs caller-supplied parameters this generic path cannot provide
/// (a container id) or is not a read at all. The selected-but-unbuildable cases
/// are simply not collected.
pub fn context_input_for(cap: Capability) -> Option<ReadContextInput> {
    let kind = match cap {
        Capability::SystemInfo => ContextKind::SystemInfo(SystemInfoParams::default()),
        Capability::ProcessList => ContextKind::ProcessList(ProcessListParams::default()),
        Capability::NetworkPorts => ContextKind::NetworkPorts(NetworkPortsParams::default()),
        Capability::ServiceStatus => ContextKind::ServiceStatus(ServiceStatusParams::default()),
        Capability::LogRecent => ContextKind::LogRecent(LogRecentParams::default()),
        Capability::ContainerList => ContextKind::ContainerList(ContainerListParams::default()),
        Capability::ScreenCaptureCurrent => ContextKind::ScreenCaptureCurrent(Default::default()),
        // Need a container id; not auto-collectable from a question alone.
        Capability::ContainerInspect | Capability::ContainerLogs => return None,
        // Not reads.
        Capability::ShellExecReadonly | Capability::ShellExecConfirmed => return None,
    };
    Some(ReadContextInput { kind })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kinds: &[&str], include_screen: bool) -> DiagnoseRequestData {
        DiagnoseRequestData {
            question: "why?".into(),
            include_screen,
            context_kinds: kinds.iter().map(|s| s.to_string()).collect(),
            locale: None,
            conversation_id: None,
            model_id: None,
        }
    }

    /// With no explicit kinds and logs allowed, the default set is collected
    /// (including `log.recent`), but never a screenshot without a request.
    #[test]
    fn default_set_with_logs_allowed() {
        let policy = CollectionPolicy {
            allow_logs: true,
            allow_screen: true,
        };
        let caps = select_capabilities(&request(&[], false), &policy);
        assert!(caps.contains(&Capability::SystemInfo));
        assert!(caps.contains(&Capability::LogRecent));
        assert!(!caps.contains(&Capability::ScreenCaptureCurrent));
    }

    /// `allow_logs = false` removes log.recent, container.logs, and
    /// container.inspect — even when explicitly requested.
    #[test]
    fn logs_disallowed_excludes_logs_and_inspect() {
        let policy = CollectionPolicy {
            allow_logs: false,
            allow_screen: true,
        };
        let caps = select_capabilities(
            &request(
                &[
                    "system.info",
                    "log.recent",
                    "container.logs",
                    "container.inspect",
                ],
                false,
            ),
            &policy,
        );
        assert_eq!(caps, vec![Capability::SystemInfo]);
        assert!(!caps.contains(&Capability::LogRecent));
        assert!(!caps.contains(&Capability::ContainerLogs));
        assert!(!caps.contains(&Capability::ContainerInspect));
    }

    /// A screenshot needs both the request flag and the server policy.
    #[test]
    fn screen_requires_request_and_policy() {
        // Requested but policy off → excluded.
        let caps = select_capabilities(
            &request(&[], true),
            &CollectionPolicy {
                allow_logs: true,
                allow_screen: false,
            },
        );
        assert!(!caps.contains(&Capability::ScreenCaptureCurrent));

        // Policy on but not requested → excluded.
        let caps = select_capabilities(
            &request(&[], false),
            &CollectionPolicy {
                allow_logs: true,
                allow_screen: true,
            },
        );
        assert!(!caps.contains(&Capability::ScreenCaptureCurrent));

        // Both → included.
        let caps = select_capabilities(
            &request(&[], true),
            &CollectionPolicy {
                allow_logs: true,
                allow_screen: true,
            },
        );
        assert!(caps.contains(&Capability::ScreenCaptureCurrent));
    }

    /// Unknown and exec capability names are dropped from an explicit request.
    #[test]
    fn unknown_and_exec_names_are_dropped() {
        let policy = CollectionPolicy {
            allow_logs: true,
            allow_screen: true,
        };
        let caps = select_capabilities(
            &request(&["system.info", "shell.exec.readonly", "bogus.cap"], false),
            &policy,
        );
        assert_eq!(caps, vec![Capability::SystemInfo]);
    }

    /// Duplicate requested names collapse to one selection.
    #[test]
    fn duplicates_are_collapsed() {
        let policy = CollectionPolicy {
            allow_logs: true,
            allow_screen: true,
        };
        let caps = select_capabilities(&request(&["process.list", "process.list"], false), &policy);
        assert_eq!(caps, vec![Capability::ProcessList]);
    }

    /// `narrow_to_granted` keeps only the requested capabilities that the policy
    /// granted, rewriting `context_kinds` to make the gate explicit (so the edge
    /// cannot re-expand an empty request to the default set).
    #[test]
    fn narrow_keeps_only_granted_of_default_set() {
        let granted = [Capability::SystemInfo, Capability::ProcessList];
        let narrowed = narrow_to_granted(&request(&[], false), &granted).expect("non-empty");
        // The default set intersected with the grant → exactly the two granted.
        assert_eq!(
            narrowed.context_kinds,
            vec!["system.info".to_string(), "process.list".to_string()]
        );
        assert!(!narrowed.include_screen);
    }

    /// A request naming only a non-constructible parameterized capability
    /// (`container.logs` needs a container id) narrows to `None` even when it is
    /// granted, so the diagnosis is refused instead of collecting an empty
    /// snapshot and still calling the model.
    #[test]
    fn narrow_drops_unconstructable_parameterized_caps() {
        let granted = [Capability::ContainerLogs, Capability::ContainerInspect];
        assert!(narrow_to_granted(&request(&["container.logs"], false), &granted).is_none());
        // Mixed with a constructible grant, only the constructible one survives.
        let granted = [Capability::ContainerLogs, Capability::SystemInfo];
        let narrowed = narrow_to_granted(
            &request(&["container.logs", "system.info"], false),
            &granted,
        )
        .expect("system.info survives");
        assert_eq!(narrowed.context_kinds, vec!["system.info".to_string()]);
    }

    /// A request for capabilities none of which are granted yields `None` — the
    /// diagnosis must be refused before any collection is asked of the edge.
    #[test]
    fn narrow_empty_intersection_is_none() {
        // Granted an unrelated capability only.
        let granted = [Capability::NetworkPorts];
        assert!(narrow_to_granted(&request(&["system.info"], false), &granted).is_none());
        // ai.diagnose granted but no evidence capability at all.
        assert!(narrow_to_granted(&request(&[], false), &[]).is_none());
    }

    /// A screenshot survives narrowing only when explicitly granted; otherwise
    /// `include_screen` is cleared even though the request asked for it.
    #[test]
    fn narrow_gates_screenshot_on_grant() {
        // Requested screen but not granted → cleared, other granted kept.
        let granted = [Capability::SystemInfo];
        let narrowed = narrow_to_granted(&request(&["system.info"], true), &granted).expect("kept");
        assert!(!narrowed.include_screen);
        assert!(
            !narrowed
                .context_kinds
                .contains(&"screen.capture.current".to_string())
        );

        // Requested + granted → retained.
        let granted = [Capability::SystemInfo, Capability::ScreenCaptureCurrent];
        let narrowed = narrow_to_granted(&request(&["system.info"], true), &granted).expect("kept");
        assert!(narrowed.include_screen);
        assert!(
            narrowed
                .context_kinds
                .contains(&"screen.capture.current".to_string())
        );
    }

    /// `capability_name` round-trips the collectable names and rejects exec.
    #[test]
    fn capability_name_round_trips_collectable() {
        for name in [
            "system.info",
            "process.list",
            "network.ports",
            "service.status",
            "log.recent",
            "container.list",
            "container.inspect",
            "container.logs",
            "screen.capture.current",
        ] {
            let cap = capability_from_name(name).expect("known");
            assert_eq!(capability_name(cap), Some(name));
        }
        assert_eq!(capability_name(Capability::ShellExecReadonly), None);
    }

    /// Parameter-free reads build an input; container inspect/logs and exec do
    /// not.
    #[test]
    fn context_input_buildability() {
        assert!(context_input_for(Capability::SystemInfo).is_some());
        assert!(context_input_for(Capability::ScreenCaptureCurrent).is_some());
        assert!(context_input_for(Capability::ContainerInspect).is_none());
        assert!(context_input_for(Capability::ContainerLogs).is_none());
        assert!(context_input_for(Capability::ShellExecReadonly).is_none());
    }
}
