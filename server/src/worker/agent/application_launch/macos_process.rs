//! Launch Services handoff for an explicitly selected application bundle.
use block2::RcBlock;
use desk_agent_protocol::application_launch::{
    ApplicationTargetKind, ArgumentDelivery, LaunchApplicationRequest, LaunchApplicationResult,
    LaunchFailureReason, LaunchOutcome,
};
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

pub(crate) fn resolve(
    request: &LaunchApplicationRequest,
) -> Result<desk_agent_protocol::application_launch::ResolvedApplicationIdentity, LaunchFailureReason>
{
    use sha2::{Digest, Sha256};
    use std::io::Read;
    preflight(request)?;
    let host =
        super::unix_catalog_host::current().map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let canonical = std::fs::canonicalize(&request.target.value)
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
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
        let mut file =
            std::fs::File::open(input).map_err(|_| LaunchFailureReason::InvalidTarget)?;
        let mut bytes = [0u8; 65536];
        loop {
            let length = file
                .read(&mut bytes)
                .map_err(|_| LaunchFailureReason::InvalidTarget)?;
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
pub(crate) fn preflight(request: &LaunchApplicationRequest) -> Result<(), LaunchFailureReason> {
    request
        .validate()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    if request.run_as_admin
        || request.cwd.is_some()
        || request.target.kind != ApplicationTargetKind::MacosBundle
    {
        return Err(LaunchFailureReason::Unsupported);
    }
    super::unix_catalog_host::current().map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let path = Path::new(&request.target.value);
    if !path.is_absolute()
        || !path.is_dir()
        || !path
            .extension()
            .is_some_and(|v| v.eq_ignore_ascii_case("app"))
    {
        return Err(LaunchFailureReason::InvalidTarget);
    }
    autoreleasepool(|_| unsafe {
        let native_path = NSString::from_str(&request.target.value);
        let bundle: *mut AnyObject = msg_send![class!(NSBundle), bundleWithPath: &*native_path];
        if bundle.is_null() {
            return Err(LaunchFailureReason::InvalidTarget);
        }
        let executable: *mut AnyObject = msg_send![bundle, executablePath];
        if string(executable).is_none() {
            return Err(LaunchFailureReason::InvalidTarget);
        }
        if !request.args.is_empty() {
            // Sandboxed callers silently lose arguments according to AppKit's contract.
            if std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some() {
                return Err(LaunchFailureReason::Unsupported);
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
                    return Err(LaunchFailureReason::Unsupported);
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
            result.failure_reason = Some(reason);
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
            let failed = !error.is_null();
            let _ = sender.send((accepted, failed));
        });
        let _: () = msg_send![workspace, openApplicationAtURL: url configuration: configuration completionHandler: &*callback];
    });
    match receiver.recv_timeout(Duration::from_secs(25)) {
        Ok((true, _)) => result.launch_outcome = LaunchOutcome::LaunchAccepted,
        Ok((false, true)) => result.failure_reason = Some(LaunchFailureReason::NativeFailure),
        Ok((false, false)) | Err(_) => result.launch_outcome = LaunchOutcome::OutcomeUnknown,
    }
    // Another instance can appear between preflight and activation. AppKit may
    // ignore argv for that instance, so delivery remains unknown without evidence.
    result
}
