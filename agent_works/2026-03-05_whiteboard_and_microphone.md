# Whiteboard & Microphone Feature Implementation

## Overview

Full-stack implementation of remote desktop whiteboard and browser microphone audio push features.

- **Whiteboard**: Draw and add text on the remote screen via the browser Canvas, transmitted through WebRTC DataChannel to a transparent Tauri overlay window on the controlled machine.
- **Microphone**: Real-time browser microphone audio capture via WebRTC AudioTrack, decoded from Opus via opusic-c on the server, and played on the remote desktop via cpal.

## Architecture

### Whiteboard Data Flow
```
Browser Canvas → Normalized coordinates (0~1) → JSON → DataChannel(whiteboard_event)
  → server whiteboard_event.rs → WhiteboardCommand::DrawMessage → mpsc channel
  → tauri WhiteboardManager → emit("whiteboard-draw") → overlay webview
  → whiteboard-page.tsx → Canvas rendering
```

### Audio Data Flow
```
Browser getUserMedia → replaceTrack(sendrecv transceiver)
  → WebRTC AudioTrack → server on_track → audio_playback.rs
  → Opus decode (opusic-c) → ringbuf (60ms pre-buffer) → cpal output device
```

## Modified Files

### signal-facade
- **[NEW]** `model/whiteboard.rs` — Whiteboard message data structures
- **[MOD]** `model/model.rs` — Register whiteboard module
- **[MOD]** `model/signal.rs` — `InitSignalingData.has_tauri: bool`

### server
- **[MOD]** `model/data_channel.rs` — `DATA_CHANNEL_LABEL_WHITEBOARD_EVENT`
- **[MOD]** `model/system_setting.rs` — `WhiteboardCommand` enum
- **[MOD]** `lib.rs` — `ExternalChannels.whiteboard_cmd_sender`
- **[NEW]** `service/whiteboard_event.rs` — DataChannel handler → mpsc forwarding
- **[NEW]** `service/audio_playback.rs` — RTP → Opus decode → cpal playback with ring buffer
- **[MOD]** `service/data_channel.rs` — Whiteboard dispatch + sender parameter
- **[MOD]** `service/signaling.rs` — DeskSession whiteboard sender, has_tauri, on_track
- **[MOD]** `service.rs` — Module registration
- **[MOD]** `Cargo.toml` — Added cpal 0.15, ringbuf 0.4

### tauri-app
- **[NEW]** `src/whiteboard.rs` — WhiteboardManager (transparent overlay WebviewWindow)
- **[MOD]** `src/lib.rs` — Register whiteboard module and channels

### frontend (vite-project)
- **[NEW]** `use-desk-whiteboard.ts` — Whiteboard hook (drawing state, coordinate normalization, DataChannel messaging)
- **[NEW]** `whiteboard-canvas.tsx` — Canvas overlay component
- **[NEW]** `whiteboard-toolbar.tsx` — Floating toolbar (pen/text/color/width/undo/clear)
- **[NEW]** `whiteboard-page.tsx` — Tauri overlay webview page (listens via window.__TAURI__)
- **[NEW]** `use-desk-microphone.ts` — Microphone hook (getUserMedia + replaceTrack)
- **[MOD]** `use-desk-rtc.ts` — whiteboardChannel + peerConnection export
- **[MOD]** `desk-session.tsx` — Whiteboard button (PenTool) + Canvas overlay + Mic button
- **[MOD]** `router.tsx` — /whiteboard route

## Key Design Decisions

1. **has_tauri Detection**: Frontend checks `initData.has_tauri` to enable/disable whiteboard button. The server determines this based on whether `whiteboard_cmd_sender` is present in `ExternalChannels`.

2. **Coordinate Normalization**: All drawing coordinates are normalized to 0.0~1.0 range relative to video content, ensuring correct rendering regardless of window size differences.

3. **Audio Pre-buffering**: ~60ms pre-buffer before starting cpal playback to mitigate jitter. Ring buffer (ringbuf crate) provides lock-free inter-thread audio transfer.

4. **Tauri Event Access**: Instead of npm-installing `@tauri-apps/api`, the whiteboard page uses `window.__TAURI__.event.listen()` at runtime, avoiding a hard dependency.

5. **Audio Transceiver Reuse**: The existing `sendrecv` audio transceiver is reused via `replaceTrack()` rather than creating a new one, avoiding renegotiation.

## Verification

- ✅ `cargo check --package lcxl-remote-desk-server` — zero errors
- ✅ `npx tsc --noEmit` — zero TypeScript errors

## Pending Testing

- [ ] End-to-end whiteboard test (browser drawing → overlay display)
- [ ] End-to-end microphone test (browser capture → remote playback)
- [ ] Whiteboard button correctly disabled when no Tauri
- [ ] Cross-platform audio playback (Windows/Linux/macOS)
