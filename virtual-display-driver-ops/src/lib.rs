//! Windows IDD `LcxlVirtualDisplay` driver installer/uninstaller.
//!
//! Wraps `pnputil` (and PowerShell `Get-WindowsDriver` for structured
//! enumeration) so the server can stage or remove the
//! `LcxlVirtualDisplay` INF from a SCM-elevated `--install-service`
//! flow or from a ServiceDaemon REST endpoint. The API is synchronous;
//! callers are expected to offload via
//! `tokio::task::spawn_blocking` + `tokio::time::timeout` if they
//! care about HTTP responsiveness.

use std::path::{Path, PathBuf};

mod oem;
mod parser;

#[cfg(target_os = "windows")]
mod command;
#[cfg(not(target_os = "windows"))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows_impl;

/// PnP hardware id used by the INF; also doubles as the leaf directory
/// name under `<exe_dir>/drivers/`.
pub const DRIVER_HW_ID: &str = "LcxlVirtualDisplay";
pub const DRIVER_INF_BASENAME: &str = "LcxlVirtualDisplay.inf";
pub const DRIVER_CAT_BASENAME: &str = "LcxlVirtualDisplay.cat";
pub const DRIVER_DLL_BASENAME: &str = "LcxlVirtualDisplay.dll";
/// WUDF reflector co-installed by the UMDF driver. The file is
/// expected to be present in the staging dir; the WDK build copies it
/// as `WUDFRD.dll` (upper-case), so the basename must match exactly.
pub const DRIVER_WUDFRD_BASENAME: &str = "WUDFRD.dll";
/// Sub-directory under `<exe_dir>/` where the staging files live.
pub const DRIVER_DIR_NAME: &str = "drivers/LcxlVirtualDisplay";

/// Locations of the four files that make up a stageable driver.
///
/// All fields are `pub` and a [`DriverFiles::from_dir`] constructor is
/// provided so the `server` crate's mock-installer unit tests can build
/// a `DriverFiles` instance without going through the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverFiles {
    pub dir: PathBuf,
    pub inf: PathBuf,
    pub cat: PathBuf,
    pub dll: PathBuf,
    pub wudfrd: PathBuf,
}

impl DriverFiles {
    /// Builds the four expected paths under `dir` without any
    /// existence checks. Use [`discover_driver_files_in`] when you
    /// want a `None` result if any file is missing.
    pub fn from_dir(dir: PathBuf) -> Self {
        Self {
            inf: dir.join(DRIVER_INF_BASENAME),
            cat: dir.join(DRIVER_CAT_BASENAME),
            dll: dir.join(DRIVER_DLL_BASENAME),
            wudfrd: dir.join(DRIVER_WUDFRD_BASENAME),
            dir,
        }
    }
}

/// Current driver state as seen by the installer.
///
/// `installed` is `Some(b)` when the query returned a definite answer
/// and `None` when both the PowerShell and `pnputil` paths failed
/// (typical scenario: Default-mode worker running without admin).
/// `installed_oem_infs` mirrors the same three-state semantics:
/// `Some(vec![])` means "confirmed zero matches", `None` means
/// "could not determine".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverStatus {
    pub files_available: bool,
    pub files_dir: Option<PathBuf>,
    pub installed: Option<bool>,
    pub installed_oem_infs: Option<Vec<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum InstallerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("operation unsupported on this platform")]
    Unsupported,
    #[error(
        "install status could not be determined: both Get-WindowsDriver and pnputil queries failed"
    )]
    StatusUnknown,
    #[error("rejected oem inf name '{0}' (does not match ^oem\\d+\\.inf$)")]
    InvalidOemName(String),
    #[error("command `{command}` exited with {exit_code:?}: {stderr}")]
    CommandFailed {
        command: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("parse error: {0}")]
    Parse(String),
}

/// Looks for the driver files next to the current executable —
/// the production layout `<exe_dir>/drivers/LcxlVirtualDisplay/`.
///
/// Returns `Ok(None)` when one or more files are missing; returns an
/// error only for true I/O failures (permission denied, etc.).
pub fn discover_driver_files() -> Result<Option<DriverFiles>, InstallerError> {
    let exe = std::env::current_exe()?;
    let parent = exe.parent().ok_or_else(|| {
        InstallerError::Parse(format!("current_exe has no parent: {}", exe.display()))
    })?;
    discover_driver_files_in(parent)
}

/// Looks for the driver files under `<base_dir>/drivers/LcxlVirtualDisplay/`.
///
/// Used by `windows_service::install_service` to discover files
/// relative to the sidecar executable's own directory (not
/// `current_exe()`, which inside the elevated `--install-service`
/// process matches the staged copy rather than the source build).
pub fn discover_driver_files_in(base_dir: &Path) -> Result<Option<DriverFiles>, InstallerError> {
    let dir = base_dir.join("drivers").join(DRIVER_HW_ID);
    let files = DriverFiles::from_dir(dir);
    let all_present = files.inf.try_exists()?
        && files.cat.try_exists()?
        && files.dll.try_exists()?
        && files.wudfrd.try_exists()?;
    Ok(if all_present { Some(files) } else { None })
}

#[cfg(target_os = "windows")]
pub fn query_install_status() -> Result<DriverStatus, InstallerError> {
    windows_impl::query_install_status(&command::RealRunner)
}

#[cfg(target_os = "windows")]
pub fn install(files: &DriverFiles) -> Result<(), InstallerError> {
    windows_impl::install(&command::RealRunner, files)
}

#[cfg(target_os = "windows")]
pub fn uninstall_all() -> Result<usize, InstallerError> {
    windows_impl::uninstall_all(&command::RealRunner)
}

#[cfg(not(target_os = "windows"))]
pub fn query_install_status() -> Result<DriverStatus, InstallerError> {
    Ok(unsupported::query_install_status())
}

#[cfg(not(target_os = "windows"))]
pub fn install(_files: &DriverFiles) -> Result<(), InstallerError> {
    Err(InstallerError::Unsupported)
}

#[cfg(not(target_os = "windows"))]
pub fn uninstall_all() -> Result<usize, InstallerError> {
    Err(InstallerError::Unsupported)
}

/// Trait used by `server::daemon::windows_service` so the
/// install/uninstall flow can be unit-tested with a mock implementation
/// without depending on `pnputil` or SCM.
pub trait DriverInstallerOps: Send + Sync {
    fn discover(&self, base_dir: &Path) -> Result<Option<DriverFiles>, InstallerError>;
    fn install(&self, files: &DriverFiles) -> Result<(), InstallerError>;
    fn uninstall_all(&self) -> Result<usize, InstallerError>;
}

/// Production implementation backed by the free-standing
/// [`discover_driver_files_in`] / [`install`] / [`uninstall_all`]
/// functions.
pub struct RealInstaller;

impl DriverInstallerOps for RealInstaller {
    fn discover(&self, base_dir: &Path) -> Result<Option<DriverFiles>, InstallerError> {
        discover_driver_files_in(base_dir)
    }
    fn install(&self, files: &DriverFiles) -> Result<(), InstallerError> {
        install(files)
    }
    fn uninstall_all(&self) -> Result<usize, InstallerError> {
        uninstall_all()
    }
}
