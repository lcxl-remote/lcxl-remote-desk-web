//! macOS LaunchAgent management for unattended auto-start.
//!
//! The production app ships as a single Tauri `.app` that embeds the server in
//! process (`run_with_hub`), so auto-start launches that one `.app` with
//! `--hidden`; there is no daemon/worker split on macOS. This module owns the
//! single managed plist at `~/Library/LaunchAgents/<AGENT_LABEL>.plist` and is
//! the only thing that writes it (the OS-service install path is not used on
//! macOS).
//!
//! The `pocs/poc-macos-launchd` fixture validates these semantics:
//! - `launchctl disable gui/<uid>/<label>` is a persistent override that blocks
//!   the next login load but does NOT kill a currently-running job and does NOT
//!   stop its in-session `KeepAlive`. `bootout` is the only true unload, and it
//!   kills the current process synchronously — unusable from inside the
//!   launchd-managed Tauri process (it would kill itself).
//! - `KeepAlive = { SuccessfulExit = false }` relaunches only on a crash, so the
//!   tray "Quit" (clean `exit 0`) really quits and a disable + clean exit is a
//!   real stop.
//!
//! Hence enable/disable use a symmetric "next-login boundary" semantic: write /
//! remove the plist plus set/clear the disable override, never kickstart or
//! bootout. The currently running instance is intentionally left untouched; the
//! in-session stop is delegated to the tray Quit + the KeepAlive policy above.

// DeskError is the crate's standard error type and is large; returning it in
// Result is consistent with the rest of the server crate.
#![allow(clippy::result_large_err)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use desk_utils::{
    error::DeskErrorCode,
    host_data_paths::{HostDataPaths, HostDataScope},
};

use crate::error::DeskError;

/// Stable LaunchAgent label / plist basename. Kept identical to the historical
/// `auto-launch` app name so an upgrade reuses the same single plist file
/// instead of orphaning the old one.
pub const AGENT_LABEL: &str = "lcxl-remote-desk";

/// Parameters needed to render the plist.
pub struct AgentSpec {
    /// Absolute path to the executable launchd will exec — the current process's
    /// own executable (the Tauri main binary, which embeds the server).
    pub program: PathBuf,
    /// Absolute explicit `--config-file-path`. `None` keeps the shared platform default.
    /// launchd receives no config argument for the default profile.
    pub config_file_path: Option<PathBuf>,
    /// Absolute directory for the agent's stdout/stderr logs.
    pub log_dir: PathBuf,
}

/// Loaded / configured state of the managed agent, surfaced to the console as
/// `background_start`. `loaded == false` right after enable is normal (the agent
/// takes effect at the next login), not an error.
#[derive(Debug, Clone, Copy)]
pub struct BackgroundStartStatus {
    /// The plist exists (single source of truth for the `auto_start` flag).
    pub configured: bool,
    /// launchd currently has the agent loaded in this GUI session.
    pub loaded: bool,
    /// The executable the plist points at still exists on disk.
    pub path_valid: bool,
}

fn err(msg: &str) -> DeskError {
    DeskError::new_custom_error(DeskErrorCode::AUTO_START_ERROR, msg)
}

fn home_dir() -> Result<PathBuf, DeskError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| err("HOME environment variable is not set"))
}

/// Per-user LaunchAgents directory: `~/Library/LaunchAgents`.
fn launch_agents_dir() -> Result<PathBuf, DeskError> {
    Ok(home_dir()?.join("Library").join("LaunchAgents"))
}

/// Absolute path of the single managed plist.
pub fn plist_path() -> Result<PathBuf, DeskError> {
    Ok(launch_agents_dir()?.join(format!("{AGENT_LABEL}.plist")))
}

fn current_uid() -> u32 {
    // SAFETY: getuid() is always-succeeds and has no preconditions.
    unsafe { libc::getuid() }
}

/// `gui/<uid>/<label>` service target for `launchctl enable|disable|print`.
fn service_target() -> String {
    format!("gui/{}/{}", current_uid(), AGENT_LABEL)
}

/// Default absolute log directory for the agent: `~/Library/Logs/lcxl-remote-desk`.
fn default_log_dir() -> Result<PathBuf, DeskError> {
    HostDataPaths::resolve(HostDataScope::User, None)
        .map(|paths| paths.log_dir().to_path_buf())
        .map_err(|error| err(&format!("failed to resolve log directory: {error}")))
}

/// Build an [`AgentSpec`] for the current process: launchd should re-exec this
/// very binary (the Tauri main binary, which embeds the server) with `--hidden`.
/// The optional config override has already been normalized by `HostDataPaths`;
/// only that explicit override is persisted in the plist.
pub fn current_spec(config_override: Option<&Path>) -> Result<AgentSpec, DeskError> {
    let program = std::env::current_exe()
        .map_err(|e| err(&format!("failed to get current executable path: {e}")))?;
    if config_override.is_some_and(|path| !path.is_absolute()) {
        return Err(err("explicit config path must be absolute"));
    }
    let config_file_path = config_override.map(Path::to_path_buf);
    Ok(AgentSpec {
        program,
        config_file_path,
        log_dir: default_log_dir()?,
    })
}

/// Escape the five XML predefined entities so arbitrary paths are safe inside a
/// `<string>` element.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Inverse of `xml_escape` (`&amp;` last, mirroring the escape order).
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Render the LaunchAgent plist for `spec`.
pub fn build_plist(spec: &AgentSpec) -> String {
    let program = xml_escape(&spec.program.to_string_lossy());
    let config_arguments = spec
        .config_file_path
        .as_ref()
        .map(|path| {
            format!(
                "\n        <string>--config-file-path</string>\n        <string>{}</string>",
                xml_escape(&path.to_string_lossy())
            )
        })
        .unwrap_or_default();
    let out_log = xml_escape(&spec.log_dir.join("auto-start.out.log").to_string_lossy());
    let err_log = xml_escape(&spec.log_dir.join("auto-start.err.log").to_string_lossy());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{AGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>--hidden</string>{config_arguments}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{out_log}</string>
    <key>StandardErrorPath</key>
    <string>{err_log}</string>
</dict>
</plist>
"#
    )
}

/// Resolve the `.app` bundle that contains `program`, if any. A macOS bundle has
/// the shape `Foo.app/Contents/MacOS/<exe>`, so walk parents for a component
/// ending in `.app`.
pub fn resolve_app_bundle(program: &Path) -> Option<PathBuf> {
    let mut cursor = program;
    while let Some(parent) = cursor.parent() {
        if parent.extension().and_then(|e| e.to_str()) == Some("app") {
            return Some(parent.to_path_buf());
        }
        cursor = parent;
    }
    None
}

/// Standard install locations where a signed `.app` keeps a stable cdhash and
/// TCC grants survive: `/Applications` and `~/Applications`.
fn is_in_applications_dir(bundle: &Path) -> bool {
    let Some(parent) = bundle.parent() else {
        return false;
    };
    if parent == Path::new("/Applications") {
        return true;
    }
    if let Ok(home) = home_dir()
        && parent == home.join("Applications")
    {
        return true;
    }
    false
}

/// Reject auto-start unless `program` runs from a `.app` under a standard
/// applications directory. A bare dev binary or a translocated (quarantined)
/// app would be "absolute and exists" yet never work in practice: no Info.plist,
/// a cdhash that drifts each build (TCC grants lost), or a read-only random
/// `AppTranslocation` path. App Translocation is covered for free — a
/// translocated bundle lives under `/private/var/folders/.../AppTranslocation/`,
/// whose parent is neither applications dir.
pub fn guard_app_dir(program: &Path) -> Result<(), DeskError> {
    let bundle = resolve_app_bundle(program).ok_or_else(|| {
        err(
            "auto-start requires running from an application bundle; please move \
             the app into the Applications folder first",
        )
    })?;
    if !is_in_applications_dir(&bundle) {
        return Err(err(
            "auto-start requires the app to be located in the Applications \
             folder; please move it there first",
        ));
    }
    Ok(())
}

/// Run a `launchctl` subcommand, mapping a non-zero exit to a `DeskError`.
fn launchctl(args: &[&str]) -> Result<(), DeskError> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .map_err(|e| err(&format!("failed to spawn launchctl: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(err(&format!(
            "launchctl {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(())
}

/// Atomically write `contents` to `path` (temp file in the same dir + rename),
/// so a crash mid-write never leaves a truncated plist in place of the old one.
fn atomic_write(path: &Path, contents: &str) -> Result<(), DeskError> {
    let dir = path
        .parent()
        .ok_or_else(|| err("plist path has no parent directory"))?;
    fs::create_dir_all(dir)
        .map_err(|e| err(&format!("failed to create {}: {e}", dir.display())))?;
    let tmp = path.with_extension("plist.tmp");
    {
        let mut file = fs::File::create(&tmp)
            .map_err(|e| err(&format!("failed to create temp plist: {e}")))?;
        file.write_all(contents.as_bytes())
            .map_err(|e| err(&format!("failed to write temp plist: {e}")))?;
        let _ = file.sync_all();
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        err(&format!("failed to atomically replace plist: {e}"))
    })?;
    Ok(())
}

/// Enable auto-start at the next login boundary: clear any leftover disable
/// override (best-effort — the plist's `RunAtLoad` is what actually arms it),
/// then atomically write the plist. Does not kickstart; the currently running
/// instance is unaffected.
pub fn enable(spec: &AgentSpec) -> Result<(), DeskError> {
    guard_app_dir(&spec.program)?;
    let target = service_target();
    if let Err(e) = launchctl(&["enable", &target]) {
        log::warn!("launchctl enable (override clear) failed, non-fatal: {e}");
    }
    let path = plist_path()?;
    atomic_write(&path, &build_plist(spec))?;
    log::info!("auto-start enabled: wrote {}", path.display());
    Ok(())
}

/// Disable auto-start at the next login boundary: set the persistent disable
/// override (best-effort double-guard) and remove the plist. The currently
/// running instance is intentionally left alive — `disable` does not kill it,
/// and removing the plist only prevents the next login from loading it.
pub fn disable() -> Result<(), DeskError> {
    let target = service_target();
    if let Err(e) = launchctl(&["disable", &target]) {
        log::warn!("launchctl disable (override set) failed, non-fatal: {e}");
    }
    let path = plist_path()?;
    if path.exists() {
        fs::remove_file(&path).map_err(|e| err(&format!("failed to remove plist: {e}")))?;
    }
    log::info!("auto-start disabled: removed {}", path.display());
    Ok(())
}

/// Single source of truth for the macOS `auto_start` flag: the plist exists.
pub fn is_configured() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

/// Whether launchd currently has the agent loaded in this GUI session.
fn is_loaded() -> bool {
    let target = service_target();
    Command::new("launchctl")
        .args(["print", &target])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Extract the first `ProgramArguments` entry (the executable path) from a plist.
fn parse_program_from_plist(text: &str) -> Option<String> {
    let key_pos = text.find("<key>ProgramArguments</key>")?;
    let after = &text[key_pos..];
    let s_start = after.find("<string>")? + "<string>".len();
    let s_len = after[s_start..].find("</string>")?;
    Some(xml_unescape(&after[s_start..s_start + s_len]))
}

/// Collect the configured / loaded / path-valid state for the console.
pub fn status() -> BackgroundStartStatus {
    let configured = is_configured();
    let loaded = is_loaded();
    let path_valid = plist_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|text| parse_program_from_plist(&text))
        .map(|prog| Path::new(&prog).exists())
        .unwrap_or(false);
    BackgroundStartStatus {
        configured,
        loaded,
        path_valid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> AgentSpec {
        AgentSpec {
            program: PathBuf::from(
                "/Applications/LCXL Remote Desktop.app/Contents/MacOS/lcxl-remote-desk-server",
            ),
            config_file_path: Some(PathBuf::from(
                "/Users/x/Library/Application Support/lcxl/config",
            )),
            log_dir: PathBuf::from("/Users/x/Library/Logs/lcxl"),
        }
    }

    #[test]
    fn plist_has_required_keys_and_absolute_paths() {
        let data = build_plist(&spec());
        assert!(data.contains("<key>Label</key>"));
        assert!(data.contains(&format!("<string>{AGENT_LABEL}</string>")));
        assert!(data.contains("<string>--hidden</string>"));
        assert!(data.contains("<string>--config-file-path</string>"));
        // KeepAlive must be the crash-only form, not an unconditional <true/>.
        assert!(data.contains("<key>KeepAlive</key>"));
        assert!(data.contains("<key>SuccessfulExit</key>"));
        assert!(data.contains("<false/>"));
        assert!(data.contains("<key>ThrottleInterval</key>"));
        assert!(data.contains("<key>RunAtLoad</key>"));
        // The program is the .app's own binary, not a separate sidecar path.
        assert!(data.contains("Contents/MacOS/lcxl-remote-desk-server"));
        // Absolute config + log paths.
        assert!(data.contains("Application Support/lcxl/config"));
        assert!(data.contains("auto-start.out.log"));
        assert!(data.contains("auto-start.err.log"));
    }

    #[test]
    fn default_profile_omits_config_override() {
        let mut value = spec();
        value.config_file_path = None;
        let data = build_plist(&value);
        assert!(!data.contains("--config-file-path"));
    }

    #[test]
    fn plist_escapes_xml_special_chars_in_paths() {
        let s = AgentSpec {
            program: PathBuf::from("/Applications/A&B.app/Contents/MacOS/exe"),
            config_file_path: Some(PathBuf::from("/tmp/<weird>/\"cfg\"/config")),
            log_dir: PathBuf::from("/tmp/logs"),
        };
        let data = build_plist(&s);
        assert!(data.contains("A&amp;B.app"));
        assert!(data.contains("&lt;weird&gt;"));
        assert!(data.contains("&quot;cfg&quot;"));
        // No raw unescaped ampersand/angle from the paths leaked through.
        assert!(!data.contains("A&B.app"));
        assert!(!data.contains("<weird>"));
    }

    #[test]
    fn xml_escape_unescape_roundtrip() {
        let original = "/tmp/a&b/<c>/\"d\"/'e'/config";
        assert_eq!(xml_unescape(&xml_escape(original)), original);
    }

    #[test]
    fn resolve_app_bundle_finds_enclosing_app() {
        let p = Path::new("/Applications/Foo.app/Contents/MacOS/foo");
        assert_eq!(
            resolve_app_bundle(p),
            Some(PathBuf::from("/Applications/Foo.app"))
        );
    }

    #[test]
    fn resolve_app_bundle_none_for_bare_binary() {
        let p = Path::new("/Users/x/code/target/debug/lcxl-remote-desk-server");
        assert_eq!(resolve_app_bundle(p), None);
    }

    #[test]
    fn guard_allows_standard_applications_dir() {
        let p = Path::new("/Applications/Foo.app/Contents/MacOS/foo");
        assert!(guard_app_dir(p).is_ok());
    }

    #[test]
    fn guard_rejects_bare_binary() {
        let p = Path::new("/Users/x/code/target/debug/server");
        assert!(guard_app_dir(p).is_err());
    }

    #[test]
    fn guard_rejects_non_standard_app_location() {
        let p = Path::new("/Users/x/Downloads/Foo.app/Contents/MacOS/foo");
        assert!(guard_app_dir(p).is_err());
    }

    #[test]
    fn guard_rejects_translocated_app() {
        // App Translocation runs a quarantined app from a random read-only path.
        let p = Path::new(
            "/private/var/folders/9_/abc/T/AppTranslocation/XXYY/d/Foo.app/Contents/MacOS/foo",
        );
        assert!(guard_app_dir(p).is_err());
    }

    #[test]
    fn parse_program_roundtrips_through_built_plist() {
        let data = build_plist(&spec());
        let prog = parse_program_from_plist(&data).expect("program parsed");
        assert_eq!(
            prog,
            "/Applications/LCXL Remote Desktop.app/Contents/MacOS/lcxl-remote-desk-server"
        );
    }

    #[test]
    fn parse_program_unescapes() {
        let data = build_plist(&AgentSpec {
            program: PathBuf::from("/Applications/A&B.app/Contents/MacOS/exe"),
            config_file_path: Some(PathBuf::from("/tmp/config")),
            log_dir: PathBuf::from("/tmp/logs"),
        });
        let prog = parse_program_from_plist(&data).expect("program parsed");
        assert_eq!(prog, "/Applications/A&B.app/Contents/MacOS/exe");
    }
}
