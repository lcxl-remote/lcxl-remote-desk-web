//! Named-pipe ACL helpers for the daemon ↔ worker IPC.
//!
//! An earlier implementation created the pipe with `D:(A;;GA;;;WD)` —
//! "Allow Generic All to Everyone" — which lets any same-machine process
//! impersonate the worker (inject fake mouse / keyboard events; intercept
//! clipboard; feed bogus encoded video frames into the daemon). This
//! module replaces that with a strict descriptor that grants pipe access
//! only to:
//!
//! - **SYSTEM** (`SY`) — the daemon's own context.
//! - **Built-in Administrators** (`BA`) — convenience for portable /
//!   non-service runs where the daemon is launched manually.
//! - The **specific user SID** owning the target session (if known) — so
//!   the worker, started under that user via `CreateProcessAsUserW`, can
//!   connect.
//!
//! The dual-pipe design hands every connected pipe to the
//! framed-transport helpers in `desk-ipc-protocol::dual_transport`, but
//! pipe creation itself stays here — the platform-specific Win32 ACL
//! plumbing has no business living in `desk-ipc-protocol`.

/// Build a SDDL string for a daemon-created named pipe. The shape is
/// always `D:(A;;GA;;;SY)(A;;GA;;;BA)[(A;;GA;;;<user-sid>)]`.
///
/// The wildcard "Everyone" / "World" SID (`WD`) is never included; if
/// `allowed_user_sid` is `None` only SYSTEM and Administrators get
/// access (the worker case where we couldn't resolve the per-session
/// user SID is rare and indicates the daemon should retry rather than
/// fall back to a permissive ACL).
pub fn build_pipe_sddl(allowed_user_sid: Option<&str>) -> String {
    match allowed_user_sid {
        Some(sid) if !sid.is_empty() => {
            format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})")
        }
        _ => "D:(A;;GA;;;SY)(A;;GA;;;BA)".to_string(),
    }
}

/// Look up the user SID owning a given Windows session id, formatted as
/// the standard `S-1-5-21-…` string. Used to scope the named-pipe ACL to
/// exactly the user under which the worker will run.
///
/// Returns `Ok(None)` if the session has no logged-on user (e.g.
/// session 0 service-only context), or `Err` if the Win32 calls failed.
/// Daemon callers should treat both `None` and `Err` as "fall back to
/// SY+BA only" — never fall back to "Everyone".
#[cfg(target_os = "windows")]
pub fn query_session_user_sid(session_id: u32) -> std::io::Result<Option<String>> {
    use std::ffi::c_void;
    use std::io::{Error, ErrorKind};
    use std::ptr::null_mut;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_USER, TokenUser};
    use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
    use windows_core::PWSTR;

    unsafe {
        let mut token = HANDLE::default();
        if WTSQueryUserToken(session_id, &mut token).is_err() {
            // No active user in this session (or privilege missing).
            // Caller falls back to SY+BA-only.
            return Ok(None);
        }

        // Ask once with size = 0 to learn the required buffer length.
        let mut needed: u32 = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
        if needed == 0 {
            let _ = CloseHandle(token);
            return Err(Error::new(
                ErrorKind::Other,
                "GetTokenInformation(TokenUser) returned needed=0",
            ));
        }

        let mut buf = vec![0u8; needed as usize];
        let res = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr() as *mut c_void),
            needed,
            &mut needed,
        );
        let _ = CloseHandle(token);
        res.map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("GetTokenInformation(TokenUser) failed: {e}"),
            )
        })?;

        // SAFETY: TOKEN_USER is `{ Sid: PSID, Attributes: u32 }`; the
        // PSID points inside `buf` so we must not let `buf` outlive the
        // string conversion below.
        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);

        let mut sid_str = PWSTR(null_mut());
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str).map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("ConvertSidToStringSidW failed: {e}"),
            )
        })?;

        // Copy out before LocalFree, then free the OS allocation.
        let owned = sid_str.to_string().map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("invalid UTF-16 in SID string: {e}"),
            )
        })?;
        let _ = LocalFree(Some(HLOCAL(sid_str.0 as *mut _)));

        Ok(Some(owned))
    }
}

/// Return the SID of the account running the daemon process.
///
/// `WTSQueryUserToken` requires privileges normally held by a Windows service.
/// A portable daemon runs as the interactive user and therefore cannot always
/// query the active-session token. In that mode its own token is the narrow,
/// correct fallback for a local-control pipe: it grants access to the same
/// account without widening the ACL to Authenticated Users or Everyone.
#[cfg(target_os = "windows")]
pub fn query_current_process_user_sid() -> std::io::Result<String> {
    use std::ffi::c_void;
    use std::io::{Error, ErrorKind};
    use std::ptr::null_mut;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_core::PWSTR;

    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(|error| {
            Error::other(format!("OpenProcessToken(current process) failed: {error}"))
        })?;

        let result = (|| -> std::io::Result<String> {
            let mut needed = 0u32;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed == 0 {
                return Err(Error::new(
                    ErrorKind::Other,
                    "GetTokenInformation(TokenUser) returned needed=0",
                ));
            }

            let mut buffer = vec![0u8; needed as usize];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr() as *mut c_void),
                needed,
                &mut needed,
            )
            .map_err(|error| {
                Error::other(format!("GetTokenInformation(TokenUser) failed: {error}"))
            })?;

            let token_user = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut sid = PWSTR(null_mut());
            ConvertSidToStringSidW(token_user.User.Sid, &mut sid)
                .map_err(|error| Error::other(format!("ConvertSidToStringSidW failed: {error}")))?;
            let user_id = sid
                .to_string()
                .map_err(|error| Error::other(format!("invalid UTF-16 in SID: {error}")))?;
            let _ = LocalFree(Some(HLOCAL(sid.0 as *mut _)));
            Ok(user_id)
        })();

        let _ = CloseHandle(token);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SDDL must include all three ACEs in the documented order and
    /// must never contain the permissive `WD` (Everyone / World) SID
    /// that the earlier code used.
    #[test]
    fn sddl_with_user_sid_grants_sy_ba_user_only() {
        let sid = "S-1-5-21-1234567890-1-2-1001";
        let sddl = build_pipe_sddl(Some(sid));
        assert_eq!(
            sddl,
            "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;S-1-5-21-1234567890-1-2-1001)"
        );
        assert!(!sddl.contains("WD"), "Everyone SID must not appear");
        assert!(
            !sddl.contains("AU"),
            "Authenticated Users SID must not appear"
        );
        assert!(
            !sddl.contains("IU"),
            "Interactive Users SID must not appear"
        );
    }

    /// Without a user SID the ACL collapses to SY+BA — not to a
    /// permissive fallback. This is the contract: when the daemon can't
    /// resolve the user SID, it MUST NOT widen access (better to fail
    /// loud than to grant Everyone).
    #[test]
    fn sddl_without_user_sid_falls_back_to_sy_ba_only() {
        let sddl = build_pipe_sddl(None);
        assert_eq!(sddl, "D:(A;;GA;;;SY)(A;;GA;;;BA)");
        assert!(!sddl.contains("WD"));
    }

    /// Empty-string user SID is treated identically to `None`. Defends
    /// against accidentally piping in a default-initialised `String` and
    /// silently building `(A;;GA;;;)` (which `ConvertStringSecurityDescriptorToSecurityDescriptorW`
    /// would reject anyway, but failing earlier is better).
    #[test]
    fn sddl_with_empty_user_sid_treated_as_none() {
        assert_eq!(build_pipe_sddl(Some("")), build_pipe_sddl(None));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn current_process_user_sid_can_secure_portable_pipe() {
        let sid = query_current_process_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"));
        let sddl = build_pipe_sddl(Some(&sid));
        assert!(sddl.contains(&format!("(A;;GA;;;{sid})")));
        assert!(!sddl.contains(";;;WD"));
    }
}
