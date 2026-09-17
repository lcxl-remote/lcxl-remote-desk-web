//! Validate the selected logind session and the user manager's process credentials.
use crate::host_control::session_shell::{UnixProcessIdentity, read_process_identity};
use desk_agent_protocol::application_launch::LaunchFailureReason;
use std::time::Duration;
use zbus::zvariant::OwnedObjectPath;

pub(super) fn current_identity() -> Result<String, LaunchFailureReason> {
    let identity = read_process_identity(std::process::id())
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    if identity.uid == 0 {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    Ok(format!(
        "{}:{}:{:?}",
        identity.uid, identity.gid, identity.supplementary_groups
    ))
}

pub(super) async fn verify(
    connection: &zbus::Connection,
    session_id: &str,
) -> Result<(), LaunchFailureReason> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let current = read_process_identity(std::process::id())
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let bus = zbus::Proxy::new(
            connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let pid: u32 = bus
            .call("GetConnectionUnixProcessID", &("org.freedesktop.systemd1",))
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let manager =
            read_process_identity(pid).map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        same_credentials(&current, &manager)?;
        let (_, requested_session) = session_id
            .split_once(':')
            .ok_or(LaunchFailureReason::SessionUnavailable)?;
        let system = zbus::Connection::system()
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let login = zbus::Proxy::new(
            &system,
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
        )
        .await
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let path: OwnedObjectPath = login
            .call("GetSession", &(requested_session,))
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let session = zbus::Proxy::new(
            &system,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )
        .await
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let user: (u32, OwnedObjectPath) = session
            .get_property("User")
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let id: String = session
            .get_property("Id")
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let kind: String = session
            .get_property("Type")
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let class: String = session
            .get_property("Class")
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let state: String = session
            .get_property("State")
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let locked: bool = session
            .get_property("LockedHint")
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        if user.0 != current.uid
            || id != requested_session
            || !matches!(kind.as_str(), "x11" | "wayland")
            || class != "user"
            || !matches!(state.as_str(), "online" | "active")
            || locked
        {
            return Err(LaunchFailureReason::SessionUnavailable);
        }
        let display: String = session
            .get_property("Display")
            .await
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        if !display.is_empty() && std::env::var("DISPLAY").ok().as_deref() != Some(display.as_str())
        {
            return Err(LaunchFailureReason::SessionUnavailable);
        }
        Ok(())
    })
    .await
    .map_err(|_| LaunchFailureReason::SessionUnavailable)?
}

fn same_credentials(
    current: &UnixProcessIdentity,
    manager: &UnixProcessIdentity,
) -> Result<(), LaunchFailureReason> {
    if current.uid == 0
        || current.uid != manager.uid
        || current.gid != manager.gid
        || current.supplementary_groups != manager.supplementary_groups
    {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manager_cannot_change_the_approved_user_or_group_set() {
        let current = UnixProcessIdentity {
            uid: 1000,
            gid: 1000,
            supplementary_groups: vec![10, 1000],
            start_ticks: 1,
        };
        let mut manager = current.clone();
        manager.start_ticks = 99;
        assert!(same_credentials(&current, &manager).is_ok());
        manager.supplementary_groups.push(27);
        assert!(same_credentials(&current, &manager).is_err());
        manager = current.clone();
        manager.uid = 0;
        assert!(same_credentials(&current, &manager).is_err());
    }
}
