//! Native service-operation results survive an embedded server/daemon disconnect.
use lcxl_remote_desk_server::{
    ServiceOp,
    host_control::{
        ServiceOpKind,
        service_operations::{
            ServiceOperationError, ServiceOperationState, ServiceOperationStatus,
        },
    },
};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::{path::PathBuf, sync::mpsc::Receiver};
use tauri::Manager;

pub(crate) struct ServiceJob {
    pub operation_id: Option<String>,
    pub op: ServiceOp,
    pub result_tx: Option<tokio::sync::mpsc::UnboundedSender<ServiceOperationStatus>>,
}

pub(crate) type Outcome = (
    ServiceOperationState,
    Option<ServiceOperationError>,
    Option<i32>,
);

pub(crate) fn start(handle: tauri::AppHandle, rx: Receiver<ServiceJob>, config: Option<PathBuf>) {
    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            let op = match &job.op {
                ServiceOp::Install { .. } => ServiceOpKind::Install,
                ServiceOp::Uninstall => ServiceOpKind::Uninstall,
            };
            let (state, error, exit_code) = crate::handle_service_op(job.op, config.as_deref());
            let Some(operation_id) = job.operation_id else {
                continue;
            };
            let status = ServiceOperationStatus {
                operation_id,
                op,
                state,
                error,
                exit_code,
            };
            // The sender belongs to the original WS connection. A reconnect
            // must never submit a completion under a different session identity.
            if let Some(tx) = job.result_tx {
                let _ = tx.send(status.clone());
            }
            if let Some(window) = handle.get_webview_window(crate::MAIN_WINDOW_LABEL)
                && let Ok(json) = serde_json::to_string(&status)
            {
                let _ = window.eval(&format!(
                    "window.dispatchEvent(new CustomEvent('lrd-service-operation', {{detail:{json}}}));"
                ));
            }
        }
    });
}

#[cfg(target_os = "linux")]
pub(crate) fn execute_linux(sidecar: &Path, op: &ServiceOp, config: Option<&Path>) -> Outcome {
    let mut command = std::process::Command::new("pkexec");
    // A GUI action must use the desktop's authentication agent, never a hidden
    // terminal password prompt. Passwords do not pass through this process.
    command.arg("--disable-internal-agent").arg(sidecar);
    match op {
        ServiceOp::Install {
            install_path,
            install_idd_driver,
        } => {
            if *install_idd_driver {
                return (
                    ServiceOperationState::Failed,
                    Some(ServiceOperationError::Unsupported),
                    None,
                );
            }
            command
                .arg("--install-service")
                .arg("--install-path")
                .arg(install_path);
            if let Some(path) = config {
                command.arg("--config-file-path").arg(path);
            }
        }
        ServiceOp::Uninstall => {
            command.arg("--uninstall-service");
        }
    }
    run(&mut command)
}

#[cfg(target_os = "linux")]
fn run(command: &mut std::process::Command) -> Outcome {
    use ServiceOperationError as Error;
    use ServiceOperationState as State;
    let outcome = match command.status() {
        Ok(status) => match status.code() {
            Some(0) => (State::Succeeded, None, Some(0)),
            Some(126) => (State::Cancelled, None, Some(126)),
            Some(127) => (State::Failed, Some(Error::AuthorizationNotGranted), Some(127)),
            Some(code) if code == lcxl_remote_desk_server::daemon::linux_service::SERVICE_OPERATION_BUSY_EXIT_CODE =>
                (State::Failed, Some(Error::Busy), Some(code)),
            Some(code) => (State::Failed, Some(Error::InstallerFailed), Some(code)),
            None => (State::Unknown, Some(Error::ConnectionLost), None),
        },
        Err(error) => (State::Failed, Some(if error.kind() == std::io::ErrorKind::NotFound { Error::MissingPkexec } else { Error::LaunchFailed }), None),
    };
    log::info!(
        "Service operation result: state={:?}, error={:?}, exit_code={:?}",
        outcome.0,
        outcome.1,
        outcome.2
    );
    outcome
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn process_outcomes_are_not_confused_with_launch_acceptance() {
        for (code, state, error) in [
            (0, ServiceOperationState::Succeeded, None),
            (126, ServiceOperationState::Cancelled, None),
            (
                127,
                ServiceOperationState::Failed,
                Some(ServiceOperationError::AuthorizationNotGranted),
            ),
            (
                1,
                ServiceOperationState::Failed,
                Some(ServiceOperationError::InstallerFailed),
            ),
            (
                75,
                ServiceOperationState::Failed,
                Some(ServiceOperationError::Busy),
            ),
        ] {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", &format!("exit {code}")]);
            assert_eq!(run(&mut command), (state, error, Some(code)));
        }
        let mut missing = std::process::Command::new("/nonexistent/lrd-test-pkexec");
        assert_eq!(
            run(&mut missing).1,
            Some(ServiceOperationError::MissingPkexec)
        );
        let mut killed = std::process::Command::new("/bin/sh");
        killed.args(["-c", "kill -TERM $$"]);
        assert_eq!(run(&mut killed).0, ServiceOperationState::Unknown);
    }
}
