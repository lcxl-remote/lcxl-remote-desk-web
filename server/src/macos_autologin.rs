//! macOS automatic-login helper: read-only probing plus guided / manual-command
//! text generation.
//!
//! Pre-login screen capture was proven impossible on macOS (Apple gates it
//! behind a `persistent-content-capture` entitlement), so the unattended story
//! falls back to **automatic login**: the machine boots straight into the user's
//! Aqua session, where the already-granted screen-recording + accessibility TCC
//! lets the resident app capture and inject through the lock screen.
//!
//! This module deliberately contains **no password-handling logic**. Configuring
//! automatic login requires the user's plaintext password, and there is no safe
//! channel for the app to relay it (`sudo` has no TTY, `do shell script` does
//! not forward stdin, and an external-URL webview has no `invoke()`); a real
//! one-click flow would need a privileged helper and is out of scope. So the app
//! only ever:
//! - **probes** the current state read-only (`fdesetup isactive` for FileVault,
//!   `sysadminctl -autologin status` for the configured user), and
//! - **generates guidance text**: a deep link into System Settings and a
//!   copy-paste Terminal command whose `-password -` makes `sysadminctl` prompt
//!   for the password interactively — the password never passes through us.
//!
//! Everything here is pure and unit-testable except [`probe`], which shells out
//! to the two read-only commands above.

use std::process::Command;

/// Read-only automatic-login state gathered from the OS.
#[derive(Debug, Clone, Default)]
pub struct AutologinStatus {
    /// FileVault is active. When true the OS disables automatic login entirely
    /// and there is no way around it, so the helper must surface this and stop.
    pub filevault_enabled: bool,
    /// The user automatic login is currently set to, if any (`None` = disabled).
    pub autologin_user: Option<String>,
}

/// Interpret `fdesetup isactive` output. The command prints `true`/`false` and
/// sets its exit code to match (0 = active). We treat either signal as
/// authoritative so a future output tweak on one channel alone still reads
/// correctly; an empty stdout falls back to the exit code.
pub fn parse_filevault_active(stdout: &str, success: bool) -> bool {
    let trimmed = stdout.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        return true;
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return false;
    }
    success
}

/// Extract the configured automatic-login user from `sysadminctl -autologin
/// status` output.
///
/// `sysadminctl` writes to stderr and prefixes each line with a localizable
/// timestamp + `sysadminctl[pid]:`, e.g.
/// `2026-06-26 10:00:00.000 sysadminctl[123:456] Automatic login user johndoe`
/// when enabled, or a line containing `Automatic login disabled` when off. We
/// anchor on the stable English marker `Automatic login user` and take the
/// remainder of that line (tolerating an optional `:` separator), which avoids
/// depending on the timestamp/locale formatting.
pub fn parse_autologin_user(output: &str) -> Option<String> {
    const MARKER: &str = "Automatic login user";
    for line in output.lines() {
        let Some(idx) = line.find(MARKER) else {
            continue;
        };
        let rest = line[idx + MARKER.len()..].trim();
        let rest = rest.strip_prefix(':').unwrap_or(rest).trim();
        if !rest.is_empty() {
            return Some(rest.to_string());
        }
    }
    None
}

/// Single-quote a string for safe inclusion in a POSIX shell command line: wrap
/// in `'…'` and replace embedded `'` with the `'\''` idiom. Used so a username
/// with spaces or shell metacharacters renders as one literal argument in the
/// copy-paste command we show the user.
pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build the copy-paste command that enables automatic login for `username`.
/// `-password -` makes `sysadminctl` prompt for the password interactively in
/// the user's own Terminal, so the plaintext never touches this app.
pub fn build_enable_command(username: &str) -> String {
    format!(
        "sudo sysadminctl -autologin set -userName {} -password -",
        shell_single_quote(username)
    )
}

/// Command that turns automatic login back off. Takes no password.
pub fn disable_command() -> &'static str {
    "sudo sysadminctl -autologin off"
}

/// Probe FileVault state via `fdesetup isactive`. On spawn failure we
/// conservatively report "not active" (the guidance is informational; the OS
/// still enforces the real policy when the user configures login).
fn probe_filevault() -> bool {
    match Command::new("fdesetup").arg("isactive").output() {
        Ok(out) => {
            parse_filevault_active(&String::from_utf8_lossy(&out.stdout), out.status.success())
        }
        Err(e) => {
            log::warn!("failed to run `fdesetup isactive`: {e}");
            false
        }
    }
}

/// Probe the configured automatic-login user via `sysadminctl -autologin
/// status`. `sysadminctl` reports on stderr, so both streams are parsed.
fn probe_autologin_user() -> Option<String> {
    match Command::new("sysadminctl")
        .args(["-autologin", "status"])
        .output()
    {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            parse_autologin_user(&text)
        }
        Err(e) => {
            log::warn!("failed to run `sysadminctl -autologin status`: {e}");
            None
        }
    }
}

/// Gather the read-only automatic-login state. Shells out to the two probing
/// commands; never mutates anything.
pub fn probe() -> AutologinStatus {
    AutologinStatus {
        filevault_enabled: probe_filevault(),
        autologin_user: probe_autologin_user(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filevault_true_from_stdout() {
        assert!(parse_filevault_active("true\n", false));
        assert!(parse_filevault_active("TRUE", false));
    }

    #[test]
    fn filevault_false_from_stdout() {
        assert!(!parse_filevault_active("false\n", true));
        assert!(!parse_filevault_active("False", true));
    }

    #[test]
    fn filevault_falls_back_to_exit_code_when_stdout_blank() {
        assert!(parse_filevault_active("", true));
        assert!(!parse_filevault_active("   ", false));
    }

    #[test]
    fn autologin_user_parsed_from_enabled_line() {
        let out = "2026-06-26 10:00:00.000 sysadminctl[123:456] Automatic login user johndoe";
        assert_eq!(parse_autologin_user(out), Some("johndoe".to_string()));
    }

    #[test]
    fn autologin_user_tolerates_colon_separator() {
        let out = "sysadminctl[1:2] Automatic login user: jane.doe";
        assert_eq!(parse_autologin_user(out), Some("jane.doe".to_string()));
    }

    #[test]
    fn autologin_user_none_when_disabled() {
        let out = "2026-06-26 10:00:00.000 sysadminctl[123:456] Automatic login disabled";
        assert_eq!(parse_autologin_user(out), None);
    }

    #[test]
    fn autologin_user_none_for_empty_output() {
        assert_eq!(parse_autologin_user(""), None);
    }

    #[test]
    fn autologin_user_none_when_marker_present_but_value_blank() {
        // Defensive: a marker with no trailing value must not yield an empty name.
        assert_eq!(parse_autologin_user("Automatic login user   "), None);
    }

    #[test]
    fn shell_quote_wraps_plain_value() {
        assert_eq!(shell_single_quote("johndoe"), "'johndoe'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        assert_eq!(shell_single_quote("O'Brien"), "'O'\\''Brien'");
    }

    #[test]
    fn shell_quote_keeps_spaces_as_one_argument() {
        assert_eq!(shell_single_quote("john doe"), "'john doe'");
    }

    #[test]
    fn enable_command_quotes_username_and_uses_interactive_password() {
        let cmd = build_enable_command("john doe");
        assert_eq!(
            cmd,
            "sudo sysadminctl -autologin set -userName 'john doe' -password -"
        );
        // `-password -` is what keeps the plaintext out of this app.
        assert!(cmd.ends_with("-password -"));
    }

    #[test]
    fn disable_command_takes_no_password() {
        assert_eq!(disable_command(), "sudo sysadminctl -autologin off");
        assert!(!disable_command().contains("password"));
    }
}
