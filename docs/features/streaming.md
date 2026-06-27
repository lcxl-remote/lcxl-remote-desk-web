# Remote Control & Streaming

LCXL Remote Desk streams the remote screen and audio over **WebRTC** for ultra-low latency, with full keyboard and mouse control.

## Video Encoding

The capture engine supports software and hardware encoding across multiple codecs:

| Codec | Backend |
|---|---|
| H.264 | X264 / OpenH264 |
| VP8 / VP9 | libvpx |
| AV1 | SVT-AV1 |

Capture backends: Windows (**DXGI / WGC**), Linux (**X11 / Wayland portal + PipeWire**).

The encoder can be auto-selected or pinned via `desk.video_encoder`. Frame rate (`video_fps`), quality (`video_quality`, 0–63, lower is better), and dirty-rectangle incremental encoding (`enable_dirty_rect`) are all configurable — see the [config.toml Reference](/config/config-toml#desktop-desk).

The configured encoder is a **preference**, not an absolute choice: when a client (browser, Android, or iOS) connects, the host negotiates the actual video codec against the codecs that client advertises it can decode in its WebRTC offer. The preferred encoder is honoured when the client can decode it; otherwise the host automatically falls back to the best codec both sides support. This means a client is never sent a codec it cannot decode — no per-client capability configuration is required.

## Audio

System audio is captured and encoded with **Opus** (libopus). Capture backends: Windows (**WASAPI**), Linux (**ALSA / PipeWire**).

## Input

Mouse and keyboard input are injected on the controlled device over a dedicated data channel. In service-daemon mode, injection runs inside the user's desktop session — see [Startup Modes](/guide/startup-modes).

## Monitor Selection

`desk.video_device_name` selects which monitor to capture (e.g. `\\.\DISPLAY1` on Windows). An empty value asks the browser to pick on first connection.

## Tuning Tips

- Lower `video_fps` (e.g. 30) to reduce CPU and bandwidth.
- Raise `video_quality` number for smaller bitrate, lower for better picture.
- Enable `enable_dirty_rect` for largely static screens.
