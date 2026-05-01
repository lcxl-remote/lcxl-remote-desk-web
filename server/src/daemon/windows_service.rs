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
                    if let Ok(mut guard) = stop_tx.lock()
                        && let Some(tx) = guard.take() {
                            let _ = tx.send(());
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

        let system = actix_web::rt::System::new();
        system.block_on(async {
            use crate::daemon::run_service_daemon_inner;
            use clap::Parser;
            let args = crate::model::settings::Args::parse();
            // Pass the shutdown receiver directly so the daemon can reach
            // shutdown_all() before we set the Stopped status.
            if let Err(e) = run_service_daemon_inner(args, Some(stop_rx)).await {
                error!("[WindowsService] Daemon error: {e}");
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

        let file_name = src_exe.file_name().ok_or("current exe has no file name")?;
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

        // Copy static/ alongside the binary so the daemon can serve the frontend.
        // Fail fast if the directory is missing — better to catch this at install
        // time than to discover the frontend is broken after the service starts.
        let src_static = src_exe
            .parent()
            .map(|d| d.join("static"))
            .ok_or("could not determine source directory")?;
        if !src_static.exists() {
            return Err(format!(
                "static/ directory not found at '{}'. \
                 Build the vite project and place the output in a 'static/' directory \
                 alongside the server binary before installing the service.",
                src_static.display()
            )
            .into());
        }
        let dst_static = install_path.join("static");
        if dst_static.exists() {
            std::fs::remove_dir_all(&dst_static)?;
            info!("Removed old static directory at {}", dst_static.display());
        }
        copy_dir_recursive(&src_static, &dst_static)?;
        info!("Copied static/ to {}", dst_static.display());

        let manager = ServiceManager::local_computer(
            None::<&str>,
            ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
        )?;

        // Store config in ProgramData (consistent with the log directory location)
        // so the daemon is not sensitive to its working directory.
        let config_path = service_data_dir()
            .join("conf")
            .join("config")
            .to_string_lossy()
            .into_owned();

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
                OsString::from("--config-file-path"),
                OsString::from(&config_path),
            ],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };

        let service = manager.create_service(&info, ServiceAccess::START)?;
        info!("Service '{SERVICE_NAME}' installed at '{install_dir}'");
        service.start(&[] as &[&std::ffi::OsStr])?;
        info!("Service '{SERVICE_NAME}' started");
        Ok(())
    }

    /// Remove the Windows Service registration.
    pub fn uninstall_service() -> WsResult<()> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;

        // Open service with enough permissions to query config, stop it, and delete it.
        let service = match manager.open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_CONFIG
                | ServiceAccess::QUERY_STATUS
                | ServiceAccess::STOP
                | ServiceAccess::DELETE,
        ) {
            Ok(s) => s,
            Err(e) => {
                log::warn!(
                    "Could not open service with full access: {}. Trying with DELETE only.",
                    e
                );
                manager.open_service(SERVICE_NAME, ServiceAccess::DELETE)?
            }
        };

        // cfg.executable_path is the raw SCM command line: exe_path + launch args.
        // Parse only the executable path from the first token.
        let exe_path = service.query_config().ok().and_then(|cfg| {
            let cmdline = cfg.executable_path.to_string_lossy();
            let cmdline = cmdline.trim();
            let path_str = if cmdline.starts_with('"') {
                cmdline[1..].split('"').next()?.to_string()
            } else {
                cmdline.split_whitespace().next()?.to_string()
            };
            Some(std::path::PathBuf::from(path_str))
        });

        if let Ok(status) = service.query_status()
            && status.current_state != ServiceState::Stopped {
                let _ = service.stop();
                std::thread::sleep(Duration::from_secs(1));
            }

        service.delete()?;
        info!("Service '{SERVICE_NAME}' uninstalled successfully");

        if let Some(exe_path) = exe_path {
            // Wait up to 5 seconds for the exe file lock to be released before
            // removing the whole install directory.
            for _ in 0..10 {
                if !exe_path.exists() || std::fs::remove_file(&exe_path).is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }

            if let Some(parent) = exe_path.parent() {
                if std::fs::remove_dir_all(parent).is_ok() {
                    info!("Removed install directory {}", parent.display());
                } else {
                    log::warn!(
                        "Could not fully remove install directory {}",
                        parent.display()
                    );
                }
            }
        }

        Ok(())
    }

    /// Returns the canonical config file path used by the service daemon.
    ///
    /// Both the daemon (via `--config-file-path` launch argument) and the Tauri app
    /// use this function so they always agree on the same file.
    pub fn get_service_config_path() -> Option<std::path::PathBuf> {
        Some(service_data_dir().join("conf").join("config"))
    }

    /// Returns the service data directory: `%ProgramData%\LCXL Remote Desktop`.
    /// Config and logs are stored here, consistent across service restarts and
    /// independent of the working directory Windows assigns to the service process.
    pub fn service_data_dir() -> std::path::PathBuf {
        let program_data =
            std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".to_string());
        std::path::PathBuf::from(program_data).join("LCXL Remote Desktop")
    }

    fn copy_dir_recursive(
        src: &std::path::Path,
        dst: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let dst_path = dst.join(entry.file_name());
            if ty.is_dir() {
                copy_dir_recursive(&entry.path(), &dst_path)?;
            } else {
                std::fs::copy(entry.path(), &dst_path)?;
            }
        }
        Ok(())
    }
}

/// Returns None on non-Windows (no service daemon concept).
#[cfg(not(target_os = "windows"))]
pub fn get_service_config_path() -> Option<std::path::PathBuf> {
    None
}

#[cfg(not(target_os = "windows"))]
pub fn service_data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("/var/lib/lcxl-remote-desk")
}

/// Returns the default service installation directory for this platform.
pub fn default_install_dir() -> String {
    #[cfg(target_os = "windows")]
    {
        let pf = std::env::var("PROGRAMFILES").unwrap_or_else(|_| "C:\\Program Files".to_string());
        format!("{}\\LCXL Remote Desktop", pf)
    }
    #[cfg(not(target_os = "windows"))]
    {
        "/opt/lcxl-remote-desk".to_string()
    }
}
