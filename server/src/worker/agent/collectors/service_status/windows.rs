//! Windows SCM enumeration and independent configuration reads.
use super::*;
use ::windows::Win32::Foundation::ERROR_MORE_DATA;
use ::windows::Win32::System::Services::{
    CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, OpenSCManagerW,
    SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_STATE_ALL,
    SERVICE_WIN32,
};
use ::windows::core::{PCWSTR, PWSTR};
use windows_service::service::{ServiceAccess, ServiceStartType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

fn native(
    stage: DiagnosticStage,
    operation: &str,
    error: ::windows::core::Error,
) -> NativeDiagnostic {
    NativeDiagnostic::new(
        stage,
        operation,
        "hresult",
        Some(i64::from(error.code().0)),
        &error.message(),
    )
}
pub(super) fn enrich(s: &mut ServiceEntry, _deadline: Instant) {
    let result = (|| {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
        let service = manager.open_service(&s.name, ServiceAccess::QUERY_CONFIG)?;
        service.query_config()
    })();
    match result {
        Ok(config) => {
            s.start_type = Some(start_type_label(config.start_type).into());
            s.display_name = Some(config.display_name.to_string_lossy().into_owned());
        }
        Err(e) => {
            s.metadata_error = Some(match e {
                windows_service::Error::Winapi(io) => NativeDiagnostic::from_io(
                    DiagnosticStage::ServiceConfiguration,
                    "QueryServiceConfigW",
                    &io,
                ),
                other => diagnostic(
                    DiagnosticStage::ServiceConfiguration,
                    "QueryServiceConfigW",
                    &other.to_string(),
                ),
            });
        }
    }
}

pub(super) fn enumerate(_params: &ServiceStatusParams, deadline: Instant) -> Enumeration {
    let mut result = Enumeration {
        scope: "system".into(),
        ..Default::default()
    };
    // SAFETY: the handle is closed after enumeration, including all failure paths.
    unsafe {
        match OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ENUMERATE_SERVICE) {
            Ok(scm) => {
                enumerate_inner(scm, deadline, &mut result);
                let _ = CloseServiceHandle(scm);
            }
            Err(e) => result.errors.push(native(
                DiagnosticStage::ServiceEnumeration,
                "OpenSCManagerW",
                e,
            )),
        }
    }
    result
}

unsafe fn enumerate_inner(scm: SC_HANDLE, deadline: Instant, out: &mut Enumeration) {
    enumerate_with(deadline, out, |buffer, needed, count, resume| unsafe {
        EnumServicesStatusExW(
            scm,
            SC_ENUM_PROCESS_INFO,
            SERVICE_WIN32,
            SERVICE_STATE_ALL,
            Some(buffer),
            needed,
            count,
            Some(resume),
            PCWSTR::null(),
        )
    });
}

fn enumerate_with(
    deadline: Instant,
    out: &mut Enumeration,
    mut next: impl FnMut(&mut [u8], &mut u32, &mut u32, &mut u32) -> ::windows::core::Result<()>,
) {
    // The documented maximum buffer is 256 KiB. Native continuation is internal
    // to this enumeration; model-facing cursors are authenticated separately.
    const BYTES: usize = 256 * 1024;
    let mut resume = 0u32;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..256 {
        if Instant::now() >= deadline {
            break;
        }
        let previous = resume;
        let mut needed = 0;
        let mut count = 0;
        let mut backing = vec![0u64; BYTES / 8];
        // SAFETY: aligned storage is live during the API call and all decoding.
        let buffer =
            unsafe { std::slice::from_raw_parts_mut(backing.as_mut_ptr().cast::<u8>(), BYTES) };
        let status = next(buffer, &mut needed, &mut count, &mut resume);
        let more = status
            .as_ref()
            .is_err_and(|e| e.code() == ERROR_MORE_DATA.to_hresult());
        if let Err(e) = status.as_ref() {
            if !more {
                out.errors.push(native(
                    DiagnosticStage::ServiceEnumeration,
                    "EnumServicesStatusExW",
                    e.clone(),
                ));
                return;
            }
        }
        if count as usize > BYTES / std::mem::size_of::<ENUM_SERVICE_STATUS_PROCESSW>() {
            out.errors.push(diagnostic(
                DiagnosticStage::ServiceEnumeration,
                "EnumServicesStatusExW",
                "Invalid returned entry count",
            ));
            return;
        }
        let entries = unsafe {
            std::slice::from_raw_parts(
                backing.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                count as usize,
            )
        };
        for entry in entries {
            let decoded = unsafe {
                read_string(entry.lpServiceName, &backing)
                    .zip(read_string(entry.lpDisplayName, &backing))
            };
            let Some((name, display)) = decoded else {
                out.errors.push(diagnostic(
                    DiagnosticStage::ServiceEnumeration,
                    "EnumServicesStatusExW",
                    "Invalid service string in enumeration buffer",
                ));
                return;
            };
            out.services.push(ServiceEntry {
                name,
                display_name: Some(display),
                state: super::state_label(entry.ServiceStatusProcess.dwCurrentState.0),
                scope: "system".into(),
                ..Default::default()
            });
        }
        if !more {
            return;
        }
        if !continuation_progress(previous, resume, count) || !seen.insert(resume) {
            out.errors.push(diagnostic(
                DiagnosticStage::ServiceEnumeration,
                "EnumServicesStatusExW",
                "Enumeration did not advance; query incomplete",
            ));
            return;
        }
    }
    out.errors.push(diagnostic(
        DiagnosticStage::ServiceEnumeration,
        "EnumServicesStatusExW",
        "Enumeration budget exceeded; query incomplete",
    ));
}
fn continuation_progress(previous: u32, next: u32, count: u32) -> bool {
    count > 0 && next != 0 && next != previous
}
unsafe fn read_string(value: PWSTR, backing: &[u64]) -> Option<String> {
    let start = backing.as_ptr() as usize;
    let end = start.checked_add(std::mem::size_of_val(backing))?;
    let ptr = value.0 as usize;
    if ptr < start || ptr >= end || ptr % 2 != 0 {
        return None;
    }
    let words = unsafe { std::slice::from_raw_parts(value.0, (end - ptr) / 2) };
    let len = words.iter().position(|c| *c == 0)?;
    Some(String::from_utf16_lossy(&words[..len]))
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

#[cfg(test)]
mod tests {
    use super::*;
    fn write_entry(buffer: &mut [u8], name: &str) {
        let text: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let base = buffer.as_mut_ptr();
        unsafe {
            let ptr = base
                .add(std::mem::size_of::<ENUM_SERVICE_STATUS_PROCESSW>())
                .cast::<u16>();
            std::ptr::copy_nonoverlapping(text.as_ptr(), ptr, text.len());
            let entry = &mut *base.cast::<ENUM_SERVICE_STATUS_PROCESSW>();
            *entry = ENUM_SERVICE_STATUS_PROCESSW::default();
            entry.lpServiceName = PWSTR(ptr);
            entry.lpDisplayName = PWSTR(ptr);
        }
    }
    #[test]
    fn native_empty_success_and_initial_failure_are_distinct() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut empty = Enumeration::default();
        enumerate_with(deadline, &mut empty, |_, _, _, _| Ok(()));
        assert!(empty.services.is_empty() && empty.errors.is_empty());
        let mut failed = Enumeration::default();
        enumerate_with(deadline, &mut failed, |_, needed, _, _| {
            assert_eq!(*needed, 0);
            Err(::windows::core::Error::from_hresult(
                ::windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult(),
            ))
        });
        assert!(failed.services.is_empty());
        assert_eq!(
            failed.errors[0].code,
            Some(i64::from(
                ::windows::Win32::Foundation::ERROR_ACCESS_DENIED
                    .to_hresult()
                    .0
            ))
        );
    }
    #[test]
    fn continuation_retains_every_batch_and_reports_midstream_failure() {
        for fail_last in [false, true] {
            let mut out = Enumeration::default();
            let mut calls = 0;
            enumerate_with(
                Instant::now() + Duration::from_secs(1),
                &mut out,
                |buffer, _, count, resume| {
                    calls += 1;
                    assert_eq!(*resume, calls - 1);
                    if fail_last && calls == 3 {
                        return Err(::windows::core::Error::from_hresult(
                            ::windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult(),
                        ));
                    }
                    write_entry(buffer, &format!("service{calls}"));
                    *count = 1;
                    *resume = calls;
                    if calls < 3 {
                        Err(::windows::core::Error::from_hresult(
                            ERROR_MORE_DATA.to_hresult(),
                        ))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(calls, 3);
            assert_eq!(out.services.len(), if fail_last { 2 } else { 3 });
            assert_eq!(!out.errors.is_empty(), fail_last);
        }
    }
    #[test]
    fn stalled_native_cursor_retains_received_entry_and_stops() {
        let mut out = Enumeration::default();
        enumerate_with(
            Instant::now() + Duration::from_secs(1),
            &mut out,
            |buffer, _, count, _| {
                write_entry(buffer, "known");
                *count = 1;
                Err(::windows::core::Error::from_hresult(
                    ERROR_MORE_DATA.to_hresult(),
                ))
            },
        );
        assert_eq!(out.services.len(), 1);
        assert!(out.errors[0].message.contains("did not advance"));
    }
    #[test]
    fn requires_progress_and_a_valid_continuation() {
        assert!(continuation_progress(0, 12, 4));
        assert!(!continuation_progress(12, 12, 4));
        assert!(!continuation_progress(0, 0, 0));
        assert!(!continuation_progress(0, 12, 0));
    }
    #[test]
    fn rejects_out_of_buffer_strings() {
        let backing = [0u64; 4];
        assert!(unsafe { read_string(PWSTR::null(), &backing) }.is_none());
        let ptr = backing.as_ptr() as *mut u16;
        assert_eq!(
            unsafe { read_string(PWSTR(ptr), &backing) }.as_deref(),
            Some("")
        );
    }
}
