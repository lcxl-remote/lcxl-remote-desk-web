//! Startup "move to Applications" guidance (LetsMove style), macOS only.
//!
//! A stable bundle path under `/Applications` is what keeps the app's code-sign
//! identity (and therefore its TCC grants) and lets the auto-start LaunchAgent
//! point at a real binary. An app run from a DMG / Downloads folder may also be
//! *translocated* by Gatekeeper — executed from a random read-only
//! `AppTranslocation` path — which breaks both. This module detects those cases
//! at startup and offers a one-click move; it is the user-facing counterpart to
//! the server-side `auto_start` `/Applications` guard.
//!
//! It only runs on a foreground (non-`--hidden`) launch: the auto-start path is
//! already guarded to `/Applications`, so a hidden launch never needs this.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use core_foundation::base::TCFType;
use core_foundation::url::{CFURL, CFURLRef};

type Boolean = u8;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecTranslocateIsTranslocatedURL(
        path: CFURLRef,
        is_translocated: *mut Boolean,
        error: *mut *mut c_void,
    ) -> Boolean;
    fn SecTranslocateCreateOriginalPathForURL(
        translocated: CFURLRef,
        error: *mut *mut c_void,
    ) -> CFURLRef;
}

/// Detect a non-standard / translocated location and, with the user's consent,
/// move the bundle into `/Applications` and relaunch from there. Returns
/// normally (app keeps running) when no move is needed or the user declines.
pub fn maybe_offer_relocate() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return,
    };
    // Not inside a .app (e.g. a dev `target/debug` binary): nothing to relocate.
    let Some(running_bundle) = enclosing_app_bundle(&exe) else {
        return;
    };

    // If we're translocated, the bundle we can actually move is the original
    // quarantined copy, not the read-only translocated one we're running from.
    let source_bundle = if is_translocated(&running_bundle) {
        original_path(&running_bundle).unwrap_or_else(|| running_bundle.clone())
    } else {
        running_bundle.clone()
    };

    let home = std::env::var_os("HOME").map(PathBuf::from);
    if is_in_standard_apps_dir(&source_bundle, home.as_deref()) {
        return;
    }

    if !prompt_move(&source_bundle) {
        // Declined: keep running from here. Auto-start stays guarded server-side.
        log::info!("User declined move to Applications; continuing in place");
        return;
    }

    let target = target_in_applications(&source_bundle);
    if let Err(e) = move_bundle(&source_bundle, &target) {
        log::error!("Move to Applications failed: {e}");
        notify(&format!(
            "Could not move the app automatically ({e}). Please drag it into the \
             Applications folder manually."
        ));
        return;
    }

    log::info!("Moved app to {}; relaunching", target.display());
    relaunch(&target);
}

/// Resolve the `.app` bundle that contains `exe`, if any (shape
/// `Foo.app/Contents/MacOS/<exe>`).
fn enclosing_app_bundle(exe: &Path) -> Option<PathBuf> {
    let mut cursor = exe;
    while let Some(parent) = cursor.parent() {
        if parent.extension().and_then(|e| e.to_str()) == Some("app") {
            return Some(parent.to_path_buf());
        }
        cursor = parent;
    }
    None
}

/// Standard locations a user-installed `.app` belongs in: `/Applications` and
/// `~/Applications`. (Translocated paths are never here, so they fail this too.)
fn is_in_standard_apps_dir(bundle: &Path, home: Option<&Path>) -> bool {
    let Some(parent) = bundle.parent() else {
        return false;
    };
    if parent == Path::new("/Applications") {
        return true;
    }
    if let Some(home) = home
        && parent == home.join("Applications")
    {
        return true;
    }
    false
}

/// `/Applications/<bundle name>`.
fn target_in_applications(bundle: &Path) -> PathBuf {
    let name = bundle
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("LCXL Remote Desktop.app"));
    Path::new("/Applications").join(name)
}

/// Whether Gatekeeper is running `path` from a translocated (random read-only)
/// location. FFI failure is treated as "not translocated".
fn is_translocated(path: &Path) -> bool {
    let Some(url) = CFURL::from_path(path, true) else {
        return false;
    };
    let mut flag: Boolean = 0;
    let mut err: *mut c_void = std::ptr::null_mut();
    // SAFETY: `url` outlives the call; out-params are valid local pointers.
    let ok =
        unsafe { SecTranslocateIsTranslocatedURL(url.as_concrete_TypeRef(), &mut flag, &mut err) };
    ok != 0 && flag != 0
}

/// Resolve the original (pre-translocation) path of a translocated bundle.
fn original_path(translocated: &Path) -> Option<PathBuf> {
    let url = CFURL::from_path(translocated, true)?;
    let mut err: *mut c_void = std::ptr::null_mut();
    // SAFETY: `url` outlives the call; the returned ref follows the create rule.
    let orig_ref =
        unsafe { SecTranslocateCreateOriginalPathForURL(url.as_concrete_TypeRef(), &mut err) };
    if orig_ref.is_null() {
        return None;
    }
    let cfurl = unsafe { CFURL::wrap_under_create_rule(orig_ref) };
    cfurl.to_path()
}

/// Native confirmation dialog. Returns true only if the user chose to move.
fn prompt_move(bundle: &Path) -> bool {
    let name = bundle
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("This app");
    let script = format!(
        "display dialog \"To run reliably in the background and keep its \
         permissions, \\\"{name}\\\" should live in the Applications folder. \
         Move it there now?\" buttons {{\"Not Now\", \"Move to Applications\"}} \
         default button \"Move to Applications\" with icon note"
    );
    match std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains("Move to Applications"),
        Err(_) => false,
    }
}

/// Copy the bundle into `dst` with `ditto` (preserves the code signature and
/// works across APFS volumes, unlike `rename`), then best-effort remove the
/// source. Writing to `/Applications` may require the user to be an admin; a
/// failure here surfaces as a manual-move prompt.
fn move_bundle(src: &Path, dst: &Path) -> std::io::Result<()> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    let status = std::process::Command::new("/usr/bin/ditto")
        .arg(src)
        .arg(dst)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("ditto failed to copy bundle"));
    }
    // The translocated source is read-only; the real original (e.g. in
    // Downloads) is removable. Either way this is best-effort.
    let _ = std::fs::remove_dir_all(src);
    Ok(())
}

/// Launch the relocated bundle and exit this (old-location) instance.
fn relaunch(app_bundle: &Path) -> ! {
    let _ = std::process::Command::new("/usr/bin/open")
        .arg(app_bundle)
        .spawn();
    std::process::exit(0);
}

/// One-button informational dialog (best-effort).
fn notify(message: &str) {
    let script = format!("display dialog \"{message}\" buttons {{\"OK\"}} default button \"OK\"");
    let _ = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&script)
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enclosing_bundle_found_and_absent() {
        assert_eq!(
            enclosing_app_bundle(Path::new("/Applications/Foo.app/Contents/MacOS/foo")),
            Some(PathBuf::from("/Applications/Foo.app"))
        );
        assert_eq!(
            enclosing_app_bundle(Path::new("/Users/x/code/target/debug/foo")),
            None
        );
    }

    #[test]
    fn standard_apps_dir_detection() {
        let home = PathBuf::from("/Users/x");
        assert!(is_in_standard_apps_dir(
            Path::new("/Applications/Foo.app"),
            Some(&home)
        ));
        assert!(is_in_standard_apps_dir(
            Path::new("/Users/x/Applications/Foo.app"),
            Some(&home)
        ));
        assert!(!is_in_standard_apps_dir(
            Path::new("/Users/x/Downloads/Foo.app"),
            Some(&home)
        ));
        // A translocated path is likewise not a standard location.
        assert!(!is_in_standard_apps_dir(
            Path::new("/private/var/folders/9_/abc/T/AppTranslocation/XY/d/Foo.app"),
            Some(&home)
        ));
    }

    #[test]
    fn target_is_under_applications() {
        assert_eq!(
            target_in_applications(Path::new("/Users/x/Downloads/Foo.app")),
            PathBuf::from("/Applications/Foo.app")
        );
    }
}
