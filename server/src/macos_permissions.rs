//! macOS TCC (Transparency, Consent & Control) permission probing.
//!
//! Capabilities on macOS are gated by per-app, per-user TCC consent, orthogonal
//! to uid — root cannot bypass it. Screen capture and input injection each need
//! their own grant, so they are reported as two independent fields rather than a
//! single privilege bool (which is why this never folds into `is_admin`).
//!
//! Both probes are read-only and non-prompting: `CGPreflightScreenCaptureAccess`
//! queries the screen-recording grant without showing the system prompt, and
//! `AXIsProcessTrusted` (no options) queries the accessibility grant likewise.

use crate::model::info::MacosPermissions;

// Screen Recording grant query. CoreGraphics is already linked by the
// `core-graphics` dependency; the explicit `#[link]` is harmless (frameworks
// dedupe) and documents the requirement.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

// Accessibility (synthetic input / AX) grant query.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Probe the current process's screen-recording and accessibility TCC grants.
pub fn probe() -> MacosPermissions {
    // SAFETY: both functions take no arguments, never block, and only read the
    // TCC database for the calling process.
    let screen_recording = unsafe { CGPreflightScreenCaptureAccess() };
    let accessibility = unsafe { AXIsProcessTrusted() };
    MacosPermissions {
        screen_recording,
        accessibility,
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGRequestScreenCaptureAccess() -> bool;
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

/// Ask macOS to present the system-owned Screen Recording and Accessibility
/// consent flows. The return values are deliberately ignored: `/api/server_info`
/// re-runs the non-prompting preflight and remains the only readiness truth.
pub fn request() {
    // SAFETY: the CoreGraphics call has no arguments and only asks TCC to show
    // its standard consent UI for this process.
    let _ = unsafe { CGRequestScreenCaptureAccess() };

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
}
