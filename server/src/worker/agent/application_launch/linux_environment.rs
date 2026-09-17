//! Combine the user manager environment with only the selected session's GUI context.
use desk_agent_protocol::application_launch::LaunchFailureReason;
use std::collections::BTreeMap;

pub(super) const SESSION_KEYS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "XDG_SESSION_DESKTOP",
    "XDG_SEAT",
    "XDG_VTNR",
];

pub(super) fn build(
    manager: &[String],
    session: &BTreeMap<String, String>,
    home: &str,
    user: &str,
    runtime: &str,
) -> Result<(Vec<String>, Vec<String>), LaunchFailureReason> {
    let mut environment = BTreeMap::new();
    if manager.len() > 4096 || manager.iter().map(String::len).sum::<usize>() > 2 * 1024 * 1024 {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    for entry in manager {
        let (key, value) = entry
            .split_once('=')
            .ok_or(LaunchFailureReason::SessionUnavailable)?;
        insert(&mut environment, key, value)?;
    }
    // The user manager can serve several desktops. Never use its global GUI
    // variables as a fallback for a missing value in the selected session.
    let mut unset = Vec::new();
    for key in SESSION_KEYS {
        environment.remove(*key);
        if let Some(value) = session.get(*key) {
            insert(&mut environment, key, value)?;
        } else {
            unset.push((*key).to_owned());
        }
    }
    insert(&mut environment, "HOME", home)?;
    insert(&mut environment, "USER", user)?;
    insert(&mut environment, "LOGNAME", user)?;
    insert(&mut environment, "XDG_RUNTIME_DIR", runtime)?;
    insert(
        &mut environment,
        "DBUS_SESSION_BUS_ADDRESS",
        &format!("unix:path={runtime}/bus"),
    )?;
    environment
        .entry("PATH".into())
        .or_insert_with(|| "/usr/local/bin:/usr/bin:/bin".into());
    Ok((
        environment
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect(),
        unset,
    ))
}

fn insert(
    environment: &mut BTreeMap<String, String>,
    key: &str,
    value: &str,
) -> Result<(), LaunchFailureReason> {
    if key.is_empty()
        || key.contains(['=', '\0'])
        || value.contains('\0')
        || key.len() + value.len() > 128 * 1024
    {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    environment.insert(key.into(), value.into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_only_variables_do_not_leak_and_other_desktops_are_not_fallbacks() {
        let manager = vec![
            "USER_SETTING=preserved".into(),
            "DISPLAY=:other".into(),
            "XAUTHORITY=/other/session".into(),
        ];
        let session = BTreeMap::from([
            ("DISPLAY".into(), ":selected".into()),
            ("HOST_ONLY_TEST_SECRET".into(), "never-copy".into()),
        ]);
        let (environment, unset) =
            build(&manager, &session, "/home/user", "user", "/run/user/1000").unwrap();
        assert!(environment.contains(&"USER_SETTING=preserved".into()));
        assert!(environment.contains(&"DISPLAY=:selected".into()));
        assert!(environment.contains(&"HOME=/home/user".into()));
        assert!(unset.contains(&"XAUTHORITY".into()));
        assert!(
            !environment
                .iter()
                .any(|entry| entry.contains("never-copy") || entry.contains("/other/session"))
        );
    }
    #[test]
    fn malformed_manager_environment_fails_before_dispatch() {
        assert!(
            build(
                &["missing-separator".into()],
                &BTreeMap::new(),
                "/home/u",
                "u",
                "/run/user/1"
            )
            .is_err()
        );
        assert!(
            build(
                &["KEY=bad\0value".into()],
                &BTreeMap::new(),
                "/home/u",
                "u",
                "/run/user/1"
            )
            .is_err()
        );
    }
}
