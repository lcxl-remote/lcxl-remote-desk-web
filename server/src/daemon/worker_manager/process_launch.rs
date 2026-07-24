//! Platform-specific worker process launch helpers.

use super::*;

/// Restricted desktops whose DACL refuses ordinary user tokens; capturing
/// them needs the daemon's own SYSTEM token re-targeted to the user's
/// session. Right now only Windows' UAC secure desktop qualifies.
#[cfg(target_os = "windows")]
pub(super) fn desktop_requires_system_token(desktop_name: Option<&str>) -> bool {
    matches!(
        desktop_name,
        Some(name) if name == crate::worker::desktop_monitor::RESTRICTED_DESKTOP_NAME
    )
}

/// Watchdog decision: should we declare the worker stuck and trigger
/// a restart? Pulled into a free function so the timing semantics
/// can be exercised without spawning a real watchdog task.
///
/// Returns `false` when the watchdog is disabled (operator-controlled
/// debug aid: hung worker stays alive long enough to capture a
/// stack trace) or when the elapsed time hasn't yet exceeded the
/// configured timeout. The strict `>` (not `>=`) keeps boundary
/// behaviour predictable when timeout is set to a round number
/// equal to the heartbeat interval.
pub(super) fn worker_is_stale(
    enabled: bool,
    timeout: Duration,
    elapsed_since_heartbeat: Duration,
) -> bool {
    enabled && elapsed_since_heartbeat > timeout
}

#[cfg(target_os = "windows")]
pub(super) fn launch_worker_as_user(
    session_id: u32,
    desktop_name: Option<&str>,
    cmd_line: &str,
    force_system_token: bool,
) -> Result<NativeWindowsChild, Box<dyn std::error::Error + Send + Sync>> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        Security::{
            DuplicateTokenEx, SecurityIdentification, SecurityImpersonation, SetTokenInformation,
            TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
        },
        System::{
            Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock},
            RemoteDesktop::WTSQueryUserToken,
            Threading::{
                CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
                GetCurrentProcess, OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW,
            },
        },
    };

    info!(
        "CreateProcessAsUserW: session={session_id}, desktop={desktop_name:?}, \
         force_system_token={force_system_token}"
    );

    unsafe {
        let mut user_token = HANDLE::default();
        let use_system_token = if force_system_token {
            // Skip WTSQueryUserToken entirely — even a successful user
            // token cannot open Winlogon, so the only viable path is the
            // SYSTEM token with `SetTokenInformation(TokenSessionId)`.
            info!(
                "Forcing SYSTEM token launch path for desktop={desktop_name:?} \
                 (user-token DACL would deny access)"
            );
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut user_token)
                .map_err(|e| format!("OpenProcessToken: {e}"))?;
            true
        } else {
            match WTSQueryUserToken(session_id, &mut user_token) {
                Ok(()) => {
                    info!("WTSQueryUserToken succeeded for session {session_id}");

                    // Swap the WTS-returned filtered (UAC-limited) token for the
                    // user's elevated linked token when one exists. The reason
                    // this matters for a remote-desktop daemon is UIPI: a
                    // medium-IL process cannot `SendInput` into a higher-IL
                    // window, so without elevation the worker's mouse / keyboard
                    // injection silently no-ops the moment the user moves focus
                    // onto an admin-elevated window (admin cmd, Task Manager,
                    // Registry Editor, ...). Remote control would freeze on
                    // those windows even though the screen capture still
                    // updates. UAC's secure desktop is a separate concern —
                    // captured by `force_system_token` (Winlogon path), which
                    // does not depend on this branch.
                    //
                    // Trade-off: filtered and elevated linked tokens belong to
                    // *different* logon sessions (LUIDs). Mapped network drives
                    // (`net use Z: ...`, Explorer "Map Network Drive") are bound
                    // to the LUID where they were created — typically the
                    // filtered LUID — so the elevated worker's
                    // `GetLogicalDriveStringsW` will *not* surface them.
                    // Operators who need the worker to see mapped drives on top
                    // of UIPI injection have one OS-level escape hatch:
                    // `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\EnableLinkedConnections = 1`
                    // (admin + reboot), which mirrors mapped drives across both
                    // tokens. We surface this in the file-management UI when
                    // running under ServiceDaemon.
                    use windows::Win32::Security::{
                        GetTokenInformation, TOKEN_LINKED_TOKEN, TokenLinkedToken,
                    };
                    let mut linked_token = TOKEN_LINKED_TOKEN::default();
                    let mut return_length = 0;
                    let res = GetTokenInformation(
                        user_token,
                        TokenLinkedToken,
                        Some(&mut linked_token as *mut _ as *mut std::ffi::c_void),
                        std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
                        &mut return_length,
                    );
                    if res.is_ok() && !linked_token.LinkedToken.is_invalid() {
                        info!(
                            "Successfully retrieved LinkedToken (elevated token) for session {session_id}"
                        );
                        let _ = CloseHandle(user_token);
                        user_token = linked_token.LinkedToken;
                    } else {
                        // No linked token (already elevated, standard user with
                        // no admin token, or UAC disabled). Either path keeps
                        // mapped drives visible because we stay on the original
                        // LUID; UIPI injection only works against same-or-lower
                        // IL windows, but for non-admin users that's the entire
                        // window set anyway.
                        info!("Could not retrieve LinkedToken, using default user token");
                    }

                    false
                }
                Err(e) => {
                    warn!(
                        "WTSQueryUserToken failed (session={session_id}): {e}, \
                         falling back to SYSTEM token with SessionId injection"
                    );
                    OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut user_token)
                        .map_err(|e| format!("OpenProcessToken: {e}"))?;
                    true
                }
            }
        };

        let mut dup_token = HANDLE::default();
        let dup_result = DuplicateTokenEx(
            user_token,
            TOKEN_ALL_ACCESS,
            None,
            if use_system_token {
                SecurityImpersonation
            } else {
                SecurityIdentification
            },
            TokenPrimary,
            &mut dup_token,
        );
        let _ = CloseHandle(user_token);
        dup_result.map_err(|e| format!("DuplicateTokenEx: {e}"))?;

        // When using SYSTEM token, inject the target Session ID so the worker
        // process is associated with the correct user session / desktop.
        if use_system_token {
            let mut target_session_id = session_id;
            let set_result = SetTokenInformation(
                dup_token,
                TokenSessionId,
                &mut target_session_id as *mut _ as *const std::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            );
            if let Err(e) = set_result {
                let _ = CloseHandle(dup_token);
                return Err(
                    format!("SetTokenInformation(TokenSessionId={session_id}): {e}").into(),
                );
            }
            info!("Set SYSTEM token SessionId to {session_id}");
        }

        let mut env_block: *mut std::ffi::c_void = std::ptr::null_mut();
        let env_ok = CreateEnvironmentBlock(&mut env_block, Some(dup_token), false);
        let env_ptr: Option<*const std::ffi::c_void> = if env_ok.is_ok() {
            Some(env_block as *const _)
        } else {
            warn!("CreateEnvironmentBlock failed, proceeding without user env");
            None
        };

        let desktop_str = match desktop_name {
            Some(n) => format!("WinSta0\\{n}"),
            None => "WinSta0\\Default".to_string(),
        };
        let mut desktop_wide: Vec<u16> = OsStr::new(&desktop_str)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut si = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = windows::core::PWSTR(desktop_wide.as_mut_ptr());

        let mut pi = PROCESS_INFORMATION::default();
        let mut cmd_wide: Vec<u16> = OsStr::new(cmd_line)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let create_result = CreateProcessAsUserW(
            Some(dup_token),
            None,
            Some(windows::core::PWSTR(cmd_wide.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            env_ptr,
            None,
            &si,
            &mut pi,
        );

        if let Some(ptr) = env_ptr {
            let _ = DestroyEnvironmentBlock(ptr);
        }
        let _ = CloseHandle(dup_token);

        create_result.map_err(|e| format!("CreateProcessAsUserW: {e}"))?;

        info!(
            "Worker process created: PID={}, desktop={desktop_str}, system_token_fallback={use_system_token}",
            pi.dwProcessId
        );

        let _ = CloseHandle(pi.hThread);
        Ok(NativeWindowsChild::new(pi.hProcess, pi.dwProcessId))
    }
}
