//! Preserve the native error object at the Windows call site.
use desk_agent_protocol::application_launch::{LaunchError, LaunchFailureReason};
use desk_agent_protocol::native_diagnostic::{DiagnosticStage, NativeDiagnostic};

pub(crate) fn failure(
    reason: LaunchFailureReason,
    stage: DiagnosticStage,
    operation: &str,
    error: &(dyn std::error::Error + 'static),
) -> LaunchError {
    let diagnostic = if let Some(e) = error.downcast_ref::<windows::core::Error>() {
        let mut d = NativeDiagnostic::new(
            stage,
            operation,
            "hresult",
            Some(i64::from(e.code().0)),
            &e.message(),
        );
        d.name = win32_code(e.code().0).map(|code| match code {
            740 => "ERROR_ELEVATION_REQUIRED".into(),
            5 => "ERROR_ACCESS_DENIED".into(),
            1314 => "ERROR_PRIVILEGE_NOT_HELD".into(),
            1346 => "ERROR_BAD_IMPERSONATION_LEVEL".into(),
            1349 => "ERROR_BAD_TOKEN_TYPE".into(),
            _ => format!("WIN32_{code}"),
        });
        d
    } else if let Some(e) = error.downcast_ref::<std::io::Error>() {
        NativeDiagnostic::from_io(stage, operation, e)
    } else {
        NativeDiagnostic::new(stage, operation, "application", None, &error.to_string())
    };
    LaunchError { reason, diagnostic }
}
pub(crate) fn win32_code(hresult: i32) -> Option<u32> {
    let raw = hresult as u32;
    ((raw & 0xffff0000) == 0x80070000).then_some(raw & 0xffff)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_errors_keep_stage_domain_message_and_unknown_codes() {
        for (code, reason) in [
            (740, LaunchFailureReason::ElevationRequired),
            (1314, LaunchFailureReason::AdminLaunchUnavailable),
            (5, LaunchFailureReason::PermissionDenied),
            (123456, LaunchFailureReason::NativeFailure),
        ] {
            let hr = windows::core::HRESULT::from_win32(code);
            let error = windows::core::Error::from_hresult(hr);
            let result = failure(
                reason,
                DiagnosticStage::HelperStart,
                "CreateProcessAsUserW(application host)",
                &error,
            );
            assert_eq!(result.reason, reason);
            assert_eq!(result.diagnostic.stage, DiagnosticStage::HelperStart);
            assert_eq!(result.diagnostic.code, Some(i64::from(hr.0)));
            assert_eq!(result.diagnostic.domain, "hresult");
            assert!(!result.diagnostic.message.is_empty());
        }
    }
    #[test]
    fn extracts_only_win32_hresult_facility() {
        assert_eq!(win32_code(0x800702e4u32 as i32), Some(740));
        assert_eq!(win32_code(0x800402e4u32 as i32), None);
    }
}
