//! Read installed and loaded systemd units without loading inactive units.
use super::*;
use std::collections::BTreeMap;

pub(super) fn enumerate(params: &ServiceStatusParams, deadline: Instant) -> Enumeration {
    let scope = params.scope.as_deref().unwrap_or("system");
    let user_scope = format!("user:{}", unsafe { libc::geteuid() });
    let resolved_scope = if scope == "user" {
        user_scope.clone()
    } else if scope == "all" {
        format!("system+{user_scope}")
    } else {
        scope.into()
    };
    let mut result = Enumeration {
        scope: resolved_scope,
        ..Default::default()
    };
    for manager in if scope == "all" {
        vec!["system", "user"]
    } else {
        vec![scope]
    } {
        if manager == "user" && unsafe { libc::geteuid() } == 0 {
            result.errors.push(diagnostic(
                DiagnosticStage::ServiceEnumeration,
                "systemctl --user",
                "The current non-root desktop user is unavailable; refusing to query the root user manager",
            ));
            continue;
        }
        let mut units = BTreeMap::new();
        let manager_arg = if manager == "user" {
            "--user"
        } else {
            "--system"
        };
        for operation in ["list-unit-files", "list-units"] {
            match super::command::run(
                "systemctl",
                &[
                    manager_arg,
                    operation,
                    "--type=service",
                    "--all",
                    "--no-pager",
                    "--no-legend",
                    "--output=json",
                ],
                deadline,
            ) {
                Ok(text) => match merge_json(
                    &mut units,
                    &text,
                    operation == "list-unit-files",
                    if manager == "user" {
                        &user_scope
                    } else {
                        manager
                    },
                ) {
                    Ok(()) => {}
                    Err(e) => result.errors.push(diagnostic(
                        DiagnosticStage::ServiceEnumeration,
                        operation,
                        &e,
                    )),
                },
                Err(error) => result.errors.push(error),
            }
        }
        result.services.extend(units.into_values());
    }
    result
}
fn merge_json(
    units: &mut BTreeMap<String, ServiceEntry>,
    text: &str,
    installed: bool,
    scope: &str,
) -> Result<(), String> {
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(text).map_err(|e| format!("Invalid systemd JSON: {e}"))?;
    for row in rows {
        let name = row
            .get(if installed { "unit_file" } else { "unit" })
            .and_then(|v| v.as_str())
            .ok_or("systemd entry missing unit name")?;
        let name = name.rsplit('/').next().unwrap_or(name);
        if !name.ends_with(".service") {
            continue;
        }
        let entry = units.entry(name.into()).or_insert_with(|| ServiceEntry {
            name: name.into(),
            scope: scope.into(),
            state: "not_loaded".into(),
            ..Default::default()
        });
        if installed {
            entry.unit_file_state = row
                .get("state")
                .and_then(|v| v.as_str())
                .map(str::to_string);
        } else {
            entry.display_name = row
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let active = row
                .get("active")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let sub = row.get("sub").and_then(|v| v.as_str()).unwrap_or("");
            entry.state = match (active, sub) {
                ("active", "running") => "running",
                ("inactive", _) => "stopped",
                _ => active,
            }
            .into();
        }
    }
    Ok(())
}
pub(super) fn enrich(entry: &mut ServiceEntry, _deadline: Instant) {
    if entry.unit_file_state.is_none() {
        entry.metadata_error = Some(diagnostic(
            DiagnosticStage::ServiceConfiguration,
            "systemd unit file state",
            "No installed unit file state was reported; may be a runtime-only service",
        ));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merges_installed_and_loaded_without_inventing_running_state() {
        let mut units = BTreeMap::new();
        merge_json(&mut units, r#"[{"unit_file":"disabled.service","state":"disabled"},{"unit_file":"running.service","state":"enabled"}]"#, true, "system").unwrap();
        merge_json(&mut units, r#"[{"unit":"running.service","active":"active","sub":"running","description":"Daemon"},{"unit":"runtime.service","active":"inactive","sub":"dead"}]"#, false, "system").unwrap();
        assert_eq!(units.len(), 3);
        assert_eq!(units["disabled.service"].state, "not_loaded");
        assert_eq!(units["running.service"].state, "running");
        assert_eq!(
            units["running.service"].unit_file_state.as_deref(),
            Some("enabled")
        );
        assert!(units["runtime.service"].unit_file_state.is_none());
    }
}
