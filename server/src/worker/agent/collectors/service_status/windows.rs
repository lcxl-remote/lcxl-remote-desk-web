//! Windows backend for `service.status`.
//!
//! Single-service queries use the safe `windows-service` wrapper (state via
//! `query_status`, display name + start type via `query_config`). Bulk
//! enumeration uses the raw `EnumServicesStatusExW` Service Control Manager
//! API (state only) with the documented two-call sizing dance.

use desk_agent_protocol::{AgentError, AgentErrorKind, ServiceEntry};
use windows::Win32::System::Services::{
    CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, OpenSCManagerW,
    SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_STATE_ALL,
    SERVICE_WIN32,
};
use windows::core::{PCWSTR, PWSTR};
use windows_service::service::{ServiceAccess, ServiceStartType, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// `ERROR_SERVICE_DOES_NOT_EXIST` — distinguishes "no such service" (a caller
/// input problem) from a genuine SCM failure.
const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

fn internal(message: String) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message,
        retryable: true,
        safe_for_model: true,
        error_code: None,
    }
}

/// Query a single service by name via the safe `windows-service` wrapper.
pub(super) fn query_one(name: &str) -> Result<ServiceEntry, AgentError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| internal(format!("open SCM failed: {e}")))?;
    let service = manager
        .open_service(
            name,
            ServiceAccess::QUERY_STATUS | ServiceAccess::QUERY_CONFIG,
        )
        .map_err(|e| open_service_err(name, e))?;

    let status = service
        .query_status()
        .map_err(|e| internal(format!("query status for {name:?} failed: {e}")))?;
    // Config can be unreadable for some protected services; degrade to None.
    let config = service.query_config().ok();

    Ok(ServiceEntry {
        name: name.to_string(),
        display_name: config
            .as_ref()
            .map(|c| c.display_name.to_string_lossy().into_owned()),
        state: super::state_label(service_state_raw(status.current_state)),
        start_type: config.map(|c| start_type_label(c.start_type).to_string()),
    })
}

fn open_service_err(name: &str, err: windows_service::Error) -> AgentError {
    if let windows_service::Error::Winapi(io) = &err
        && io.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST)
    {
        return AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: format!("service {name:?} not found"),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        };
    }
    internal(format!("open service {name:?} failed: {err}"))
}

/// Enumerate all Win32 services via `EnumServicesStatusExW`.
pub(super) fn enumerate_all() -> Result<Vec<ServiceEntry>, AgentError> {
    // SAFETY: the SCM handle is opened for enumeration and closed on every
    // exit path; `enumerate_inner` only reads within the returned counts.
    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ENUMERATE_SERVICE)
            .map_err(|e| internal(format!("open SCM failed: {e}")))?;
        let result = enumerate_inner(scm);
        let _ = CloseServiceHandle(scm);
        result
    }
}

unsafe fn enumerate_inner(scm: SC_HANDLE) -> Result<Vec<ServiceEntry>, AgentError> {
    unsafe {
        let mut bytes_needed: u32 = 0;
        let mut count: u32 = 0;

        // Sizing call (no buffer): expected to fail with ERROR_MORE_DATA and
        // set the required byte count.
        let _ = EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            None,
            &mut bytes_needed,
            &mut count,
            None,
            PCWSTR::null(),
        );
        if bytes_needed == 0 {
            return Ok(Vec::new());
        }

        // 8-byte aligned backing: each entry begins with two pointers.
        let mut backing = vec![0u64; (bytes_needed as usize).div_ceil(8)];
        let buffer = std::slice::from_raw_parts_mut(
            backing.as_mut_ptr().cast::<u8>(),
            bytes_needed as usize,
        );
        EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            Some(buffer),
            &mut bytes_needed,
            &mut count,
            None,
            PCWSTR::null(),
        )
        .map_err(|e| internal(format!("enumerate services failed: {e}")))?;

        let entries = std::slice::from_raw_parts(
            backing.as_ptr() as *const ENUM_SERVICE_STATUS_PROCESSW,
            count as usize,
        );
        let mut out = Vec::with_capacity(count as usize);
        for entry in entries {
            let display = pwstr_to_string(entry.lpDisplayName);
            out.push(ServiceEntry {
                name: pwstr_to_string(entry.lpServiceName),
                display_name: if display.is_empty() {
                    None
                } else {
                    Some(display)
                },
                state: super::state_label(entry.ServiceStatusProcess.dwCurrentState.0),
                // Bulk enumeration skips the per-service config query.
                start_type: None,
            });
        }
        Ok(out)
    }
}

unsafe fn pwstr_to_string(value: PWSTR) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { value.to_string() }.unwrap_or_default()
    }
}

/// Numeric `SERVICE_STATUS_CURRENT_STATE` for a `windows-service` state, so the
/// single-service path reuses [`super::state_label`].
fn service_state_raw(state: ServiceState) -> u32 {
    match state {
        ServiceState::Stopped => 1,
        ServiceState::StartPending => 2,
        ServiceState::StopPending => 3,
        ServiceState::Running => 4,
        ServiceState::ContinuePending => 5,
        ServiceState::PausePending => 6,
        ServiceState::Paused => 7,
    }
}

fn start_type_label(start_type: ServiceStartType) -> &'static str {
    match start_type {
        ServiceStartType::AutoStart => "auto",
        ServiceStartType::OnDemand => "manual",
        ServiceStartType::Disabled => "disabled",
        ServiceStartType::SystemStart => "system",
        ServiceStartType::BootStart => "boot",
    }
}
