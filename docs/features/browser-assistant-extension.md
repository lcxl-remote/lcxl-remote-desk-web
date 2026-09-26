# Browser Assistant extension

The LCXL Chrome extension is the only supported browser adapter for AI Assistant. It runs in Chrome on the controlled device and connects to an authenticated loopback bridge. The DevTools MCP adapter, configuration switch and startup path have been removed.

## Pair once

1. In Chrome on the controlled device, open `chrome://extensions`, enable Developer mode, choose **Load unpacked**, and select the repository's `browser-extension` directory.
2. Generate a local one-time proof as described below. In the local OSS AI Assistant page, enter the proof and choose **Show pairing code**. The response is marked `no-store`.
3. Open the extension popup, enter the bridge URL and pairing code, then choose **Pair this browser**. The popup reports when the authenticated bridge is connected.
4. Gmail and Slack are built in. For another HTTPS site, open that site and choose **Allow current site** in the popup. Chrome owns this permission prompt.

Pairing is stored in that Chrome profile. Subsequent browser actions use that paired connection and remain subject to per-operation authorization. Changing Chrome profiles, removing extension storage, or rotating the device data invalidates the pairing.

## Security boundary

The extension accepts only the versioned typed actions advertised by AI Assistant: open or navigate a page, take a bounded accessibility snapshot, wait for an opaque element, fill reviewed fields, upload exact verified artifact bytes, and activate an opaque element under the applicable grant. It does not expose arbitrary JavaScript, raw DOM, cookies, storage, history, network logs, downloads, or native filesystem paths.

Passwords are never projected. Upload bytes are checked against their size and SHA-256 both before crossing the edge bridge and again inside the extension. Page and element references are bound to the Chrome profile, tab, document incarnation, origin, and revision; navigation or reconnection makes stale references fail closed.

Before filling, uploading or activating a referenced control, the extension checks that its role and accessible name still match the observation. If either changed, the old reference is rejected; observe the page again before requesting a new action.

Gmail and Slack draft preparation never activates Send. Exact-send requires a separately sealed `SendExternal` payload and central authorization. The host-local `computer_use.communication_send` ceiling defaults off and is independent of draft handoff. Sending also requires the master switch, browser semantic control and a paired Chrome extension; browser actions are unavailable while the extension is disconnected. The worker rechecks the local sending ceiling before dispatch.

The exact-send receipt check excludes success notices and matching messages that already existed before activation, including hidden notices. It requires a new visible acknowledgement. If the page reuses an old acknowledgement or the result cannot be confirmed, the outcome remains unknown; it must not be treated as successful delivery or retried automatically.

Concurrent requests can share a send result only when their idempotency key, snapshot ID and snapshot digest all match. Receipt storage updates are serialized so one completed send cannot overwrite another concurrent receipt. This receipt cache does not establish the outcome of a send interrupted by a browser crash.

Exact-send also requires an identifiable signed-in account. Gmail uses the account address; Slack requires both workspace and member IDs from the visible account control, consistent with the current workspace URL. Missing or ambiguous identity prevents sending. The extension checks the same identity immediately before activation and while waiting for acknowledgement; an account switch after activation leaves the result unknown.

## Local pairing proof

On the controlled device, run `lcxl-remote-desk-server browser-pairing-proof` as the same ordinary OS user running the host. With a custom configuration, append `--config-file-path <path>`. Enter the returned one-time code in the local Assistant page opened through `localhost` or `127.0.0.1`, then choose **Show pairing code**. The one-time code expires after five minutes; generating a new code invalidates the previous one. The HTTP request requires an owner login, matching local origin, and this OS-user proof. The old GET endpoint does not disclose pairing secrets.

If the bridge is not initialized or its pairing information cannot be read, the request does not consume a valid proof. After resolving the problem, you can submit it again within its original lifetime. Successful redemption remains one-use; retries do not extend the five-minute expiry.

Copy the displayed bridge URL and pairing code into the extension. The port is chosen on first startup and retained across normal restarts. A port conflict makes the browser adapter unavailable without stopping the host; stop the conflicting process before restarting. There is no default port or port scanning. Upgrading from the plaintext bridge requires pairing again. The new connection uses mutual authentication and authenticated encryption for commands and responses.

After opening or navigating a page, the extension checks the current loading state as well as completion events. A completion event missed during setup does not require waiting for the full navigation timeout; an unresponsive state query cannot extend that timeout.

The local pairing panel clears its displayed code after two minutes or when you choose Hide pairing code. This does not revoke the extension pairing. If clipboard access fails, select and copy the code manually.

The extension validates the exact loopback bridge address before saving. Changing either pairing setting closes the previous connection before loading the new settings. Invalid settings leave the bridge disconnected; the extension does not search for another host.

If a page action loses its response, the extension does not resend that action. Check the page before requesting a new action. Re-pairing or losing the bridge prevents pending commands from starting another browser mutation; an operation already submitted to Chrome may still complete.

Bridge commands have a 32-second total deadline. On expiry the connection closes and late continuations cannot submit another action. This does not cancel a browser operation already submitted; its outcome may remain unknown.
