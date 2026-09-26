# Privacy Screen & Whiteboard

These two features render locally on the controlled machine and therefore require the **Tauri desktop client** (`tauri-app`).

::: info Under a shared access code
Private screen and whiteboard are subject to the redeemed [access code](/guide/access-codes)'s capability ceiling, the host's global access settings, and live approval when you connect by a device or support code rather than as the owner.
:::

## Privacy Screen

The privacy screen provides platform-specific display concealment and local input blocking during remote operations. Linux currently uses best-effort input blocking; GNOME Wayland display concealment is not verified. See the Linux limitations below.

Privacy-screen settings live under `[desk.private_screen]` in `config.toml`.

Press `Ctrl` + `Alt` + `L` on the controlled machine to leave the privacy screen at any time. The shortcut is handled by the input interception itself, so it works even while every other local key and click is being discarded.

The privacy screen belongs to the controller's signaling-session lifecycle, not to one WebRTC PeerConnection. Replacing the PeerConnection for a wire-codec change therefore keeps the screen covered. Releasing or being denied remote control, explicitly turning the privacy screen off, closing the browser signaling connection, or a host-initiated disconnect removes it. This cleanup is lifecycle-driven. Linux additionally limits each activation to five minutes and releases its device handles on expiry.

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

### Linux input blocking

Linux attempts to block the keyboard and pointer devices present when activation starts. It requires existing access to those event devices and never changes device permissions automatically. The interceptor handles `Ctrl` + `Alt` + `L` directly. Activation fails if it cannot acquire an escape-capable keyboard, or if some devices failed to open or block; the privacy interface does not claim partial coverage. Newly connected devices are not covered. The native blocker releases its handles on cancellation, input failure or the five-minute limit. This is best-effort input blocking, not proof of GNOME Wayland screen concealment or complete isolation from other software input.
