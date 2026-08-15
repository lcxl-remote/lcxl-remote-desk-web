# Privacy Screen & Whiteboard

These two features render locally on the controlled machine and therefore require the **Tauri desktop client** (`tauri-app`).

::: info Under a shared access code
Private screen and whiteboard are subject to the redeemed [access code](/guide/access-codes)'s capability ceiling, the host's global access settings, and live approval when you connect by a device or support code rather than as the owner.
:::

## Privacy Screen

Lock the local display and input to ensure privacy during remote operations — bystanders at the remote machine cannot see the screen or interfere with input while you work.

Privacy-screen settings live under `[desk.private_screen]` in `config.toml`.

Press `Ctrl` + `Alt` + `L` on the controlled machine to leave the privacy screen at any time. The shortcut is handled by the input interception itself, so it works even while every other local key and click is being discarded.

The privacy screen belongs to the controller's signaling-session lifecycle, not to one WebRTC PeerConnection. Replacing the PeerConnection for a wire-codec change therefore keeps the screen covered. Releasing or being denied remote control, explicitly turning the privacy screen off, closing the browser signaling connection, or a host-initiated disconnect removes it. This cleanup is lifecycle-driven and does not depend on a configurable timeout.

::: warning The overlay cannot be checked through a remote view
The overlay is deliberately excluded from screen capture — that is what lets the remote operator keep seeing the real desktop. The exclusion applies to *every* capture path on the host, including macOS Screen Sharing, Apple Remote Desktop and `screencapture`. Looking at the controlled machine through any of them shows the real desktop with no overlay, which is the feature working, not a fault. The only way to confirm the overlay is to look at the machine's physical display.
:::

On macOS the desktop client's own Dock icon disappears while the privacy screen is up and comes back when it is dismissed. This is required, not cosmetic: macOS does not carry the windows of an application that has a Dock icon onto the Space a full-screen application owns, so without it the overlay would silently vanish for as long as anything on the controlled machine is full screen. The tray icon stays throughout.

### Current limitations

- **The overlay covers the primary monitor only.** Input interception is session-wide, so on a multi-monitor machine all local keyboard and mouse input is blocked while the secondary monitors keep showing their real contents. The client logs a warning when it detects more than one attached monitor.
- **macOS: system prompts may end up behind the overlay.** The overlay sits at the screen-saver window level so it can cover the menu bar and the Dock, which also places it above system dialogs such as TCC permission requests and SecurityAgent prompts. If one of those appears while the privacy screen is up, press `Ctrl` + `Alt` + `L` to leave the privacy screen first and then answer the prompt.
- **macOS: the privacy screen requires Accessibility permission.** Without it the input interception cannot start, and the client reports an error instead of showing an overlay that would not actually block anything.

## Remote Whiteboard

Draw and annotate directly on the remote screen for collaboration — useful for guided support and demonstrations.

## Running the Tauri Client

```bash
cd tauri-app
cargo tauri dev
```

See [Quick Start → Tauri Desktop Client](/guide/quick-start#option-2-tauri-desktop-client).
