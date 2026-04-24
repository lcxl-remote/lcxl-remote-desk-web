#[cfg(target_os = "windows")]
pub use windows_impl::*;

#[cfg(target_os = "windows")]
mod windows_impl {
    use log::{error, info};
    use std::ffi::OsString;
    use std::time::Duration;
    use tokio::sync::oneshot;
    use windows_service::{
        Result as WsResult, define_windows_service,
        service::{
            ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl,
            ServiceExitCode, ServiceInfo, ServiceStartType, ServiceState, ServiceStatus,
            ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    pub const SERVICE_NAME: &str = "LcxlDeskService";
    pub const SERVICE_DISPLAY_NAME: &str = "LCXL Remote Desktop Service";

    define_windows_service!(ffi_service_main, service_main);

    fn service_main(_args: Vec<OsString>) {
        if let Err(e) = run_service() {
            error!("[WindowsService] Fatal error: {e}");
        }
    }

    fn run_service() -> WsResult<()> {
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop_tx = std::sync::Mutex::new(Some(stop_tx));

        let status_handle =
            service_control_handler::register(SERVICE_NAME, move |ctrl| match ctrl {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    if let Ok(mut guard) = stop_tx.lock() {
                        if let Some(tx) = guard.take() {
                            let _ = tx.send(());
                        }
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            })?;

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        info!("[WindowsService] Service Running");

        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async {
            use crate::daemon::run_service_daemon_inner;
            use clap::Parser;
            let args = crate::model::settings::Args::parse();
            tokio::select! {
                res = run_service_daemon_inner(args) => {
                    if let Err(e) = res {
                        error!("[WindowsService] Daemon error: {e}");
                    }
                }
                _ = async { stop_rx.await.ok(); } => {
                    info!("[WindowsService] Stop signal received");
                }
            }
        });

        status_handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        })?;

        info!("[WindowsService] Service Stopped");
        Ok(())
    }

    /// Try to start the Windows Service dispatcher.
    ///
    /// Returns `Ok(true)` if we ran as a Windows Service.
    /// Returns `Ok(false)` if not running under SCM (interactive).
    pub fn try_run_as_service() -> Result<bool, Box<dyn std::error::Error>> {
        match service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            Ok(()) => Ok(true),
            Err(windows_service::Error::Winapi(e)) if e.raw_os_error() == Some(1063) => Ok(false),
            Err(e) => Err(Box::new(e)),
        }
    }

    /// Copy the server binary to `install_dir` (skipped when source and
    /// destination are in the same directory) and register it as a Windows
    /// Service in SCM.
    pub fn install_service(
        install_dir: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let src_exe = std::env::current_exe()?;

        let install_path = std::path::Path::new(install_dir);
        std::fs::create_dir_all(install_path)?;

        let file_name = src_exe
            .file_name()
            .ok_or("current exe has no file name")?;
        let dst_exe = install_path.join(file_name);

        // Skip copy when source is already inside the install directory.
        let same_dir = src_exe
            .parent()
            .and_then(|p| p.canonicalize().ok())
            .zip(install_path.canonicalize().ok())
            .map(|(src_dir, dst_dir)| src_dir == dst_dir)
            .unwrap_or(false);

        if !same_dir {
            std::fs::copy(&src_exe, &dst_exe)?;
            info!("Copied binary to {}", dst_exe.display());
        } else {
            info!("Binary already in install directory, skipping copy");
        }

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;

        let info = ServiceInfo {
            name: OsString::from(SERVICE_NAME),
            display_name: OsString::from(SERVICE_DISPLAY_NAME),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: dst_exe,
            launch_arguments: vec![
                OsString::from("--startup-mode"),
                OsString::from("service-daemon"),
            ],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };

        manager.create_service(&info, ServiceAccess::empty())?;
        info!("Service '{SERVICE_NAME}' installed at '{install_dir}'");
        Ok(())
    }

    /// Remove the Windows Service registration.
    pub fn uninstall_service() -> WsResult<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(SERVICE_NAME, ServiceAccess::DELETE)?;
        service.delete()?;
        info!("Service '{SERVICE_NAME}' uninstalled successfully");
        Ok(())
    }
}

/// Returns the default service installation directory for this platform.
pub fn default_install_dir() -> String {
    #[cfg(target_os = "windows")]
    {
        let pf = std::env::var("PROGRAMFILES")
            .unwrap_or_else(|_| "C:\\Program Files".to_string());
        format!("{}\\LCXL Remote Desktop", pf)
    }
    #[cfg(not(target_os = "windows"))]
    {
        "/opt/lcxl-remote-desk".to_string()
    }
}
