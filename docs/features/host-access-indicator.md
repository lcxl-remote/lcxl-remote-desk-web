# Remote Access Indicator

The Tauri desktop client keeps a visible, local status indicator while another
device is accessing this host. It is separate from the approval dialog: an
approval asks whether an operation may start, while the indicator shows what is
actually still active after approval.

## What Is Shown

When activity starts, each attached physical display gets a compact,
always-on-top card in its upper-right corner. The collapsed card distinguishes:

- screen viewing from mouse and keyboard control;
- remote terminal activity;
- file-manager activity;
- uploads and downloads.

Select the card to expand per-connection details. Details include the
server-authenticated display name when available, whether the source is a
signed-in account or temporary grant, a short connection suffix, and file
transfer basenames and byte counts. Full paths, tokens, user IDs, organization
IDs and IP addresses are never shown.

The first active session also produces one system notification. While access is
active, the tray icon carries an amber badge, its tooltip reports the active
session count, and **View Remote Access Status** restores hidden status windows.
The normal tray **Exit** action is blocked so a system-service session cannot
continue after its local indicator has been accidentally closed.

## Accuracy and Cleanup

The host daemon derives the status from execution-side facts:

- screen viewing requires both an established peer connection and negotiated
  video;
- remote control follows the host's accepted control state;
- terminals appear only after the host creates the terminal;
- file browsing and transfers follow typed host-side lifecycle events.

Disconnects, failed peer connections and worker restarts clear the corresponding
state. Restarting or reconnecting the Tauri shell restores the current complete
snapshot instead of reconstructing it from old notifications.

## Local Setting

<code>[system].host_access_indicator_enabled</code> defaults to <code>true</code>.
It is available as **Show remote access status (recommended)** in the
initialization wizard and the host's local System Settings page.

Turning it off requires confirmation and only hides the persistent card, tray
activity badge and first notification. It does **not** change approvals,
permissions, cleanup, or established remote sessions. The host continues to
track activity, so turning the setting back on restores the current state
immediately. This local preference is not exposed through manager remote
settings.

## Displays and Headless Modes

Status windows follow physical-display hot-plug and DPI changes. On Windows, the
LCXL virtual display is identified through its <code>LcxlVirtualDisplay</code>
device identity and excluded. In exclusive virtual-display mode, no status
window is placed on the remote-only virtual display while all physical displays
are detached; it returns when a physical display is reattached.

Explicit headless DeskServer deployments still maintain the authoritative state
and cleanup behavior, but do not promise a local visual indicator because no
Tauri shell is present.
