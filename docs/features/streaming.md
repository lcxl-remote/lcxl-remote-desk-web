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

The encoder can be auto-selected or pinned via `desk.video_encoder`. Frame rate (`video_fps`), quality (`video_quality`, 0–63, lower is better), and dirty-rectangle incremental encoding (`enable_dirty_rect`) are all configurable — see the [config.toml Reference](/config/config-toml#desktop-desk).

When the encoder is set to **Auto**, the host selects the first installed encoder whose declared limits accept the selected display resolution and whose codec the controller can decode. The order is stable and runtime-probe-only encoders are not selected automatically. When an encoder is selected explicitly, that choice is strict: the host does not silently substitute another implementation or codec.

### Resolution changes and encoder limits

If the selected encoder cannot accept the current capture dimensions, the host stops before encoding instead of producing an empty video or repeatedly retrying every frame. The browser shows the current resolution, compatible encoders when known, and actions to choose another encoder or retry after changing the host display mode. A mid-session resolution change uses the same check; returning to a supported mode requires **Retry video** unless the platform reports the mode transition automatically.

No CPU-side frame scaling is performed. This avoids the high memory-bandwidth cost of scaling 4K BGRA frames and keeps the captured desktop geometry exact.

## Audio

System audio is captured and encoded with **Opus** (libopus). The Web controller explicitly requests stereo Opus reception; audible channel separation still depends on the captured source and the controller's output device. Capture backends: Windows (**WASAPI**), Linux (**ALSA / PipeWire**).

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
