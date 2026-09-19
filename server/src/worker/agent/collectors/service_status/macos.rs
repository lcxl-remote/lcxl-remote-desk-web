//! launchd jobs and explicitly attributed on-disk launch policies.
use super::*;

pub(super) fn enumerate(_params: &ServiceStatusParams, deadline: Instant) -> Enumeration {
    // The command inherits the worker's bootstrap namespace.
    let uid = unsafe { libc::geteuid() };
    let mut result = Enumeration {
        scope: format!("bootstrap_user:{uid}"),
        ..Default::default()
    };
    match super::command::run("/bin/launchctl", &["list"], deadline) {
        Ok(text) => {
            for line in text.lines().skip(1) {
                let parts: Vec<_> = line.splitn(3, '\t').collect();
                if parts.len() != 3 {
                    continue;
                }
                let pid = parts[0].trim();
                if pid != "-" && pid.parse::<u32>().is_err() {
                    continue;
                }
                result.services.push(ServiceEntry {
                    name: parts[2].trim().into(),
                    state: if pid == "-" { "stopped" } else { "running" }.into(),
                    scope: result.scope.clone(),
                    ..Default::default()
                });
            }
        }
        Err(e) => result.errors.push(e),
    }
    result
}
pub(super) fn enrich(s: &mut ServiceEntry, deadline: Instant) {
    match policy(s, deadline) {
        Ok((domain, tags, summary)) => {
            s.policy_domain = Some(domain);
            s.policy_summary = Some(summary);
            s.launch_policies = Some(tags);
            s.policy_source = Some("disk".into());
        }
        Err(e) => s.metadata_error = Some(e),
    }
}
fn policy(
    s: &ServiceEntry,
    deadline: Instant,
) -> Result<(String, Vec<String>, String), NativeDiagnostic> {
    let fail = |message: &str| {
        diagnostic(
            DiagnosticStage::ServiceConfiguration,
            "launchd policy",
            message,
        )
    };
    let uid = unsafe { libc::geteuid() };
    let domains = if uid == 0 {
        vec!["system".into()]
    } else {
        vec![format!("gui/{uid}"), format!("user/{uid}")]
    };
    let mut sources = Vec::new();
    for domain in domains {
        let target = format!("{domain}/{}", s.name);
        if let Ok(text) = super::command::run("/bin/launchctl", &["print", &target], deadline) {
            // A service's top-level path precedes any nested dictionary. Never
            // use arbitrary nested `path` keys or guess a plist from its label.
            let path = text
                .lines()
                .skip(1)
                .take_while(|line| !line.contains('{'))
                .find_map(|line| line.trim().strip_prefix("path = "));
            if let Some(path) = path {
                sources.push((domain, path.trim().to_string()));
            }
        }
    }
    if sources.len() != 1 {
        return Err(fail(
            "Runtime policy is unavailable and a unique domain/configuration source could not be established",
        ));
    }
    let (domain, path) = sources.remove(0);
    let file = std::fs::File::open(&path).map_err(|e| {
        NativeDiagnostic::from_io(
            DiagnosticStage::ServiceConfiguration,
            "read launchd plist",
            &e,
        )
    })?;
    let metadata = file.metadata().map_err(|e| {
        NativeDiagnostic::from_io(
            DiagnosticStage::ServiceConfiguration,
            "stat launchd plist",
            &e,
        )
    })?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(fail("Launch configuration is not a bounded regular file"));
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, 1024 * 1024 + 1), &mut bytes)
        .map_err(|e| {
            NativeDiagnostic::from_io(
                DiagnosticStage::ServiceConfiguration,
                "read launchd plist",
                &e,
            )
        })?;
    if bytes.len() > 1024 * 1024 {
        return Err(fail("Launch configuration size changed beyond budget"));
    }
    let value = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .map_err(|_| fail("Launch configuration could not be parsed"))?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| fail("Launch configuration is not a dictionary"))?;
    if dict.get("Label").and_then(plist::Value::as_string) != Some(s.name.as_str()) {
        return Err(fail(
            "Launch configuration label does not match the loaded job",
        ));
    }
    let conditional = dict
        .get("KeepAlive")
        .and_then(plist::Value::as_dictionary)
        .map(|conditions| {
            conditions
                .iter()
                .take(8)
                .map(|(key, value)| {
                    let key: String = key.chars().take(64).collect();
                    if let Some(value) = value.as_boolean() {
                        format!("{key}={value}")
                    } else {
                        format!("{key}=conditional")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        });
    let summary = conditional.map(|conditions| format!("On-disk KeepAlive conditions: {conditions}; not verified as effective runtime policy"))
        .unwrap_or_else(|| "On-disk configuration; not verified as effective runtime policy".into());
    Ok((domain, tags(dict), summary))
}
fn tags(dict: &plist::Dictionary) -> Vec<String> {
    let mut result = Vec::new();
    if dict.get("RunAtLoad").and_then(plist::Value::as_boolean) == Some(true) {
        result.push("run_at_load".into());
    }
    if dict.get("KeepAlive").is_some_and(|v| {
        v.as_boolean() == Some(true) || v.as_dictionary().is_some_and(|d| !d.is_empty())
    }) {
        result.push("keep_alive".into());
    }
    if dict
        .get("StartInterval")
        .and_then(plist::Value::as_unsigned_integer)
        .is_some_and(|n| n > 0)
        || dict.get("StartCalendarInterval").is_some_and(|v| {
            v.as_dictionary().is_some() || v.as_array().is_some_and(|a| !a.is_empty())
        })
    {
        result.push("scheduled".into());
    }
    for (key, tag) in [("Sockets", "socket"), ("MachServices", "mach_service")] {
        if dict
            .get(key)
            .and_then(plist::Value::as_dictionary)
            .is_some_and(|d| !d.is_empty())
        {
            result.push(tag.into());
        }
    }
    if ["WatchPaths", "QueueDirectories"].iter().any(|key| {
        dict.get(*key)
            .and_then(plist::Value::as_array)
            .is_some_and(|a| !a.is_empty())
    }) {
        result.push("path_watch".into());
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policies_are_multiple_and_conditional_keepalive_is_not_lost() {
        let mut d = plist::Dictionary::new();
        d.insert("RunAtLoad".into(), true.into());
        let mut condition = plist::Dictionary::new();
        condition.insert("SuccessfulExit".into(), false.into());
        d.insert("KeepAlive".into(), condition.into());
        assert_eq!(tags(&d), vec!["run_at_load", "keep_alive"]);
        assert!(tags(&plist::Dictionary::new()).is_empty());
    }
}
