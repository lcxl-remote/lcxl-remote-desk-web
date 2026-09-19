//! Launch Services handoff for an explicitly selected application bundle.
use block2::RcBlock;
use desk_agent_protocol::application_launch::LaunchError;
use desk_agent_protocol::application_launch::{
    ApplicationTargetKind, ArgumentDelivery, LaunchApplicationRequest, LaunchApplicationResult,
    LaunchFailureReason, LaunchOutcome, ResolvedApplicationIdentity,
};
use desk_agent_protocol::native_diagnostic::{DiagnosticStage, NativeDiagnostic};
use objc2::{class, msg_send, rc::autoreleasepool, runtime::AnyObject};
use objc2_foundation::NSString;
use std::{ffi::CStr, path::Path, sync::mpsc, time::Duration};

fn string(object: *mut AnyObject) -> Option<String> {
    if object.is_null() {
        return None;
    }
    let bytes: *const std::ffi::c_char = unsafe { msg_send![object, UTF8String] };
    if bytes.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(bytes) }
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

// Copy bounded NSError evidence while the callback owns the native objects.
fn error_diagnostic(error: *mut AnyObject) -> NativeDiagnostic {
    let domain: *mut AnyObject = unsafe { msg_send![error, domain] };
    let code: isize = unsafe { msg_send![error, code] };
    let mut messages = Vec::new();
    let mut current = error;
    let mut visited = Vec::new();
    for _ in 0..3 {
        if current.is_null() || visited.contains(&current) {
            break;
        }
        visited.push(current);
        let description: *mut AnyObject = unsafe { msg_send![current, localizedDescription] };
        let reason: *mut AnyObject = unsafe { msg_send![current, localizedFailureReason] };
        let current_domain: *mut AnyObject = unsafe { msg_send![current, domain] };
        let current_code: isize = unsafe { msg_send![current, code] };
        messages.push(format!(
            "{} ({}): {}{}",
            string(current_domain).unwrap_or_else(|| "NSError".into()),
            current_code,
            string(description).unwrap_or_else(|| "Application launch failed".into()),
            string(reason).map(|v| format!("; {v}")).unwrap_or_default(),
        ));
        let info: *mut AnyObject = unsafe { msg_send![current, userInfo] };
        let key = NSString::from_str("NSUnderlyingError");
        let underlying: *mut AnyObject = unsafe { msg_send![info, objectForKey: &*key] };
        if underlying.is_null() {
            break;
        }
        let is_error: bool = unsafe { msg_send![underlying, isKindOfClass: class!(NSError)] };
        if !is_error {
            break;
        }
        current = underlying;
    }
    NativeDiagnostic::new(
        DiagnosticStage::ProcessCreation,
        "NSWorkspace.openApplication",
        &string(domain).unwrap_or_else(|| "NSError".into()),
        Some(code as i64),
        &messages.join("; caused by: "),
    )
}

pub(crate) fn resolve(
    request: &LaunchApplicationRequest,
) -> Result<desk_agent_protocol::application_launch::ResolvedApplicationIdentity, LaunchError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    preflight(request)?;
    let host = super::unix_catalog_host::current().map_err(|message| LaunchError {
        reason: LaunchFailureReason::SessionUnavailable,
        diagnostic: NativeDiagnostic::new(
            DiagnosticStage::Environment,
            "resolve desktop session",
            "application",
            None,
            message,
        ),
    })?;
    let canonical = std::fs::canonicalize(&request.target.value).map_err(|error| LaunchError {
        reason: LaunchFailureReason::InvalidTarget,
        diagnostic: NativeDiagnostic::from_io(
            DiagnosticStage::TargetResolution,
            "resolve application file",
            &error,
        ),
    })?;
    let path = canonical
        .to_str()
        .ok_or(LaunchFailureReason::InvalidTarget)?;
    let executable = autoreleasepool(|_| unsafe {
        let native = NSString::from_str(path);
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), bundleWithPath: &*native];
        if bundle.is_null() {
            return None;
        }
        let executable: *mut AnyObject = msg_send![bundle, executablePath];
        string(executable)
    })
    .ok_or(LaunchFailureReason::InvalidTarget)?;
    let mut digest = Sha256::new();
    for input in [
        canonical.join("Contents/Info.plist"),
        std::path::PathBuf::from(executable),
    ] {
        let mut file = std::fs::File::open(input).map_err(|error| LaunchError {
            reason: LaunchFailureReason::InvalidTarget,
            diagnostic: NativeDiagnostic::from_io(
                DiagnosticStage::TargetResolution,
                "resolve application file",
                &error,
            ),
        })?;
        let mut bytes = [0u8; 65536];
        loop {
            let length = file.read(&mut bytes).map_err(|error| LaunchError {
                reason: LaunchFailureReason::InvalidTarget,
                diagnostic: NativeDiagnostic::from_io(
                    DiagnosticStage::TargetResolution,
                    "read application identity",
                    &error,
                ),
            })?;
            if length == 0 {
                break;
            }
            digest.update(&bytes[..length]);
        }
    }
    Ok(
        desk_agent_protocol::application_launch::ResolvedApplicationIdentity {
            canonical_target: path.into(),
            identity_digest: format!("{:x}", digest.finalize()),
            resolved_cwd: None,
            user_identity: unsafe { libc::geteuid() }.to_string(),
            session_identity: host.session_id,
        },
    )
}

/// This preflight is independent of catalog lookup and executes no application.
pub(crate) fn preflight(request: &LaunchApplicationRequest) -> Result<(), LaunchError> {
    request
        .validate()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    if request.run_as_admin
        || request.cwd.is_some()
        || request.target.kind != ApplicationTargetKind::MacosBundle
    {
        return Err(LaunchFailureReason::Unsupported.into());
    }
    super::unix_catalog_host::current().map_err(|message| LaunchError {
        reason: LaunchFailureReason::SessionUnavailable,
        diagnostic: NativeDiagnostic::new(
            DiagnosticStage::Environment,
            "resolve desktop session",
            "application",
            None,
            message,
        ),
    })?;
    let path = Path::new(&request.target.value);
    if !path.is_absolute()
        || !path.is_dir()
        || !path
            .extension()
            .is_some_and(|v| v.eq_ignore_ascii_case("app"))
    {
        return Err(LaunchFailureReason::InvalidTarget.into());
    }
    autoreleasepool(|_| unsafe {
        let native_path = NSString::from_str(&request.target.value);
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), bundleWithPath: &*native_path];
        if bundle.is_null() {
            return Err(LaunchFailureReason::InvalidTarget.into());
        }
        let executable: *mut AnyObject = msg_send![bundle, executablePath];
        if string(executable).is_none() {
            return Err(LaunchFailureReason::InvalidTarget.into());
        }
        if !request.args.is_empty() {
            // Sandboxed callers silently lose arguments according to AppKit's contract.
            if std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some() {
                return Err(LaunchFailureReason::Unsupported.into());
            }
            let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
            let applications: *mut AnyObject = msg_send![workspace, runningApplications];
            let count: usize = msg_send![applications, count];
            for index in 0..count {
                let app: *mut AnyObject = msg_send![applications, objectAtIndex: index];
                let url: *mut AnyObject = msg_send![app, bundleURL];
                if url.is_null() {
                    continue;
                }
                let running_path: *mut AnyObject = msg_send![url, path];
                if string(running_path).and_then(|path| std::fs::canonicalize(path).ok())
                    == std::fs::canonicalize(path).ok()
                {
                    return Err(LaunchFailureReason::Unsupported.into());
                }
            }
        }
        Ok(())
    })
}

/// Caller persists Invoking before this call; timeout never authorizes a retry.
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
    let (sender, receiver) = mpsc::sync_channel(1);
    autoreleasepool(|_| unsafe {
        let path = NSString::from_str(&request.target.value);
        let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*path];
        let workspace: *mut AnyObject = msg_send![class!(NSWorkspace), sharedWorkspace];
        let configuration: *mut AnyObject =
            msg_send![class!(NSWorkspaceOpenConfiguration), configuration];
        let _: () = msg_send![configuration, setPromptsUserIfNeeded: false];
        let _: () = msg_send![configuration, setAllowsRunningApplicationSubstitution: false];
        let _: () = msg_send![configuration, setCreatesNewApplicationInstance: false];
        let arguments: *mut AnyObject = msg_send![class!(NSMutableArray), array];
        for argument in &request.args {
            let value = NSString::from_str(argument);
            let _: () = msg_send![arguments, addObject: &*value];
        }
        let _: () = msg_send![configuration, setArguments: arguments];
        let callback = RcBlock::new(move |application: *mut AnyObject, error: *mut AnyObject| {
            // A returned application can be a pre-existing instance; do not claim
            // its PID represents a newly created process or observed GUI window.
            let accepted = !application.is_null();
            let diagnostic = if error.is_null() {
                None
            } else {
                Some(error_diagnostic(error))
            };
            let _ = sender.send((accepted, diagnostic));
        });
        let _: () = msg_send![workspace, openApplicationAtURL: url configuration: configuration completionHandler: &*callback];
    });
    match receiver.recv_timeout(Duration::from_secs(25)) {
        Ok((true, _)) => result.launch_outcome = LaunchOutcome::LaunchAccepted,
        Ok((false, Some(diagnostic))) => {
            result.failure_reason = Some(LaunchFailureReason::NativeFailure);
            result.diagnostic = Some(diagnostic);
        }
        Ok((false, None)) => result.launch_outcome = LaunchOutcome::OutcomeUnknown,
        Err(error) => {
            result.launch_outcome = LaunchOutcome::OutcomeUnknown;
            result.diagnostic = Some(NativeDiagnostic::new(
                DiagnosticStage::ProcessWait,
                "NSWorkspace completion",
                "callback",
                None,
                &error.to_string(),
            ));
        }
    }
    // Another instance can appear between preflight and activation. AppKit may
    // ignore argv for that instance, so delivery remains unknown without evidence.
    result
}
