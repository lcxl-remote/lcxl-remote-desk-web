# Browser Assistant extension

The LCXL Chrome extension is the default browser adapter for AI Assistant. It runs in Chrome on the controlled device and connects to an authenticated loopback bridge. Chrome DevTools MCP remains available only as a default-disabled development adapter; LCXL never falls back to it automatically.

## Pair once

1. In Chrome on the controlled device, open `chrome://extensions`, enable Developer mode, choose **Load unpacked**, and select the repository's `browser-extension` directory.
2. In the local OSS AI Assistant page, choose **Show pairing code**. This owner-only response is marked `no-store`.
3. Open the extension popup, enter the bridge URL and pairing code, then choose **Pair this browser**. The popup reports when the authenticated bridge is connected.
4. Gmail and Slack are built in. For another HTTPS site, open that site and choose **Allow current site** in the popup. Chrome owns this permission prompt.

Pairing is stored in that Chrome profile. Ordinary typed browser actions do not ask for the DevTools remote-debugging confirmation again. Changing Chrome profiles, removing extension storage, or rotating the device data invalidates the pairing.

## Security boundary

The extension accepts only the versioned typed actions advertised by AI Assistant: open or navigate a page, take a bounded accessibility snapshot, wait for an opaque element, fill reviewed fields, upload exact verified artifact bytes, and activate an opaque element under the applicable grant. It does not expose arbitrary JavaScript, raw DOM, cookies, storage, history, network logs, downloads, or native filesystem paths.

Passwords are never projected. Upload bytes are checked against their size and SHA-256 both before crossing the edge bridge and again inside the extension. Page and element references are bound to the Chrome profile, tab, document incarnation, origin, and revision; navigation or reconnection makes stale references fail closed.

Gmail and Slack draft preparation never activates Send. A future exact-send path must use a separately sealed `SendExternal` payload and remains unavailable until that path passes its own release gate.
