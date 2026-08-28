//! macOS TCC (Transparency, Consent & Control) permission probing.
//!
//! Capabilities on macOS are gated by per-app, per-user TCC consent, orthogonal
//! to uid — root cannot bypass it. Screen capture, Accessibility automation,
//! and passive input observation use separate grants, so they are reported
//! independently rather than folded into `is_admin`.
//!
//! All ordinary probes are read-only and non-prompting:
//! `CGPreflightScreenCaptureAccess`, `AXIsProcessTrusted` (no options), and
//! `CGPreflightListenEventAccess` query their grants without showing prompts;
//! Apple Events uses `AEDeterminePermissionToAutomateTarget(..., false)`.

use std::ffi::c_void;

use crate::model::info::MacosPermissions;

const NUMBERS_BUNDLE_ID: &str = "com.apple.Numbers";
const PAGES_BUNDLE_ID: &str = "com.apple.Pages";
const KEYNOTE_BUNDLE_ID: &str = "com.apple.Keynote";
const TYPE_APPLICATION_BUNDLE_ID: u32 = u32::from_be_bytes(*b"bund");
const TYPE_WILDCARD: u32 = u32::from_be_bytes(*b"****");
const NO_ERR: i32 = 0;
const PROC_NOT_FOUND: i32 = -600;
const EVENT_NOT_PERMITTED: i32 = -1743;
const EVENT_WOULD_REQUIRE_CONSENT: i32 = -1744;

#[repr(C)]
struct AEDesc {
    descriptor_type: u32,
    data_handle: *mut c_void,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutomationPermissionState {
    Granted,
    Missing,
    TargetOffline,
    Failed,
}

// Screen Recording grant query. CoreGraphics is already linked by the
// `core-graphics` dependency; the explicit `#[link]` is harmless (frameworks
// dedupe) and documents the requirement.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
}

// Accessibility (synthetic input / AX) grant query.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AECreateDesc(
        type_code: u32,
        data: *const c_void,
        data_size: isize,
        result: *mut AEDesc,
    ) -> i32;
    fn AEDisposeDesc(descriptor: *mut AEDesc) -> i32;
    fn AEDeterminePermissionToAutomateTarget(
        target: *const AEDesc,
        event_class: u32,
        event_id: u32,
        ask_user_if_needed: u8,
    ) -> i32;
}

/// Probe the current process's screen-recording, Accessibility, and Input
/// Monitoring TCC grants.
pub fn probe() -> MacosPermissions {
    // SAFETY: both functions take no arguments, never block, and only read the
    // TCC database for the calling process.
    let screen_recording = unsafe { CGPreflightScreenCaptureAccess() };
    let accessibility = unsafe { AXIsProcessTrusted() };
    let input_monitoring = unsafe { CGPreflightListenEventAccess() };
    MacosPermissions {
        screen_recording,
        accessibility,
        input_monitoring,
        numbers_automation: automation_permission(NUMBERS_BUNDLE_ID, false)
            == AutomationPermissionState::Granted,
        pages_automation: automation_permission(PAGES_BUNDLE_ID, false)
            == AutomationPermissionState::Granted,
        keynote_automation: automation_permission(KEYNOTE_BUNDLE_ID, false)
            == AutomationPermissionState::Granted,
    }
}

/// Query or explicitly request Automation access for one compile-time-owned
/// bundle identifier. Callers that are reachable from remote readiness must
/// always pass `false`; only the loopback/same-origin onboarding endpoint passes
/// `true` after a local user click.
pub fn automation_permission(
    bundle_id: &'static str,
    ask_user_if_needed: bool,
) -> AutomationPermissionState {
    let mut target = AEDesc {
        descriptor_type: 0,
        data_handle: std::ptr::null_mut(),
    };
    // SAFETY: CoreServices copies the bounded static bundle-id bytes and the
    // descriptor is disposed before this function returns.
    let created = unsafe {
        AECreateDesc(
            TYPE_APPLICATION_BUNDLE_ID,
            bundle_id.as_ptr().cast(),
            bundle_id.len() as isize,
            &mut target,
        )
    };
    if created != NO_ERR {
        return AutomationPermissionState::Failed;
    }
    // SAFETY: `target` remains valid for this synchronous TCC query.
    let status = unsafe {
        AEDeterminePermissionToAutomateTarget(
            &target,
            TYPE_WILDCARD,
            TYPE_WILDCARD,
            u8::from(ask_user_if_needed),
        )
    };
    // SAFETY: ownership was returned by AECreateDesc above.
    let _ = unsafe { AEDisposeDesc(&mut target) };
    match status {
        NO_ERR => AutomationPermissionState::Granted,
        EVENT_NOT_PERMITTED | EVENT_WOULD_REQUIRE_CONSENT => AutomationPermissionState::Missing,
        PROC_NOT_FOUND => AutomationPermissionState::TargetOffline,
        _ => AutomationPermissionState::Failed,
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    static kAXTrustedCheckOptionPrompt: *const std::ffi::c_void;
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const std::ffi::c_void;
    fn CFDictionaryCreate(
        allocator: *const std::ffi::c_void,
        keys: *const *const std::ffi::c_void,
        values: *const *const std::ffi::c_void,
        count: isize,
        key_callbacks: *const std::ffi::c_void,
        value_callbacks: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFRelease(value: *const std::ffi::c_void);
}

/// Ask macOS to present its Screen Recording, Accessibility, Input Monitoring,
/// and currently reachable iWork Automation consent flows. The return values
/// are deliberately ignored:
/// `/api/server_info` re-runs the non-prompting preflight and remains the only
/// readiness truth.
pub fn request() {
    // SAFETY: the CoreGraphics call has no arguments and only asks TCC to show
    // its standard consent UI for this process.
    let _ = unsafe { CGRequestScreenCaptureAccess() };
    let _ = unsafe { CGRequestListenEventAccess() };

    // The dictionary contains two immortal framework constants. Null callback
    // tables are intentional: the temporary dictionary neither owns nor
    // releases them, and pointer identity is the contract for this AX option.
    let keys = [unsafe { kAXTrustedCheckOptionPrompt }];
    let values = [unsafe { kCFBooleanTrue }];
    // SAFETY: both arrays remain alive for the call and contain one valid
    // CoreFoundation object pointer each.
    let options = unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if !options.is_null() {
        // SAFETY: `options` is a +1 object returned by CFDictionaryCreate and
        // remains valid until the matching release below.
        let _ = unsafe { AXIsProcessTrustedWithOptions(options) };
        unsafe { CFRelease(options) };
    }

    // This function is reachable only through the loopback + same-origin local
    // onboarding endpoint. Offline apps remain false in the readiness snapshot;
    // we do not launch them or pretend a prompt was delivered.
    for bundle_id in [NUMBERS_BUNDLE_ID, PAGES_BUNDLE_ID, KEYNOTE_BUNDLE_ID] {
        let _ = automation_permission(bundle_id, true);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires all three running iWork apps with Automation approval"]
    fn live_probe_reports_per_app_iwork_tcc_grants() {
        let permissions = super::probe();
        assert!(
            permissions.numbers_automation,
            "Numbers is offline or Automation permission is not granted to this test binary"
        );
        assert!(
            permissions.pages_automation,
            "Pages is offline or Automation permission is not granted to this test binary"
        );
        assert!(
            permissions.keynote_automation,
            "Keynote is offline or Automation permission is not granted to this test binary"
        );
    }
}
