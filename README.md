# LCXL Remote Desk Web

[English](README.md) | [中文](README_CN.md)

LCXL Remote Desk Web is an **AI-native**, open-source high-performance remote desktop. It treats AI as a **first-class control plane alongside the browser**: the built-in diagnostic agent can inspect a device and, only after the device owner confirms each exact command, execute approved remediation; external AI assistants receive a separate, permanently **read-only** [MCP](https://modelcontextprotocol.io/) surface. The central signaling service owns model access, orchestration, authorization, and audit, while the controlled device remains a thin evidence-collection and execution edge. The backend is written in Rust; the frontend uses React, Vite, and Tailwind CSS.

> [!WARNING]
> **Disclaimer**: This project is currently in the early development stage. The codebase may be unstable, contain unfixed bugs, or have incomplete features.
> **Security Warning**: Remote desktop technology involves deep access to computer systems. Ensure your network environment is secure when using this project. The author(s) shall not be held liable for any damages arising from the use of this project.

---

## Key Features

- **Central AI Diagnostics**: Ask troubleshooting questions in plain language. The signaling service drives a bounded tool-calling loop, while the controlled device collects only requested evidence and strictly redacts it before transmission. OpenAI-compatible and Anthropic APIs are supported.
- **Owner-Confirmed Command Execution**: The model suggests commands but never executes them directly. The server risk-classifies and blocklist-checks each proposal; after the device owner confirms the exact command, it seals an argv-level plan that the edge re-validates field-for-field before execution. Results are backfilled into the diagnosis and audited.
- **Read-Only MCP Server**: `--startup-mode mcp-stdio` exposes a static four-tool whitelist for system information, processes, listening ports, and policy-gated recent logs. It contains no model call, screenshot, execution, control, or write tool.
- **Capability-Scoped Access**: Device codes can carry per-capability ceilings for remote control, file browsing/transfer/delete, terminal, clipboard, privacy screen, and whiteboard. Host policy and live approval still apply.
- **High-Performance Streaming**: WebRTC video with X264 / OpenH264 / VP8 / VP9 / AV1 software encoders, plus Opus system audio.
- **Remote Terminal**: A built-in xterm.js terminal over a separately authenticated WebSocket.
- **File Management**: Upload, download, delete, and Recycle Bin workflows.
- **Clipboard Sync**: Bidirectional text clipboard synchronization.
- **Remote Whiteboard**: Draw and annotate on the remote screen (requires `tauri-app`).
- **Privacy Screen**: Lock the local display and input during remote operation (requires `tauri-app`).
- **Windows Virtual Display**: An IddCx virtual monitor with adaptive resolution and optional exclusive mode, available in Windows `service-daemon` mode.
- **Cross-Platform Capture**: Windows WASAPI, Linux PipeWire, and macOS ScreenCaptureKit system-audio paths, with desktop capture and input support across Windows, Linux, and macOS.
- **Multi-language Support**: UI and documentation are available in English and Chinese.

---

## Quick Start

### Option 1: Docker Signaling Service

The provided image starts in `signaling` mode. It hosts the web control plane, signaling, and optional TURN relay; desktop capture and input injection still run on controlled devices outside the container.

```bash
docker compose up -d
```

Open `http://localhost:8081` and create the admin account on the first visit.

### Option 2: Tauri Desktop Client

Use this when you need local GUI integrations such as the Privacy Screen or Whiteboard:

```bash
cd tauri-app
cargo tauri dev
```

### Option 3: Run from Source

1. Install the repository-pinned Rust 1.90 toolchain, Node.js 22.16+, and the platform dependencies listed in the [Development Guide](DEVELOPMENT.md).

2. Start the portable backend. `default` embeds both signaling and the controlled-device pipeline:

   ```bash
   cargo run -p lcxl-remote-desk-server --release -- --startup-mode default
   ```

3. Start the frontend in another terminal:

   ```bash
   cd vite-project
   npm ci
   npm run dev
   ```

   Open `http://localhost:5174`.

---

## Configuration

Controlled-device settings use one platform default profile across portable, desk-server, service-daemon, MCP, and local access commands. Use `-c, --config-file-path` only for an explicit profile override; `LRD_*` environment variables can still override individual settings. The local console persists host settings such as:

- **System & Connectivity**: Listen address, ports, local/remote signaling, manager links, logging, and bundled TURN interfaces.
- **Desktop & Encoding**: Display, frame rate, encoder, cursor, audio, and per-session media settings.
- **Host Security**: Per-capability allow / deny / prompt policy, collection policy (`allow_logs`, `allow_screen`), and the maximum locally permitted AI execution risk.
- **Windows Virtual Display**: Driver state, enablement, exclusive mode, and adaptive-resolution controls for `service-daemon` mode.

Model provider settings are different: the central signaling service stores the provider, base URL, model, and write-only API key configured through its console. The browser and controlled edge never receive provider credentials.

`--startup-mode` supports `default`, `signaling`, `desk-server`, `service-daemon`, `session-worker`, and `mcp-stdio`. `session-worker` is an internal child-process mode launched by the daemon.

> See the [Development Guide](DEVELOPMENT.md) for build dependencies and detailed architecture notes.

---

## How It Works

### Connection & Media Path

![Connection and media path](assets/architecture/connection-path.svg)

The browser and controlled device exchange SDP / ICE through signaling and use STUN/TURN to gather candidates. A direct WebRTC peer connection is preferred; TURN is used only when NAT traversal fails. The bundled relay is active only when its listen/public interfaces are configured, and deployments must expose the configured relay ports.

Once connected, WebRTC carries video, Opus audio, and data channels for input, clipboard, file transfer, and whiteboard events. The remote terminal does **not** share those data channels: it opens a separate authenticated WebSocket.

### Process Model

All controlled-device modes use the same logical daemon → PeerConnection manager → worker pipeline. In `default` and `desk-server`, those components run in one process and communicate through in-process channels. In `service-daemon`, the same boundary is split across operating-system processes so desktop work can run in the interactive user session:

![Service-daemon process model](assets/architecture/process-model.svg)

The long-lived ServiceDaemon owns signaling, the WebRTC PeerConnection, and worker lifecycle. A SessionWorker performs capture, encoding, system audio, input injection, clipboard, and file operations inside the desktop session. Three IPC lanes separate traffic: a bidirectional event pipe, a one-way worker-to-daemon media pipe for encoded audio/video frames, and a bidirectional file-transfer pipe. The worker can restart during a user-session switch while the browser's PeerConnection stays online.

Windows currently provides the actual system-service integration. On other platforms, `service-daemon` runs interactively rather than through a native service manager.

---

## AI Architecture

AI inference is centrally orchestrated, while the controlled device is a thin evidence and execution edge. In `default` mode the bundled signaling service supplies that central brain, so the portable build remains self-contained. A standalone `desk-server` connects to an external signaling service or manager for AI orchestration.

![AI diagnostics and owner-confirmed execution flow](assets/architecture/ai-diagnostics.svg)

- **Bounded Agent Loop**: The central orchestrator selects read capabilities, requests evidence, calls the configured model, and may continue through multiple tool turns within configured step and repeat limits.
- **Edge Collection & Redaction**: System information, processes, listening ports, services, logs, and optional screenshots are collected on demand. Logs and screenshots are locally gated; every evidence item is strictly redacted on the edge, and redaction failure blocks the request.
- **Server-Side Model Access**: OpenAI-compatible and Anthropic dialects are supported. API keys remain in the central service and are never sent to the browser or controlled device.
- **Explicit Image Capability**: The provider setting declares whether the configured model accepts image input. The existing provider test automatically switches from the text pong probe to a repository-owned visual marker probe when enabled; diagnosis screenshots fail closed unless permission, local collection policy, and this model capability all agree.
- **Suggest-Only by Default**: A proposed command crosses a separate authorization path. It requires risk and blocklist checks plus per-command owner confirmation; the server seals the approved argv plan, and the edge independently checks its immutable fields and risk ceiling before execution.
- **Privacy-Preserving Audit**: Model calls, approvals, denials, redaction failures, and execution outcomes emit audit metadata and summaries; raw prompts, model responses, stdout, and screenshots are not stored in audit events.

**MCP Server.** `mcp-stdio` is intentionally separate from the diagnostic agent. It exposes exactly `lcxl_system_info`, `lcxl_process_list`, `lcxl_network_ports`, and `lcxl_recent_logs`; the last tool is evaluated against `allow_logs` on every call. The MCP server never calls a model and never exposes screenshots, execution, remote control, or writes.

---

## Project Structure

- **`server`**: The multi-mode Rust binary. It can run the portable all-in-one application, signaling/control plane, controlled-device edge, Windows service daemon, internal worker, or read-only MCP stdio server.
- **`signal`**: Signaling, access grants, the central AI orchestrator/model gateway, authorization, and audit persistence.
- **`diagnose-core` / `agent-protocol`**: Shared model-neutral agent logic and the typed evidence/exec wire contracts.
- **`capture-engine` / `input` / `ipc-protocol`**: Capture and encoding, input injection, and daemon-worker transports.
- **`tauri-app`**: Desktop GUI shell and local integrations such as Privacy Screen and Whiteboard.
- **`vite-project`**: React web console and browser remote-control client.

> See the [Development Guide](DEVELOPMENT.md) for module-level build and platform details.

---

## Roadmap

- [x] High-performance WebRTC streaming
- [x] Cross-platform support (Linux / Windows / macOS)
- [x] Remote terminal and file management
- [x] Capability-scoped access codes
- [x] Privacy screen and whiteboard
- [x] Windows virtual display in service-daemon mode
- [x] Central AI diagnostics (OpenAI-compatible / Anthropic)
- [x] Read-only MCP server integration
- [x] AI command execution with per-command owner confirmation, sealed plans, and edge re-validation
- [ ] Mobile interface optimizations
- [ ] Role-Based Access Control (RBAC) and multi-user management
- [ ] Session recording support

---

## License

This project is licensed under the [Apache-2.0](LICENSE) License.
