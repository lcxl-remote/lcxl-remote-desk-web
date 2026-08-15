# Remote Control & Streaming

LCXL Remote Desk streams the remote screen and audio over **WebRTC** for ultra-low latency, with full keyboard and mouse control.

## Video Encoding

The capture engine supports software and hardware encoding across multiple codecs:

| Codec | Backend |
|---|---|
| H.264 | X264 / OpenH264 |
| VP8 / VP9 | libvpx |
| AV1 | SVT-AV1 |

Capture backends: Windows (**DXGI / WGC**), Linux (**X11 / Wayland portal + PipeWire**). Linux backends are filtered by the active desktop session: native X11 advertises X11, while Wayland advertises only the ScreenCast portal so XWayland is not mistaken for full-desktop capture.

The controller resolves the capture backend, display, encoder, frame rate, quality, cursor and dirty-rectangle choices for each remote-desktop connection. These choices no longer overwrite the host's configuration file. A submitted change takes effect without reloading the page: the host either applies it live, rebuilds only the affected media pipeline, or automatically establishes a replacement WebRTC session when the wire codec changes. The dialog reports the actual effect. If the controller held remote control before a replacement session, it requests control again after the new PeerConnection is connected; authorization is never copied across session epochs, so an ask policy still prompts the host again.

Before replacing a PeerConnection, the controller releases held keys and mouse buttons. Clipboard and whiteboard interaction pause until control is accepted again, and an enabled controller microphone is attached to the replacement audio transceiver without asking for capture permission again. Host-system audio is interrupted and restarted with the new media session. An attached virtual display stays attached across the handoff, but exclusive mode exits while control is unapproved so local approval UI remains visible; it may enter again after the fresh control grant.

When the encoder is set to **Auto**, the host selects the first installed encoder whose declared limits accept the selected display resolution and whose codec the controller can decode. The order is stable and runtime-probe-only encoders are not selected automatically. When an encoder is selected explicitly, that choice is strict: the host does not silently substitute another implementation or codec.

### Resolution changes and encoder limits

If the selected encoder cannot accept the current capture dimensions, the host stops before encoding instead of producing an empty video or repeatedly retrying every frame. The browser shows the current resolution, compatible encoders when known, and actions to choose another encoder or retry after changing the host display mode. A mid-session resolution change uses the same check; returning to a supported mode requires **Retry video** unless the platform reports the mode transition automatically.

No CPU-side frame scaling is performed. This avoids the high memory-bandwidth cost of scaling 4K BGRA frames and keeps the captured desktop geometry exact.

## Audio

System audio is captured and encoded with **Opus** (libopus). The Web controller explicitly requests stereo Opus reception; audible channel separation still depends on the captured source and the controller's output device. Capture backends: Windows (**WASAPI**), Linux (**ALSA / PipeWire**).

On a capable desktop host, a new browser preference starts with **Capture system audio** enabled. This is only the controller's request: the host has an independent **System audio capture** permission with Allow, Prompt and Deny behavior, and a temporary support grant may further restrict it. Denial or timeout leaves video, input and microphone uplink available while host-system audio remains off. Android hosts currently report system-audio capture as unsupported.

The controller shows the actual audio state (starting, active, denied, failed or off). The host's existing remote-access status card adds a system-audio badge only while audio is really passing its daemon output gate. Audio is WebRTC media and is not recorded or persisted by this feature. Host-system audio and controller-microphone uplink are separate features.

## Browser preferences

For signed-in owner access, the Web controller saves device settings in this browser under the controller account plus target device. Personal and organization views therefore reuse the same preference for the same account and device. The two adaptive encoder toggles are saved once per controller account. Preferences do not sync to another browser and are removed when site data is cleared. Temporary support-code/grant sessions never read or write the owner's saved preferences.

## Input

Mouse and keyboard input are injected on the controlled device over a dedicated data channel. In service-daemon mode, injection runs inside the user's desktop session — see [Startup Modes](/guide/startup-modes).

## Prepare the controlled host

The controlled host's home page shows the local actions required before it can accept a desktop session. Windows keeps the **Install service** action there; macOS reports Screen Recording and Accessibility separately and can open their system permission prompts from the same area.

On a logged-in Wayland desktop, click **Enable Wayland remote access** locally before connecting from another device. The requested scope follows the host's input mode:

- `none` and `uinput` request screen sharing only;
- `portal` and Wayland `auto` request screen sharing plus keyboard and pointer control.

The Portal picker is owned by the local desktop. A remote controller cannot open or accept it. Until the required screen/input components are ready, a remote request fails immediately with an instruction to prepare the host; it does not wait for the picker or enter WebRTC/ICE retry.

The host keeps the selected Portal session alive between remote connections. Where the desktop supports persistent restore and the packaged application has a stable identity, the next application launch attempts to restore it; otherwise the home page states that authorization lasts only for the current run. Revoking permission, restarting the Portal backend, or losing the selected source returns the host to **Needs authorization**.

The long-lived session keeps the monitor chosen in the Portal picker. Both **Enable input control** and **Change shared screen / Reauthorize** create a replacement Portal session, reopen the local system prompt, and stop active media/input pipelines first. Reconnect after authorization. No remote connection can trigger either action or upgrade screen-only authorization implicitly.

Wayland support here covers an already logged-in graphical user session. It does not cover GDM, a login screen, a headless host, or unattended operation without a graphical user session.

## Monitor Selection

`desk.video_device_name` selects which monitor to capture (e.g. `\\.\DISPLAY1` on Windows). On first connection, the browser automatically selects the first capture mode that has a usable display and then prefers the display at the desktop origin. If a saved mode or display is no longer available, it is corrected to that usable default and the dialog explains the change.

## Tuning Tips

- Lower `video_fps` (e.g. 30) to reduce CPU and bandwidth.
- Raise `video_quality` number for smaller bitrate, lower for better picture.
- Enable `enable_dirty_rect` for largely static screens.
