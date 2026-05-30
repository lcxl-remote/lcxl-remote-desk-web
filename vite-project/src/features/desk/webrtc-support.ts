/**
 * Whether the current webview / browser exposes the WebRTC API.
 *
 * Tauri's Windows (WebView2) and macOS (WKWebView) webviews always provide it,
 * but a Linux WebKitGTK build with WebRTC disabled leaves `RTCPeerConnection`
 * undefined. Callers should check this before constructing a peer connection so
 * they can surface a clear message instead of throwing an unhandled rejection.
 */
export function isWebRtcAvailable(): boolean {
    return typeof RTCPeerConnection !== "undefined"
}
