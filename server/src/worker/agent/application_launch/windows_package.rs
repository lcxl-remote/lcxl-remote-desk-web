//! Explicit current-user package activation, independently resolved from its AUMID.
use super::windows_diagnostics::failure;
use desk_agent_protocol::application_launch::LaunchError;
use desk_agent_protocol::application_launch::{
    ApplicationTargetKind, ArgumentDelivery, LaunchApplicationRequest, LaunchApplicationResult,
    LaunchFailureReason, LaunchOutcome,
};
use desk_agent_protocol::native_diagnostic::DiagnosticStage;
use desk_diagnose_core::application_launch::ResolvedApplicationIdentity;
use sha2::{Digest, Sha256};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
        Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily},
        System::{
            Com::{
                CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Threading::{GetCurrentProcess, OpenProcessToken},
        },
        UI::Shell::{AO_NOERRORUI, ApplicationActivationManager, IApplicationActivationManager},
    },
    core::{PCWSTR, PWSTR},
};

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
struct ComApartment;
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

pub(crate) fn resolve(
    request: &LaunchApplicationRequest,
) -> Result<ResolvedApplicationIdentity, LaunchError> {
    request
        .validate()
        .map_err(|_| LaunchError::from(LaunchFailureReason::InvalidTarget))?;
    if request.target.kind != ApplicationTargetKind::WindowsAppId
        || request.run_as_admin
        || request.cwd.is_some()
    {
        return Err(LaunchFailureReason::Unsupported.into());
    }
    let session = super::windows_process::catalog_session_identity()?;
    // Activation runs in the caller's session and privilege context. An elevated
    // worker must delegate to its ordinary-user host, never silently elevate.
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "OpenProcessToken",
            &error,
        )
    })?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    let query = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        )
    };
    unsafe {
        let _ = CloseHandle(token);
    }
    query.map_err(|error| {
        failure(
            LaunchFailureReason::SessionUnavailable,
            DiagnosticStage::Environment,
            "GetTokenInformation(TokenElevation)",
            &error,
        )
    })?;
    if elevation.TokenIsElevated != 0 {
        return Err(LaunchFailureReason::SessionUnavailable.into());
    }
    let (family, app_id) = request
        .target
        .value
        .split_once('!')
        .filter(|(a, b)| !a.is_empty() && !b.is_empty())
        .ok_or(LaunchFailureReason::InvalidTarget)?;
    if app_id.contains('!') {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    let family_wide = wide(family);
    let mut count = 0;
    let mut length = 0;
    let first = unsafe {
        GetPackagesByPackageFamily(
            PCWSTR(family_wide.as_ptr()),
            &mut count,
            None,
            &mut length,
            None,
        )
    };
    if first.0 != 122 && first.0 != 0 {
        return Err(failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "GetPackagesByPackageFamily(size)",
            &windows::core::Error::from_hresult(first.to_hresult()),
        ));
    }
    if count != 1 || length == 0 || length > 65536 {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    let mut names = vec![PWSTR::null(); count as usize];
    let mut buffer = vec![0u16; length as usize];
    unsafe {
        GetPackagesByPackageFamily(
            PCWSTR(family_wide.as_ptr()),
            &mut count,
            Some(names.as_mut_ptr()),
            &mut length,
            Some(PWSTR(buffer.as_mut_ptr())),
        )
    }
    .ok()
    .map_err(|error| {
        failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "GetPackagesByPackageFamily",
            &error,
        )
    })?;
    let package_name = unsafe { names[0].to_string() }.map_err(|error| {
        failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "decode package name",
            &error,
        )
    })?;
    let package_wide = wide(&package_name);
    let mut path_len = 0;
    let first =
        unsafe { GetPackagePathByFullName(PCWSTR(package_wide.as_ptr()), &mut path_len, None) };
    if first.0 != 122 && first.0 != 0 {
        return Err(failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "GetPackagePathByFullName(size)",
            &windows::core::Error::from_hresult(first.to_hresult()),
        ));
    }
    if path_len == 0 || path_len > 32768 {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    let mut path = vec![0u16; path_len as usize];
    unsafe {
        GetPackagePathByFullName(
            PCWSTR(package_wide.as_ptr()),
            &mut path_len,
            Some(PWSTR(path.as_mut_ptr())),
        )
    }
    .ok()
    .map_err(|error| {
        failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "GetPackagePathByFullName",
            &error,
        )
    })?;
    let end = path
        .iter()
        .position(|value| *value == 0)
        .ok_or(LaunchFailureReason::InvalidTarget)?;
    let root = String::from_utf16(&path[..end]).map_err(|error| {
        failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "decode package path",
            &error,
        )
    })?;
    let manifest_path = std::path::Path::new(&root).join("AppxManifest.xml");
    if std::fs::metadata(&manifest_path)
        .map_err(|error| {
            failure(
                LaunchFailureReason::InvalidTarget,
                DiagnosticStage::Environment,
                "stat AppxManifest.xml",
                &error,
            )
        })?
        .len()
        > 4 * 1024 * 1024
    {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    let manifest = std::fs::read(&manifest_path).map_err(|error| {
        failure(
            LaunchFailureReason::InvalidTarget,
            DiagnosticStage::TargetResolution,
            "read AppxManifest.xml",
            &error,
        )
    })?;
    validate_manifest(&manifest, app_id)?;
    let mut hash = Sha256::new();
    hash.update(package_name.as_bytes());
    hash.update(&manifest);
    Ok(ResolvedApplicationIdentity {
        canonical_target: request.target.value.clone(),
        identity_digest: format!("{:x}", hash.finalize()),
        resolved_cwd: None,
        user_identity: session.split(':').next().unwrap_or_default().into(),
        session_identity: session,
    })
}

fn validate_manifest(bytes: &[u8], app_id: &str) -> Result<(), LaunchError> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(bytes);
    let mut found = false;
    loop {
        match reader.read_event().map_err(|error| {
            failure(
                LaunchFailureReason::InvalidTarget,
                DiagnosticStage::Environment,
                "parse package manifest XML",
                &error,
            )
        })? {
            Event::Start(element) | Event::Empty(element) => {
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|error| {
                        failure(
                            LaunchFailureReason::InvalidTarget,
                            DiagnosticStage::Environment,
                            "read manifest attribute",
                            &error,
                        )
                    })?;
                    let value = attribute
                        .decode_and_unescape_value(reader.decoder())
                        .map_err(|error| {
                            failure(
                                LaunchFailureReason::InvalidTarget,
                                DiagnosticStage::Environment,
                                "decode manifest attribute",
                                &error,
                            )
                        })?;
                    if element.local_name().as_ref() == b"Application"
                        && attribute.key.as_ref() == b"Id"
                        && value == app_id
                    {
                        found = true;
                    }
                    if value == "allowElevation"
                        || value == "requireAdministrator"
                        || value == "highestAvailable"
                    {
                        return Err(LaunchFailureReason::ElevationRequired.into());
                    }
                }
            }
            Event::Eof => break,
            Event::DocType(_) => return Err(LaunchFailureReason::InvalidTarget.into()),
            _ => {}
        }
    }
    if found {
        Ok(())
    } else {
        Err(LaunchFailureReason::InvalidTarget.into())
    }
}

pub(crate) fn invoke(
    request: &LaunchApplicationRequest,
    expected: &ResolvedApplicationIdentity,
    dispatch_id: String,
) -> LaunchApplicationResult {
    let mut result = LaunchApplicationResult {
        diagnostic: None,
        dispatch_id,
        launch_outcome: LaunchOutcome::LaunchFailed,
        argument_delivery: if request.args.is_empty() {
            ArgumentDelivery::NotRequested
        } else {
            ArgumentDelivery::Unknown
        },
        failure_reason: None,
        requested_admin: request.run_as_admin,
        created_process_id: None,
        created_process_elevated: None,
        observations: vec![],
    };
    match resolve(request) {
        Ok(identity) if &identity == expected => {}
        Ok(_) => {
            result.failure_reason = Some(LaunchFailureReason::IdentityChanged);
            return result;
        }
        Err(reason) => {
            result.failure_reason = Some(reason.reason);
            result.diagnostic = Some(reason.diagnostic);
            return result;
        }
    }
    unsafe {
        if let Err(error) = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok() {
            result.diagnostic = Some(
                failure(
                    LaunchFailureReason::NativeFailure,
                    DiagnosticStage::Environment,
                    "CoInitializeEx",
                    &error,
                )
                .diagnostic,
            );
            result.failure_reason = Some(LaunchFailureReason::NativeFailure);
            return result;
        }
        let _apartment = ComApartment;
        let manager: IApplicationActivationManager =
            match CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER) {
                Ok(manager) => manager,
                Err(error) => {
                    result.diagnostic = Some(
                        failure(
                            LaunchFailureReason::NativeFailure,
                            DiagnosticStage::Environment,
                            "CoCreateInstance(IApplicationActivationManager)",
                            &error,
                        )
                        .diagnostic,
                    );
                    result.failure_reason = Some(LaunchFailureReason::NativeFailure);
                    return result;
                }
            };
        let id = wide(&request.target.value);
        let arguments = wide(
            &request
                .args
                .iter()
                .map(|value| super::windows_process::quote_argument(value))
                .collect::<Vec<_>>()
                .join(" "),
        );
        match manager.ActivateApplication(
            PCWSTR(id.as_ptr()),
            PCWSTR(arguments.as_ptr()),
            AO_NOERRORUI,
        ) {
            Ok(_) => {
                result.launch_outcome = LaunchOutcome::LaunchAccepted;
                result.argument_delivery = if request.args.is_empty() {
                    ArgumentDelivery::NotRequested
                } else {
                    ArgumentDelivery::Submitted
                };
            }
            // COM activation can cross the process boundary before a transport error.
            Err(error) => {
                result.launch_outcome = LaunchOutcome::OutcomeUnknown;
                result.diagnostic = Some(
                    failure(
                        LaunchFailureReason::NativeFailure,
                        DiagnosticStage::ProcessCreation,
                        "ActivateApplication",
                        &error,
                    )
                    .diagnostic,
                );
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manifest_requires_exact_app_id_and_rejects_elevation_capability() {
        assert!(
            validate_manifest(
                br#"<Package><Applications><Application Id="Editor"/></Applications></Package>"#,
                "Editor"
            )
            .is_ok()
        );
        assert!(
            validate_manifest(
                br#"<Package><Application Id="Editor2"/></Package>"#,
                "Editor"
            )
            .is_err()
        );
        assert_eq!(validate_manifest(br#"<Package><Application Id="Editor"/><Capability Name="allowElevation"/></Package>"#, "Editor"), Err(LaunchFailureReason::ElevationRequired.into()));
    }
}
