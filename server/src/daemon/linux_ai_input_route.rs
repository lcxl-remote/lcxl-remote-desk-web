//! Derive the native destination from daemon-owned worker/session identity.
use super::worker_manager::{WorkerIncarnation, WorkerManager};
use crate::host_control::HostControlHub;
use desk_ipc_protocol::message::{DesktopTarget, WorkerKey};
use std::path::Path;

pub(super) fn publish(
    manager: &WorkerManager,
    hub: &HostControlHub,
    key: Option<&WorkerKey>,
    incarnation: WorkerIncarnation,
    path: Option<String>,
) {
    let Some(key) = key.filter(|key| key.desktop == DesktopTarget::LinuxSession) else {
        return;
    };
    let Some(registration) = manager.session_shell_registration(&key.session) else {
        return;
    };
    let Some(registry) = manager.session_shell_registry() else {
        return;
    };
    let Some(gate) = manager.native_input_incarnation(key, incarnation) else {
        return;
    };
    if let Some(path) = &path {
        let runtime = registration
            .environment
            .iter()
            .find(|(name, _)| name == "XDG_RUNTIME_DIR")
            .map(|(_, value)| Path::new(value));
        if !runtime.is_some_and(|runtime| valid_endpoint(runtime, Path::new(path))) {
            return;
        }
    }
    hub.publish_linux_session_input(registry, registration, path, incarnation.get(), gate);
}
fn valid_endpoint(runtime: &Path, path: &Path) -> bool {
    if !runtime.is_absolute() || path.parent() != Some(runtime.join("lcxl-ai-input").as_path()) {
        return false;
    }
    let Some(name) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".sock"))
    else {
        return false;
    };
    uuid::Uuid::parse_str(name).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_a_uuid_socket_below_the_registered_runtime_is_accepted() {
        let runtime = Path::new("/run/user/1000");
        let id = uuid::Uuid::new_v4();
        assert!(valid_endpoint(
            runtime,
            &runtime.join(format!("lcxl-ai-input/{id}.sock"))
        ));
        for path in [
            format!("/run/user/1001/lcxl-ai-input/{id}.sock"),
            format!("/tmp/{id}.sock"),
            "/run/user/1000/lcxl-ai-input/../other.sock".into(),
            "/run/user/1000/lcxl-ai-input/not-a-worker.sock".into(),
        ] {
            assert!(!valid_endpoint(runtime, Path::new(&path)));
        }
    }
}
