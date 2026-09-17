//! Explicit executable creation with a verified ordinary/elevated user token.
//! No ShellExecute, runas, shell interpolation or exec containment is involved.
use desk_agent_protocol::application_launch::{
    ApplicationTargetKind, ArgumentDelivery, LaunchApplicationRequest, LaunchApplicationResult,
    LaunchFailureReason, LaunchOutcome,
};
use desk_diagnose_core::application_launch::ResolvedApplicationIdentity;
use sha2::{Digest, Sha256};
use std::{ffi::c_void, fs::File, io::Read, os::windows::fs::OpenOptionsExt, path::Path};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_ASSIGN_PRIMARY,
            TOKEN_DUPLICATE, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_LINKED_TOKEN,
            TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenLinkedToken, TokenSessionId, TokenUser,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            StationsAndDesktops::{GetThreadDesktop, GetUserObjectInformationW, UOI_NAME},
            Threading::{
                CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
                GetCurrentProcess, GetCurrentThreadId, OpenProcessToken, PROCESS_INFORMATION,
                STARTUPINFOW,
            },
        },
        UI::Shell::GetUserProfileDirectoryW,
    },
    core::{PCWSTR, PWSTR},
};

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct UserEnvironment(*mut c_void);
impl Drop for UserEnvironment {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyEnvironmentBlock(self.0);
        }
    }
}

pub(crate) struct PreparedExecutable {
    request: LaunchApplicationRequest,
    command_line: Vec<u16>,
    pub(crate) identity: ResolvedApplicationIdentity,
    token: OwnedHandle,
    uses_current_token: bool,
    elevated: bool,
    environment: UserEnvironment,
    // Deny concurrent replacement/write until after CreateProcess returns.
    _image: File,
}

fn token_value<T: Default>(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<T, LaunchFailureReason> {
    let mut value = T::default();
    let mut size = 0;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(&mut value as *mut _ as *mut c_void),
            std::mem::size_of::<T>() as u32,
            &mut size,
        )
    }
    .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    Ok(value)
}

fn token_sid(token: HANDLE) -> Result<String, LaunchFailureReason> {
    let mut size = 0;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
    }
    if size == 0 || size > 65536 {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    // usize storage supplies the alignment required by TOKEN_USER and its SID.
    let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            size,
            &mut size,
        )
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let user = &*(buffer.as_ptr().cast::<TOKEN_USER>());
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let text = sid.to_string();
        let _ = LocalFree(Some(HLOCAL(sid.0.cast())));
        text.map_err(|_| LaunchFailureReason::SessionUnavailable)
    }
}

pub(crate) fn current_elevated() -> Result<bool, LaunchFailureReason> {
    catalog_session_identity()?;
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let token = OwnedHandle(raw);
    Ok(token_value::<TOKEN_ELEVATION>(token.0, TokenElevation)?.TokenIsElevated != 0)
}

pub(crate) fn catalog_session_identity() -> Result<String, LaunchFailureReason> {
    require_default_desktop()?;
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let token = OwnedHandle(raw);
    let sid = token_sid(token.0)?;
    let session = token_value::<u32>(token.0, TokenSessionId)?;
    if session == 0 || matches!(sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    Ok(format!("{sid}:{session}"))
}

/// Storage identity does not depend on the currently visible input desktop.
pub(crate) fn storage_user_sid() -> Result<String, LaunchFailureReason> {
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) }
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let token = OwnedHandle(raw);
    let sid = token_sid(token.0)?;
    if matches!(sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    Ok(sid)
}

fn require_default_desktop() -> Result<(), LaunchFailureReason> {
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_CONTROL_FLAGS, DESKTOP_READOBJECTS, OpenInputDesktop,
    };
    fn is_default(desktop: HANDLE) -> Result<(), LaunchFailureReason> {
        let mut name = [0u16; 256];
        unsafe {
            GetUserObjectInformationW(
                desktop,
                UOI_NAME,
                Some(name.as_mut_ptr().cast()),
                (name.len() * 2) as u32,
                None,
            )
        }
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let end = name.iter().position(|v| *v == 0).unwrap_or(name.len());
        if !String::from_utf16_lossy(&name[..end]).eq_ignore_ascii_case("default") {
            return Err(LaunchFailureReason::SessionUnavailable);
        }
        Ok(())
    }
    unsafe {
        let desktop = GetThreadDesktop(GetCurrentThreadId())
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        is_default(HANDLE(desktop.0))?;
        // A worker may remain attached to Default while Winlogon/UAC is active.
        // Check the actual input desktop too, without switching or opening UI.
        let input = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let result = is_default(HANDLE(input.0));
        let _ = CloseDesktop(input);
        result
    }
}

pub(crate) fn prepare(
    request: &LaunchApplicationRequest,
    expected_user_sid: &str,
    expected_session_id: u32,
) -> Result<PreparedExecutable, LaunchFailureReason> {
    request
        .validate()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    if request.target.kind != ApplicationTargetKind::Executable {
        return Err(LaunchFailureReason::Unsupported);
    }
    require_default_desktop()?;
    if expected_user_sid.is_empty() || expected_session_id == 0 {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    let mut raw = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY,
            &mut raw,
        )
    }
    .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let current = OwnedHandle(raw);
    let current_sid = token_sid(current.0)?;
    // Service and builtin service identities are never application identities.
    if current_sid != expected_user_sid
        || matches!(current_sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20")
        || token_value::<u32>(current.0, TokenSessionId)? != expected_session_id
    {
        return Err(LaunchFailureReason::SessionUnavailable);
    }
    let current_elevated =
        token_value::<TOKEN_ELEVATION>(current.0, TokenElevation)?.TokenIsElevated != 0;
    if request.run_as_admin && !current_elevated {
        use windows::Win32::Security::{
            TOKEN_ELEVATION_TYPE, TokenElevationType, TokenElevationTypeDefault,
        };
        if token_value::<TOKEN_ELEVATION_TYPE>(current.0, TokenElevationType)?
            == TokenElevationTypeDefault
        {
            return Err(LaunchFailureReason::AdminRequired);
        }
    }
    let uses_current_token = current_elevated == request.run_as_admin;
    let token = if uses_current_token {
        current
    } else {
        let linked =
            token_value::<TOKEN_LINKED_TOKEN>(current.0, TokenLinkedToken).map_err(|_| {
                if request.run_as_admin {
                    LaunchFailureReason::AdminLaunchUnavailable
                } else {
                    LaunchFailureReason::SessionUnavailable
                }
            })?;
        let linked = OwnedHandle(linked.LinkedToken);
        if token_sid(linked.0)? != current_sid
            || token_value::<u32>(linked.0, TokenSessionId)? != expected_session_id
            || (token_value::<TOKEN_ELEVATION>(linked.0, TokenElevation)?.TokenIsElevated != 0)
                != request.run_as_admin
        {
            return Err(LaunchFailureReason::SessionUnavailable);
        }
        linked
    };
    let mut environment = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut environment, Some(token.0), false) }
        .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
    let environment = UserEnvironment(environment);
    let cwd = if let Some(cwd) = &request.cwd {
        cwd.clone()
    } else {
        let mut size = 0;
        unsafe {
            let _ = GetUserProfileDirectoryW(token.0, None, &mut size);
        }
        if size == 0 || size > 32768 {
            return Err(LaunchFailureReason::SessionUnavailable);
        }
        let mut profile = vec![0u16; size as usize];
        unsafe { GetUserProfileDirectoryW(token.0, Some(PWSTR(profile.as_mut_ptr())), &mut size) }
            .map_err(|_| LaunchFailureReason::SessionUnavailable)?;
        let end = profile
            .iter()
            .position(|v| *v == 0)
            .unwrap_or(profile.len());
        String::from_utf16(&profile[..end]).map_err(|_| LaunchFailureReason::SessionUnavailable)?
    };
    if !Path::new(&cwd).is_absolute()
        || !Path::new(&cwd).is_dir()
        || !Path::new(&request.target.value).is_absolute()
    {
        return Err(LaunchFailureReason::InvalidTarget);
    }
    let target = std::fs::canonicalize(&request.target.value)
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    let cwd = std::fs::canonicalize(cwd).map_err(|_| LaunchFailureReason::InvalidTarget)?;
    let command_line = checked_command_line(&target.to_string_lossy(), &request.args)?;
    let mut image = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&target)
        .map_err(|_| LaunchFailureReason::InvalidTarget)?;
    if !image
        .metadata()
        .map_err(|_| LaunchFailureReason::InvalidTarget)?
        .is_file()
    {
        return Err(LaunchFailureReason::InvalidTarget);
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let read = image
            .read(&mut buffer)
            .map_err(|_| LaunchFailureReason::InvalidTarget)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(PreparedExecutable {
        request: request.clone(),
        command_line,
        identity: ResolvedApplicationIdentity {
            canonical_target: target.to_string_lossy().into_owned(),
            identity_digest: format!("{:x}", hash.finalize()),
            resolved_cwd: Some(cwd.to_string_lossy().into_owned()),
            user_identity: current_sid,
            session_identity: expected_session_id.to_string(),
        },
        token,
        uses_current_token,
        elevated: request.run_as_admin,
        environment,
        _image: image,
    })
}

/// Quote one argv item using the Windows C runtime backslash/quote convention.
pub(super) fn quote_argument(value: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for ch in value.chars() {
        if ch == '\\' {
            slashes += 1;
            continue;
        }
        out.extend(std::iter::repeat_n(
            '\\',
            if ch == '"' { slashes * 2 + 1 } else { slashes },
        ));
        slashes = 0;
        out.push(ch);
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn checked_command_line(target: &str, args: &[String]) -> Result<Vec<u16>, LaunchFailureReason> {
    let command = wide(
        &std::iter::once(target)
            .chain(args.iter().map(String::as_str))
            .map(quote_argument)
            .collect::<Vec<_>>()
            .join(" "),
    );
    // CreateProcess counts UTF-16 code units, including the terminating NUL.
    // Check the fully quoted command before presenting a launch for approval.
    if command.len() > 32767 {
        return Err(LaunchFailureReason::InvalidTarget);
    }
    Ok(command)
}

impl PreparedExecutable {
    /// Caller must first persist the unique Invoking claim and recheck authority.
    pub(crate) fn invoke(self, dispatch_id: String) -> LaunchApplicationResult {
        self.invoke_with_flags(dispatch_id, CREATE_UNICODE_ENVIRONMENT)
    }

    /// Only the internal application host uses this: applications retain the
    /// platform's ordinary stdio/console defaults.
    pub(crate) fn invoke_hidden_host(self, dispatch_id: String) -> LaunchApplicationResult {
        self.invoke_with_flags(
            dispatch_id,
            CREATE_UNICODE_ENVIRONMENT | windows::Win32::System::Threading::CREATE_NO_WINDOW,
        )
    }

    fn invoke_with_flags(
        self,
        dispatch_id: String,
        creation_flags: windows::Win32::System::Threading::PROCESS_CREATION_FLAGS,
    ) -> LaunchApplicationResult {
        let mut result = LaunchApplicationResult {
            dispatch_id,
            launch_outcome: LaunchOutcome::LaunchFailed,
            argument_delivery: if self.request.args.is_empty() {
                ArgumentDelivery::NotRequested
            } else {
                ArgumentDelivery::Unknown
            },
            failure_reason: None,
            requested_admin: self.request.run_as_admin,
            created_process_id: None,
            created_process_elevated: None,
            observations: vec![],
        };
        // External host Jobs are inherited normally; native launch does not
        // reject or escape them. It only avoids our per-command containment.
        if let Err(reason) = require_default_desktop() {
            result.failure_reason = Some(reason);
            return result;
        }
        let application = wide(&self.identity.canonical_target);
        let cwd = wide(self.identity.resolved_cwd.as_deref().expect("prepared cwd"));
        let mut command = self.command_line;
        let mut desktop = wide("WinSta0\\Default");
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let created = unsafe {
            if self.uses_current_token {
                CreateProcessW(
                    PCWSTR(application.as_ptr()),
                    Some(PWSTR(command.as_mut_ptr())),
                    None,
                    None,
                    false,
                    creation_flags,
                    Some(self.environment.0.cast_const()),
                    PCWSTR(cwd.as_ptr()),
                    &startup,
                    &mut process,
                )
            } else {
                CreateProcessAsUserW(
                    Some(self.token.0),
                    PCWSTR(application.as_ptr()),
                    Some(PWSTR(command.as_mut_ptr())),
                    None,
                    None,
                    false,
                    creation_flags,
                    Some(self.environment.0.cast_const()),
                    PCWSTR(cwd.as_ptr()),
                    &startup,
                    &mut process,
                )
            }
        };
        match created {
            Ok(()) => {
                let _thread = OwnedHandle(process.hThread);
                let _process = OwnedHandle(process.hProcess);
                result.launch_outcome = LaunchOutcome::LaunchAccepted;
                result.argument_delivery = if self.request.args.is_empty() {
                    ArgumentDelivery::NotRequested
                } else {
                    ArgumentDelivery::Submitted
                };
                result.created_process_id = Some(process.dwProcessId);
                result.created_process_elevated = Some(self.elevated);
            }
            Err(error) => {
                result.failure_reason = Some(match error.code().0 as u32 & 0xffff {
                    740 => LaunchFailureReason::ElevationRequired,
                    1314 if self.request.run_as_admin => {
                        LaunchFailureReason::AdminLaunchUnavailable
                    }
                    5 | 1314 => LaunchFailureReason::PermissionDenied,
                    2 | 3 | 193 => LaunchFailureReason::InvalidTarget,
                    _ => LaunchFailureReason::NativeFailure,
                })
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_limit_includes_quotes_separators_and_terminator() {
        let target = "C:\\app.exe";
        let overhead = checked_command_line(target, &[String::new()])
            .unwrap()
            .len();
        let at_limit = "a".repeat(32767 - overhead);
        assert_eq!(
            checked_command_line(target, &[at_limit.clone()])
                .unwrap()
                .len(),
            32767
        );
        assert_eq!(
            checked_command_line(target, &[at_limit + "a"]),
            Err(LaunchFailureReason::InvalidTarget)
        );
        assert_eq!(
            checked_command_line(target, &["\\".repeat(16384)]),
            Err(LaunchFailureReason::InvalidTarget)
        );
        assert_eq!(
            checked_command_line(target, &["\"".repeat(16384)]),
            Err(LaunchFailureReason::InvalidTarget)
        );
    }

    #[test]
    fn command_limit_counts_utf16_not_utf8_bytes() {
        let command = checked_command_line("C:\\app.exe", &["😀".repeat(8000)]).unwrap();
        let overhead = checked_command_line("C:\\app.exe", &[String::new()])
            .unwrap()
            .len();
        assert_eq!(command.len(), overhead + 16000);
    }
    #[test]
    fn argv_quoting_preserves_empty_space_quote_unicode_and_trailing_slashes() {
        assert_eq!(quote_argument(""), "\"\"");
        assert_eq!(quote_argument("a b"), "\"a b\"");
        assert_eq!(quote_argument("中"), "\"中\"");
        assert_eq!(quote_argument("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote_argument("a\\"), "\"a\\\\\"");
        assert_eq!(quote_argument("a\\\"b"), "\"a\\\\\\\"b\"");
    }
}
