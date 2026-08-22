# Architecture

A developer-oriented overview of how LCXL Remote Desk is put together. For a gentler introduction, see [Core Concepts](/guide/concepts).

## Connection & Media Path

![Browser-to-device connection and media path](/architecture/connection-path.svg)

The browser and remote device exchange SDP / ICE through the signaling service and use STUN/TURN to gather candidates. They prioritize a direct WebRTC P2P connection and fall back to TURN relay only when NAT traversal fails. Signaling and TURN are built into the server. The terminal is intentionally separate from WebRTC data channels and uses its own authenticated WebSocket.

## Process Model (service-daemon)

![Service-daemon process and IPC model](/architecture/process-model.svg)

The ServiceDaemon (SYSTEM / root) owns the WebRTC connection, signaling, and child processes; it spawns a SessionWorker per desktop session for capture, encoding, input, files, and clipboard. The peer connection lives in the daemon, so workers can restart during user switching without dropping the browser connection.

Daemon and worker use three independent transports: a bidirectional event pipe for signaling and control, a one-way media pipe for encoded audio/video, and a bidirectional file pipe for file commands and chunks. Portable `default` and `desk-server` modes reuse the same logical daemon/worker path with in-process channels.

## AI Diagnostic Pipeline

![AI diagnostics and owner-confirmed execution flow](/architecture/ai-diagnostics.svg)

The central orchestrator runs **collect → redact → model → render** and fails closed on edge redaction. A model may suggest a command, but only the authenticated owner can approve the exact preview; the host receives a sealed plan and revalidates it before execution. The MCP service remains read-only. See the [AI Security Model](/security/ai-security-model).

## Tech Stack

**Backend** — Rust (Edition 2024, 1.90+), Actix-Web 4.11, webrtc-rs 0.17, Actix-Session, Utoipa 5 (OpenAPI), turn 0.17, Prometheus.

**Frontend** — React 19, TailwindCSS + Shadcn UI (Radix), Vite 7, Kubb (OpenAPI → React Query / TS), TypeScript 5.9, xterm.js 5.5, TanStack Query v5.

**Multimedia** — capture via DXGI / WGC (Windows), X11 / Wayland + PipeWire (Linux); encode via X264 / OpenH264 / VP8 / VP9 / AV1; audio via WASAPI / ALSA / PipeWire + Opus.

See the [Module Map](/reference/modules) for crate-level detail.
