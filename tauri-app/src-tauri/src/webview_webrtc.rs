//! Enable WebRTC in the platform webview where it is gated off by default.
//!
//! Tauri's Windows (WebView2 / Chromium) and macOS (WKWebView) backends expose
//! `RTCPeerConnection` out of the box. Linux WebKitGTK ships WebRTC as an
//! experimental feature whose `enable-webrtc` setting defaults to off, so the
//! desk page's `new RTCPeerConnection(...)` throws
//! `ReferenceError: Can't find variable: RTCPeerConnection`. This module flips
//! that setting on for the windows whose page actually needs it.

use tauri::WebviewWindow;

/// Whether the page at `label` uses WebRTC (`RTCPeerConnection`).
///
/// Only the main desk window does. The overlay windows (`whiteboard`,
/// `private-screen`, `security-approval-*`) render canvas / static / form pages
/// that never create a peer connection.
pub(crate) fn label_hosts_webrtc(label: &str) -> bool {
    label == crate::MAIN_WINDOW_LABEL
}

/// Enable WebRTC on a freshly built window when its page needs it.
///
/// No-op for non-WebRTC windows and on non-Linux platforms (where the webview
/// already exposes WebRTC). Failures are logged, never propagated — the worst
/// case degrades to the pre-existing behaviour (the page's own
/// `RTCPeerConnection` error), so this never introduces a new crash path.
pub(crate) fn enable_webrtc_if_needed(window: &WebviewWindow, label: &str) {
    if !label_hosts_webrtc(label) {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        use webkit2gtk::{SettingsExt, WebViewExt};
        if let Err(e) = window.with_webview(|pw| {
            let webview = pw.inner();
            if let Some(settings) = WebViewExt::settings(&webview) {
                // WebRTC depends on MediaStream being enabled.
                settings.set_enable_media_stream(true);
                settings.set_enable_webrtc(true);
            }
        }) {
            log::error!("[webview] failed to enable WebRTC on WebKitGTK: {e}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = window;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_window_hosts_webrtc() {
        assert!(label_hosts_webrtc(crate::MAIN_WINDOW_LABEL));
    }

    #[test]
    fn overlay_windows_do_not_host_webrtc() {
        assert!(!label_hosts_webrtc("whiteboard"));
        assert!(!label_hosts_webrtc("private-screen"));
        assert!(!label_hosts_webrtc("security-approval-abc123"));
    }
}
