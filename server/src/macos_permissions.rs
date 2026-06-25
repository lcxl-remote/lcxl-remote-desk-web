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
