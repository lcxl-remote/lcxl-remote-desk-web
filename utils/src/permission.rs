#[cfg(windows)]
use windows::Win32::UI::Shell::IsUserAnAdmin;

/// Check if the current process has administrative/root privileges.
pub fn is_admin() -> bool {
    #[cfg(windows)]
    {
        unsafe { IsUserAnAdmin().as_bool() }
    }

    #[cfg(unix)]
    {
        unsafe { libc::getuid() == 0 }
    }

    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

/// Check whether the platform service with the given name is currently running.
///
/// Windows queries SCM. Linux queries the system systemd manager only when the
/// host actually booted with systemd. Other platforms return `false`.
pub fn is_service_running(service_name: &str) -> bool {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::System::Services::{
            CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
            SC_MANAGER_CONNECT, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
        };

        unsafe {
            let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
                return false;
            };

            let name_wide: Vec<u16> = OsStr::new(service_name)
                .encode_wide()
                .chain(std::iter::once(0u16))
                .collect();

            let svc = match OpenServiceW(
                scm,
                windows::core::PCWSTR(name_wide.as_ptr()),
                SERVICE_QUERY_STATUS,
            ) {
                Ok(s) => s,
                Err(_) => {
                    let _ = CloseServiceHandle(scm);
                    return false;
                }
            };
            let _ = CloseServiceHandle(scm);

            let mut status = windows::Win32::System::Services::SERVICE_STATUS::default();
            let ok = QueryServiceStatus(svc, &mut status).is_ok();
            let _ = CloseServiceHandle(svc);

            ok && status.dwCurrentState == SERVICE_RUNNING
        }
    }

    #[cfg(target_os = "linux")]
    {
        if !std::path::Path::new("/run/systemd/system").is_dir() {
            return false;
        }
        std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", service_name])
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = service_name;
        false
    }
}

/// Check whether the platform service with the given name is installed.
///
/// Windows queries SCM; Linux asks systemd whether the unit is loaded.
pub fn is_service_installed(service_name: &str) -> bool {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::System::Services::{
            CloseServiceHandle, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
            SERVICE_QUERY_STATUS,
        };

        unsafe {
            let Ok(scm) = OpenSCManagerW(None, None, SC_MANAGER_CONNECT) else {
                return false;
            };

            let name_wide: Vec<u16> = OsStr::new(service_name)
                .encode_wide()
                .chain(std::iter::once(0u16))
                .collect();

            let svc_result = OpenServiceW(
                scm,
                windows::core::PCWSTR(name_wide.as_ptr()),
                SERVICE_QUERY_STATUS,
            );
            let _ = CloseServiceHandle(scm);

            match svc_result {
                Ok(svc) => {
                    let _ = CloseServiceHandle(svc);
                    true
                }
                Err(_) => false,
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        if !std::path::Path::new("/run/systemd/system").is_dir() {
            return false;
        }
        std::process::Command::new("systemctl")
            .args(["show", "--property=LoadState", "--value", service_name])
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).trim() == "loaded"
            })
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = service_name;
        false
    }
}
