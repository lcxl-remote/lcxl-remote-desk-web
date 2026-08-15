# Remote Access Indicator

The Tauri desktop client keeps a visible, local status indicator while another
device is accessing this host. It is separate from the approval dialog: an
approval asks whether an operation may start, while the indicator shows what is
actually still active after approval.

## What Is Shown

When activity starts, each attached physical display gets a compact,
non-activating, always-on-top card in its upper-right corner. Showing or
refreshing the card does not take keyboard focus from the local user's active
application. The collapsed card distinguishes:

- screen viewing from mouse and keyboard control;
- active host-system audio capture;
- remote terminal activity;
- file-manager activity;
- uploads and downloads.

The transparent margin follows the card's rounded outline instead of drawing an
opaque rectangular window. Drag the handle at the top of the card to move it
away from content underneath. Each card remains bound to its physical display:
the shell preserves positions within that display and clamps a card to the
nearest edge instead of allowing it to move onto another display.

Select the card to expand per-connection details. Details include the
server-authenticated display name when available, whether the source is a
signed-in account or temporary grant, a short connection suffix, and file
transfer basenames and byte counts. Full paths, tokens, user IDs, organization
IDs and IP addresses are never shown.

The first active session also produces one system notification. While access is
active, the tray icon carries an amber badge, its tooltip reports the active
session count, and **View Remote Access Status** restores hidden status windows.
The normal tray **Exit** action requires a native confirmation while access is
active or the host is locked. Exiting closes only the UI and never unlocks the
daemon. See [Disconnect and Lock Remote Access](./remote-access-lock.md).

## Accuracy and Cleanup

The host daemon derives the status from execution-side facts:

- screen viewing requires both an established peer connection and negotiated
  video;
- system audio appears only while the daemon output fence is open for the
  current connection and audio generation;
- remote control follows the host's accepted control state;
- terminals appear only after the host creates the terminal;
- file browsing and transfers follow typed host-side lifecycle events.

A replacement peer connection keeps the logical screen-view session in the
indicator, avoiding a false stop/start notification, while system-audio and
remote-control badges clear until their new pipelines or authorization become
active. A signaling disconnect, host-initiated disconnect, or worker restart
clears the corresponding logical state. Restarting or reconnecting the Tauri
shell restores the current complete snapshot instead of reconstructing it from
old notifications.

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

The system-audio badge follows this same visibility switch; it is not a separate
always-on window. The independent **System audio capture** Allow/Prompt/Deny
permission remains enforced even when the indicator is hidden.

## Displays and Headless Modes

Status windows follow physical-display hot-plug and DPI changes. On Windows, the
LCXL virtual display is identified through its <code>LcxlVirtualDisplay</code>
device identity and excluded. In exclusive virtual-display mode, no status
window is placed on the remote-only virtual display while all physical displays
are detached; it returns when a physical display is reattached.

Explicit headless DeskServer deployments still maintain the authoritative state
and cleanup behavior, but do not promise a local visual indicator because no
Tauri shell is present.
