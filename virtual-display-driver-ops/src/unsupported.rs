//! Non-Windows fallback. Reports a definite "not installed" rather
//! than an `Unsupported` error so REST callers can render a consistent
//! status page on Linux/macOS dev hosts; the actual `install` /
//! `uninstall_all` calls still return `Unsupported` to prevent any
//! attempt to spawn `pnputil`.

use crate::DriverStatus;

pub(crate) fn query_install_status() -> DriverStatus {
    DriverStatus {
        files_available: false,
        files_dir: None,
        installed: Some(false),
        installed_oem_infs: Some(Vec::new()),
    }
}
