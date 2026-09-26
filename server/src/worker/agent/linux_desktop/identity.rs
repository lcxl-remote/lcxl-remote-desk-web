//! logind selects the graphical session; socket and bus peers bind the worker.
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::net::UnixStream;

use zbus::{Connection, Proxy, zvariant::OwnedObjectPath};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DesktopIdentity {
    pub uid: u32,
    pub session_id: String,
    pub session_path: OwnedObjectPath,
    pub runtime_path: PathBuf,
    pub socket_path: PathBuf,
    pub socket_device: u64,
    pub socket_inode: u64,
    pub compositor_pid: u32,
    pub compositor_start: u64,
    pub bus_id: String,
    pub shell_owner: String,
    pub logind_owner: String,
}

impl DesktopIdentity {
    pub(crate) fn binding(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        for value in [
            self.uid.to_string(),
            self.session_id.clone(),
            self.session_path.to_string(),
            self.runtime_path.to_string_lossy().into_owned(),
            self.socket_path.to_string_lossy().into_owned(),
            self.socket_device.to_string(),
            self.socket_inode.to_string(),
            self.compositor_pid.to_string(),
            self.compositor_start.to_string(),
            self.bus_id.clone(),
            self.shell_owner.clone(),
            self.logind_owner.clone(),
        ] {
            hash.update((value.len() as u64).to_be_bytes());
            hash.update(value.as_bytes());
        }
        format!("linux-wayland-{:x}", hash.finalize())
    }
}

fn reason(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// No authorization, capture, restore or desktop setting changes occur here.
pub(crate) async fn resolve() -> Result<DesktopIdentity> {
    tokio::time::timeout(Duration::from_secs(3), resolve_inner())
        .await
        .map_err(|_| "Desktop identity lookup timed out".to_string())?
}

async fn resolve_inner() -> Result<DesktopIdentity> {
    // A system daemon's environment cannot authorize a root graphical worker.
    let uid = unsafe { libc::geteuid() };
    if uid == 0 || uid != unsafe { libc::getuid() } {
        return Err("Desktop requires an unelevated user worker".into());
    }
    let system = Connection::system().await.map_err(reason)?;
    let bus = Connection::session().await.map_err(reason)?;
    let manager = Proxy::new(
        &system,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    .map_err(reason)?;
    let user_path: OwnedObjectPath = manager.call("GetUser", &(uid,)).await.map_err(reason)?;
    let user = Proxy::new(
        &system,
        "org.freedesktop.login1",
        user_path,
        "org.freedesktop.login1.User",
    )
    .await
    .map_err(reason)?;
    let display: (String, OwnedObjectPath) = user.get_property("Display").await.map_err(reason)?;
    if display.0.is_empty() || display.1.as_str() == "/" {
        return Err("No primary graphical session".into());
    }
    let runtime: String = user.get_property("RuntimePath").await.map_err(reason)?;
    let session = Proxy::new(
        &system,
        "org.freedesktop.login1",
        display.1.clone(),
        "org.freedesktop.login1.Session",
    )
    .await
    .map_err(reason)?;
    validate_session(&session, uid).await?;

    // Display is authoritative, but a second compositor needs an explicit
    // trusted association that this worker does not possess.
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        manager.call("ListSessions", &()).await.map_err(reason)?;
    for (id, owner, _, _, path) in sessions {
        if owner != uid || id == display.0 {
            continue;
        }
        let other = Proxy::new(
            &system,
            "org.freedesktop.login1",
            path,
            "org.freedesktop.login1.Session",
        )
        .await
        .map_err(reason)?;
        let kind: String = other.get_property("Type").await.map_err(reason)?;
        if kind == "wayland" || kind == "x11" {
            return Err("Multiple graphical sessions need an explicit worker association".into());
        }
    }
    let runtime_path = PathBuf::from(runtime);
    let environment_runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    if environment_runtime.as_ref() != Some(&runtime_path) {
        return Err("Worker runtime directory does not match logind".into());
    }
    let display_name = std::env::var_os("WAYLAND_DISPLAY").ok_or("Wayland display is missing")?;
    let name = Path::new(&display_name);
    if !single_component(name) {
        return Err("Wayland display must name a socket in the user runtime directory".into());
    }
    let socket_path = runtime_path.join(name);
    let (socket_device, socket_inode, compositor_pid, compositor_start) =
        socket_identity(&runtime_path, &socket_path, uid).await?;
    let dbus = Proxy::new(
        &bus,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .map_err(reason)?;
    let bus_id: String = dbus.call("GetId", &()).await.map_err(reason)?;
    let shell_owner: String = dbus
        .call("GetNameOwner", &("org.gnome.Shell",))
        .await
        .map_err(reason)?;
    let shell_pid: u32 = dbus
        .call("GetConnectionUnixProcessID", &(&shell_owner,))
        .await
        .map_err(reason)?;
    let shell_uid: u32 = dbus
        .call("GetConnectionUnixUser", &(&shell_owner,))
        .await
        .map_err(reason)?;
    if shell_uid != uid || shell_pid != compositor_pid {
        return Err("Session bus GNOME Shell differs from the Wayland peer".into());
    }
    let executable = std::fs::read_link(format!("/proc/{compositor_pid}/exe")).map_err(reason)?;
    if executable.file_name() != Some(std::ffi::OsStr::new("gnome-shell")) {
        return Err("Wayland peer is not GNOME Shell".into());
    }
    let screen_saver = Proxy::new(
        &bus,
        "org.gnome.ScreenSaver",
        "/org/gnome/ScreenSaver",
        "org.gnome.ScreenSaver",
    )
    .await
    .map_err(reason)?;
    let locked: bool = screen_saver.call("GetActive", &()).await.map_err(reason)?;
    if locked {
        return Err("GNOME session is locked".into());
    }
    let system_bus = Proxy::new(
        &system,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    .map_err(reason)?;
    let logind_owner = system_bus
        .call("GetNameOwner", &("org.freedesktop.login1",))
        .await
        .map_err(reason)?;
    // Recheck after the cross-service lookup to reject a changed binding.
    validate_session(&session, uid).await?;
    let current: (String, OwnedObjectPath) = user.get_property("Display").await.map_err(reason)?;
    if current != display
        || socket_identity(&runtime_path, &socket_path, uid).await?
            != (
                socket_device,
                socket_inode,
                compositor_pid,
                compositor_start,
            )
    {
        return Err("Desktop identity changed during lookup".into());
    }
    Ok(DesktopIdentity {
        uid,
        session_id: display.0,
        session_path: display.1,
        runtime_path,
        socket_path,
        socket_device,
        socket_inode,
        compositor_pid,
        compositor_start,
        bus_id,
        shell_owner,
        logind_owner,
    })
}

async fn validate_session(session: &Proxy<'_>, uid: u32) -> Result<()> {
    let user: (u32, OwnedObjectPath) = session.get_property("User").await.map_err(reason)?;
    let kind: String = session.get_property("Type").await.map_err(reason)?;
    let class: String = session.get_property("Class").await.map_err(reason)?;
    let remote: bool = session.get_property("Remote").await.map_err(reason)?;
    let active: bool = session.get_property("Active").await.map_err(reason)?;
    let locked: bool = session.get_property("LockedHint").await.map_err(reason)?;
    let state: String = session.get_property("State").await.map_err(reason)?;
    if user.0 != uid
        || kind != "wayland"
        || class != "user"
        || remote
        || !active
        || locked
        || state != "active"
    {
        return Err("Primary session is not an active unlocked local Wayland user session".into());
    }
    Ok(())
}

fn single_component(path: &Path) -> bool {
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

async fn socket_identity(runtime: &Path, socket: &Path, uid: u32) -> Result<(u64, u64, u32, u64)> {
    let directory = std::fs::symlink_metadata(runtime).map_err(reason)?;
    if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
        return Err("Runtime directory is not private and owned by the worker".into());
    }
    let metadata = std::fs::symlink_metadata(socket).map_err(reason)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err("Wayland socket is not owned by the worker".into());
    }
    let stream = UnixStream::connect(socket).await.map_err(reason)?;
    let credentials = stream.peer_cred().map_err(reason)?;
    let pid = credentials
        .pid()
        .filter(|pid| *pid > 0)
        .ok_or("Wayland peer pid is missing")? as u32;
    if credentials.uid() != uid {
        return Err("Wayland peer uid mismatch".into());
    }
    let after = std::fs::symlink_metadata(socket).map_err(reason)?;
    if (after.dev(), after.ino()) != (metadata.dev(), metadata.ino()) {
        return Err("Wayland socket changed".into());
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(reason)?;
    let start = process_start(&stat)?;
    Ok((metadata.dev(), metadata.ino(), pid, start))
}

fn process_start(stat: &str) -> Result<u64> {
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "Invalid compositor process identity".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "reads live GNOME logind, session bus and Wayland peer identity; use an external timeout"]
    async fn live_gnome_identity_remains_bound_across_independent_resolutions() {
        let first = resolve().await.expect("trusted GNOME identity");
        let second = resolve().await.expect("trusted GNOME identity recheck");
        assert_eq!(first, second);
        assert_eq!(first.uid, unsafe { libc::geteuid() });
        assert_ne!(first.uid, 0);
        assert!(!first.session_id.is_empty());
        assert!(first.compositor_pid > 0);
        assert!(first.compositor_start > 0);
        assert_eq!(first.binding(), second.binding());
    }

    #[test]
    fn display_must_stay_inside_runtime_directory() {
        assert!(single_component(Path::new("wayland-0")));
        for invalid in [
            "",
            "/tmp/wayland-0",
            "../wayland-0",
            "nested/wayland-0",
            ".",
        ] {
            assert!(!single_component(Path::new(invalid)));
        }
    }
    #[test]
    fn process_name_parentheses_do_not_shift_start_time() {
        let fields = (0..20)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            process_start(&format!("12 (name ) with spaces) {fields}")),
            Ok(19)
        );
        assert!(process_start("12 (truncated)").is_err());
    }
}
