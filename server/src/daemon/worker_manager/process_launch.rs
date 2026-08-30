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

#[cfg(target_os = "linux")]
pub(super) fn inherited_linux_worker_identity_is_safe(effective_uid: u32) -> bool {
    effective_uid != 0
}

#[cfg(target_os = "linux")]
#[allow(dead_code)] // wired by the map-backed resident-worker runtime in the next implementation slice
pub(super) fn launch_linux_session_worker(
    executable: &std::path::Path,
    pipe_name: &str,
    registration: &crate::host_control::session_shell::RegisteredSessionShell,
) -> Result<tokio::process::Child, Box<dyn std::error::Error + Send + Sync>> {
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--startup-mode")
        .arg("session-worker")
        .arg("--pipe")
        .arg(pipe_name);
    configure_linux_session_command(&mut command, executable, registration)?;
    let mut child = command.spawn()?;
    let result = child
        .id()
        .ok_or("spawned Linux session worker has no process id")
        .and_then(|pid| verify_linux_worker_process_identity(pid, registration));
    if let Err(error) = result {
        let _ = child.start_kill();
        return Err(error.into());
    }
    Ok(child)
}

#[cfg(target_os = "linux")]
fn verify_linux_worker_process_identity(
    pid: u32,
    registration: &crate::host_control::session_shell::RegisteredSessionShell,
) -> Result<(), &'static str> {
    let actual = crate::host_control::session_shell::read_process_identity(pid)
        .map_err(|_| "cannot verify spawned Linux worker identity through /proc")?;
    let expected = &registration.process_identity;
    if actual.uid != expected.uid
        || actual.gid != expected.gid
        || actual.supplementary_groups != expected.supplementary_groups
    {
        return Err("spawned Linux worker credentials differ from the registered Tauri process");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[allow(dead_code)] // see launch_linux_session_worker staging note
fn configure_linux_session_command(
    command: &mut tokio::process::Command,
    executable: &std::path::Path,
    registration: &crate::host_control::session_shell::RegisteredSessionShell,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::ffi::CString;
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    use std::process::Stdio;

    let identity = &registration.process_identity;
    if identity.uid == 0 {
        return Err("refusing to launch a Linux session worker as root".into());
    }

    let daemon_euid = unsafe { libc::geteuid() };
    if daemon_euid != 0 && daemon_euid != identity.uid {
        return Err(format!(
            "non-root daemon uid {daemon_euid} cannot launch worker for uid {}",
            identity.uid
        )
        .into());
    }
    if daemon_euid != 0 {
        let mut expected_groups = identity.supplementary_groups.clone();
        expected_groups.sort_unstable();
        expected_groups.dedup();
        if unsafe { libc::getegid() } != identity.gid
            || current_supplementary_groups()? != expected_groups
        {
            return Err("non-root daemon groups differ from the registered Tauri process".into());
        }
    }

    if daemon_euid == 0 {
        let metadata = executable.symlink_metadata()?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.uid() != 0
            || metadata.mode() & 0o022 != 0
        {
            return Err(format!(
                "worker executable {} must be root-owned and not group/world-writable",
                executable.display()
            )
            .into());
        }
    }

    let cwd = CString::new(registration.cwd.as_os_str().as_bytes())?;
    let home = registration
        .environment
        .iter()
        .find(|(key, _)| key.as_os_str().as_bytes() == b"HOME")
        .map(|(_, value)| value.as_os_str())
        .filter(|value| std::path::Path::new(value).is_absolute())
        .ok_or("registered session environment lacks an absolute HOME")?;
    let home = CString::new(home.as_bytes())?;
    let uid = identity.uid;
    let gid = identity.gid;
    let groups = identity.supplementary_groups.clone();
    let umask = registration.umask as libc::mode_t;

    command
        .env_clear()
        .envs(registration.environment.iter().cloned())
        .stdin(Stdio::null());

    // SAFETY: all captured inputs are owned and the closure only invokes
    // async-signal-safe libc credential, umask, and chdir operations before
    // exec. Credential order is supplementary groups -> gid -> uid.
    unsafe {
        command.pre_exec(move || {
            if daemon_euid == 0 {
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setresgid(gid, gid, gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setresuid(uid, uid, uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getuid() != uid
                    || libc::geteuid() != uid
                    || libc::getgid() != gid
                    || libc::getegid() != gid
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "worker credential drop did not converge",
                    ));
                }
            } else if libc::geteuid() != uid || libc::getegid() != gid {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "daemon identity changed before worker exec",
                ));
            }

            libc::umask(umask);
            if libc::chdir(cwd.as_ptr()) != 0 && libc::chdir(home.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn current_supplementary_groups() -> std::io::Result<Vec<u32>> {
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut groups = vec![0; count as usize];
    if count > 0 && unsafe { libc::getgroups(count, groups.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    groups.sort_unstable();
    groups.dedup();
    Ok(groups)
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
                    return Err(format!(
                        "WTSQueryUserToken failed for SessionUser worker session={session_id}; refusing SYSTEM-token fallback: {e}"
                    )
                    .into());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn root_daemon_identity_is_never_a_valid_inherited_worker_identity() {
        assert!(!inherited_linux_worker_identity_is_safe(0));
        assert!(inherited_linux_worker_identity_is_safe(1_000));
    }

    #[cfg(target_os = "linux")]
    fn current_registration(
        environment: Vec<(&[u8], &[u8])>,
    ) -> std::sync::Arc<crate::host_control::session_shell::RegisteredSessionShell> {
        use crate::host_control::protocol::{
            EnvironmentEntryBase64, SESSION_SHELL_PROTOCOL_VERSION, SessionShellInfo,
        };
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let stat = std::fs::read_to_string(format!("/proc/{}/stat", std::process::id())).unwrap();
        let after_name = &stat[stat.rfind(") ").unwrap() + 2..];
        let start_ticks = after_name
            .split_ascii_whitespace()
            .nth(19)
            .unwrap()
            .parse()
            .unwrap();
        crate::host_control::session_shell::SessionShellRegistry::default()
            .register(
                1,
                SessionShellInfo {
                    app_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: SESSION_SHELL_PROTOCOL_VERSION,
                    pid: std::process::id(),
                    process_start_ticks: start_ticks,
                    reported_uid: unsafe { libc::geteuid() },
                    session_id: Some("launch-test".to_string()),
                    seat: Some("seat-test".to_string()),
                    session_type: Some("tty".to_string()),
                    cwd_base64: STANDARD.encode(b"/tmp"),
                    umask: 0o027,
                    environment: environment
                        .into_iter()
                        .map(|(key, value)| EnvironmentEntryBase64 {
                            key_base64: STANDARD.encode(key),
                            value_base64: STANDARD.encode(value),
                        })
                        .collect(),
                },
            )
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn configured_linux_worker_uses_only_registered_environment() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let registration =
            current_registration(vec![(b"HOME", b"/tmp"), (b"ONLY_REGISTERED", b"present")]);
        let executable = std::path::Path::new("/usr/bin/env");
        let mut command = tokio::process::Command::new(executable);
        command.arg("-0").stdout(std::process::Stdio::piped());
        configure_linux_session_command(&mut command, executable, &registration).unwrap();

        let output = command.output().await.unwrap();
        assert!(output.status.success());
        assert!(
            output
                .stdout
                .windows(24)
                .any(|part| part == b"ONLY_REGISTERED=present\0")
        );
        assert!(output.stdout.windows(10).any(|part| part == b"HOME=/tmp\0"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_worker_target_uid_zero_is_rejected_before_spawn() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let mut registration =
            (*current_registration(vec![(b"HOME", b"/tmp"), (b"PATH", b"/usr/bin")])).clone();
        registration.process_identity.uid = 0;
        let error = launch_linux_session_worker(
            std::path::Path::new("/usr/bin/env"),
            "/tmp/never-used-worker-socket",
            &registration,
        )
        .unwrap_err();
        assert!(error.to_string().contains("as root"));
    }
}
