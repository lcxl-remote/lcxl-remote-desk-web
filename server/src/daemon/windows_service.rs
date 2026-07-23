#[cfg(target_os = "windows")]
pub use windows_impl::*;

#[cfg(target_os = "windows")]
mod windows_impl {
    use desk_virtual_display_driver_ops::{DriverInstallerOps, RealInstaller};
    use log::{error, info};
    use std::ffi::OsString;
    use std::path::Path;
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
                        && let Some(tx) = guard.take()
                    {
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

    /// Copy the server binary + static/ + drivers/ to `install_dir`,
    /// optionally stage the IDD driver, and register the result as a
    /// Windows Service in SCM. The order is fixed so the daemon never
    /// boots before its IDD driver is available: copy → install
    /// driver → create SCM entry → start service.
    pub fn install_service(
        install_dir: &str,
        install_idd_driver: bool,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        install_service_with(install_dir, install_idd_driver, &RealInstaller)
    }

    /// Test seam used by [`install_service`]. Allows the file-copy +
    /// driver-staging steps to be unit-tested with a mock installer.
    /// SCM registration (the steps that need a real Windows host) still
    /// runs in the production path.
    pub fn install_service_with(
        install_dir: &str,
        install_idd_driver: bool,
        installer: &dyn DriverInstallerOps,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let src_exe = std::env::current_exe()?;
        let src_dir = src_exe
            .parent()
            .ok_or("could not determine source directory")?
            .to_path_buf();

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
        let src_static = src_dir.join("static");
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

        // Copy drivers/ + (optionally) stage the IDD driver BEFORE the
        // SCM entry is created — the daemon must not be allowed to
        // start until the driver is available, otherwise its
        // `SwDeviceCreate` will race against `pnputil`.
        copy_drivers_and_install_if_requested(
            &src_dir,
            install_path,
            install_idd_driver,
            installer,
        )?;

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

    /// Pure (SCM-free) helper extracted from [`install_service_with`]
    /// so it can be unit-tested with a mock installer on a non-Windows
    /// CI host. Copies `<src_dir>/drivers/` into `<install_path>/drivers/`
    /// (recreating an existing directory) and, when
    /// `install_idd_driver` is true, asks `installer` to stage the
    /// LcxlVirtualDisplay driver.
    pub fn copy_drivers_and_install_if_requested(
        src_dir: &Path,
        install_path: &Path,
        install_idd_driver: bool,
        installer: &dyn DriverInstallerOps,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let src_drivers = src_dir.join("drivers");
        let dst_drivers = install_path.join("drivers");
        if src_drivers.exists() {
            if dst_drivers.exists() {
                std::fs::remove_dir_all(&dst_drivers)?;
                info!("Removed old drivers/ at {}", dst_drivers.display());
            }
            copy_dir_recursive(&src_drivers, &dst_drivers)?;
            info!("Copied drivers/ to {}", dst_drivers.display());
        } else {
            // Missing source drivers/ must not block plain installs;
            // the user can still stage the driver later from the
            // service-daemon UI once they place files manually.
            log::warn!(
                "drivers/ source directory not found at '{}'; \
                 IDD driver staging will not be possible from the install dir",
                src_drivers.display()
            );
        }

        if install_idd_driver {
            let files = installer.discover(install_path)?.ok_or_else(|| {
                format!(
                    "IDD driver files missing under '{}'. \
                     Place LcxlVirtualDisplay driver files in <install_dir>/drivers/LcxlVirtualDisplay/.",
                    install_path.display()
                )
            })?;
            installer.install(&files)?;
            info!("LcxlVirtualDisplay driver staged via pnputil");
        }
        Ok(())
    }

    /// Remove the Windows Service registration. Strict order:
    ///   1. stop the service (so the daemon releases its `SwDevice` handle);
    ///   2. uninstall every published OEM copy of the IDD driver
    ///      (best-effort — failure is logged and we still proceed);
    ///   3. delete the SCM entry;
    ///   4. remove the install directory.
    pub fn uninstall_service() -> WsResult<()> {
        uninstall_service_with(&RealInstaller)
    }

    /// Test seam used by [`uninstall_service`]. SCM steps still run in
    /// the production path; the [`DriverInstallerOps`] indirection lets
    /// the driver-uninstall side be mocked independently.
    pub fn uninstall_service_with(installer: &dyn DriverInstallerOps) -> WsResult<()> {
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
            && status.current_state != ServiceState::Stopped
        {
            let _ = service.stop();
            std::thread::sleep(Duration::from_secs(1));
        }

        // Uninstall the IDD driver BEFORE deleting the SCM entry so a
        // partial-failure leaves the user with a recoverable "service
        // entry still exists, retry uninstall" state instead of an
        // orphaned driver and no service to bind it to.
        uninstall_driver_after_service_stopped(installer);

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

    /// Pure (SCM-free) helper extracted from [`uninstall_service_with`]
    /// so the driver-uninstall side can be unit-tested with a mock
    /// installer. Failure is best-effort: a partial failure is logged
    /// and the caller proceeds with SCM cleanup so the user isn't left
    /// with a service-entry that refuses to delete because of a stale
    /// pnputil state.
    pub fn uninstall_driver_after_service_stopped(installer: &dyn DriverInstallerOps) {
        match installer.uninstall_all() {
            Ok(0) => info!("No LcxlVirtualDisplay OEM packages to uninstall"),
            Ok(n) => info!("Uninstalled {n} LcxlVirtualDisplay OEM package(s)"),
            Err(e) => {
                log::warn!("[uninstall] driver uninstall failed: {e}; continuing with SCM cleanup")
            }
        }
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use desk_virtual_display_driver_ops::{
            DRIVER_CAT_BASENAME, DRIVER_DLL_BASENAME, DRIVER_HW_ID, DRIVER_INF_BASENAME,
            DRIVER_WUDFRD_BASENAME, DriverFiles, InstallerError,
        };
        use std::sync::Mutex;
        use tempfile::TempDir;

        struct MockInstaller {
            installed: Mutex<Vec<DriverFiles>>,
            uninstall_count: Mutex<u32>,
            uninstall_result: Mutex<Result<usize, InstallerError>>,
            discover_override: Mutex<Option<Option<DriverFiles>>>,
        }

        impl MockInstaller {
            fn new() -> Self {
                Self {
                    installed: Mutex::new(Vec::new()),
                    uninstall_count: Mutex::new(0),
                    uninstall_result: Mutex::new(Ok(0)),
                    discover_override: Mutex::new(None),
                }
            }
            fn set_uninstall_result(self, r: Result<usize, InstallerError>) -> Self {
                *self.uninstall_result.lock().unwrap() = r;
                self
            }
            fn force_discover_none(self) -> Self {
                *self.discover_override.lock().unwrap() = Some(None);
                self
            }
        }

        impl DriverInstallerOps for MockInstaller {
            fn discover(&self, base_dir: &Path) -> Result<Option<DriverFiles>, InstallerError> {
                if let Some(o) = self.discover_override.lock().unwrap().clone() {
                    return Ok(o);
                }
                // Default: pretend the files are always present so the
                // happy-path install flow exercises both `discover`
                // and `install` without needing real driver bits on
                // disk.
                Ok(Some(DriverFiles::from_dir(
                    base_dir.join("drivers").join(DRIVER_HW_ID),
                )))
            }
            fn install(&self, files: &DriverFiles) -> Result<(), InstallerError> {
                self.installed.lock().unwrap().push(files.clone());
                Ok(())
            }
            fn uninstall_all(&self) -> Result<usize, InstallerError> {
                *self.uninstall_count.lock().unwrap() += 1;
                // `replace` lets us pass the prebuilt result through
                // without needing InstallerError: Clone.
                std::mem::replace(&mut *self.uninstall_result.lock().unwrap(), Ok(0))
            }
        }

        fn seed_driver_dir(src: &Path) {
            let drivers = src.join("drivers").join(DRIVER_HW_ID);
            std::fs::create_dir_all(&drivers).unwrap();
            for name in [
                DRIVER_INF_BASENAME,
                DRIVER_CAT_BASENAME,
                DRIVER_DLL_BASENAME,
                DRIVER_WUDFRD_BASENAME,
            ] {
                std::fs::write(drivers.join(name), b"").unwrap();
            }
        }

        /// Happy path: drivers/ exists on the source side, the user
        /// opted into IDD installation, the helper copies the directory
        /// and asks the installer to stage the driver exactly once.
        #[test]
        fn copy_drivers_and_install_with_flag_invokes_installer_once() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), true, &installer)
                .expect("happy path must succeed");

            assert!(
                dst.path()
                    .join("drivers")
                    .join(DRIVER_HW_ID)
                    .join(DRIVER_INF_BASENAME)
                    .exists(),
                "drivers/ must be copied into the install dir"
            );
            let installed = installer.installed.lock().unwrap();
            assert_eq!(installed.len(), 1, "installer.install must be invoked once");
        }

        /// When the user does not opt in, drivers/ is still copied so the
        /// service-daemon UI can later stage the driver itself; but
        /// `installer.install` must NOT be called.
        #[test]
        fn copy_drivers_without_flag_skips_installer() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), false, &installer)
                .expect("should succeed without staging");

            assert!(
                dst.path()
                    .join("drivers")
                    .join(DRIVER_HW_ID)
                    .join(DRIVER_INF_BASENAME)
                    .exists()
            );
            assert!(installer.installed.lock().unwrap().is_empty());
        }

        /// Missing source drivers/ + no opt-in: helper succeeds with a
        /// warn log.
        #[test]
        fn missing_drivers_dir_without_flag_is_ok() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            // No seed_driver_dir — src/drivers/ does not exist.

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), false, &installer)
                .expect("missing drivers/ must not block plain installs");
            assert!(!dst.path().join("drivers").exists());
            assert!(installer.installed.lock().unwrap().is_empty());
        }

        /// Opt-in but `installer.discover` returns None — the install
        /// must fail (a partial install with the driver missing is
        /// strictly worse than a hard error the user can address).
        #[test]
        fn opt_in_with_missing_driver_files_errors() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());

            let installer = MockInstaller::new().force_discover_none();
            let err =
                copy_drivers_and_install_if_requested(src.path(), dst.path(), true, &installer)
                    .expect_err("missing driver files must error");
            assert!(err.to_string().contains("IDD driver files missing"));
            assert!(installer.installed.lock().unwrap().is_empty());
        }

        /// Existing drivers/ in the destination is removed first so a
        /// stale install layout never wins over the freshly-copied
        /// files.
        #[test]
        fn existing_destination_drivers_dir_is_replaced() {
            let src = TempDir::new().unwrap();
            let dst = TempDir::new().unwrap();
            seed_driver_dir(src.path());
            std::fs::create_dir_all(dst.path().join("drivers").join("stale")).unwrap();
            std::fs::write(
                dst.path()
                    .join("drivers")
                    .join("stale")
                    .join("leftover.txt"),
                b"",
            )
            .unwrap();

            let installer = MockInstaller::new();
            copy_drivers_and_install_if_requested(src.path(), dst.path(), false, &installer)
                .unwrap();

            assert!(
                !dst.path().join("drivers").join("stale").exists(),
                "stale subdir must be cleared before copy"
            );
            assert!(
                dst.path()
                    .join("drivers")
                    .join(DRIVER_HW_ID)
                    .join(DRIVER_INF_BASENAME)
                    .exists()
            );
        }

        /// Uninstall helper calls `uninstall_all` exactly once.
        #[test]
        fn uninstall_helper_calls_installer_once() {
            let installer = MockInstaller::new().set_uninstall_result(Ok(2));
            uninstall_driver_after_service_stopped(&installer);
            assert_eq!(*installer.uninstall_count.lock().unwrap(), 1);
        }

        /// `installer.uninstall_all` failing must NOT panic the helper —
        /// SCM cleanup still needs to run.
        #[test]
        fn uninstall_helper_tolerates_installer_failure() {
            let installer =
                MockInstaller::new().set_uninstall_result(Err(InstallerError::StatusUnknown));
            uninstall_driver_after_service_stopped(&installer);
            assert_eq!(*installer.uninstall_count.lock().unwrap(), 1);
        }
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
