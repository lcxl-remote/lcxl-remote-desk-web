# Module Map

The Rust workspace is split into focused crates plus the frontend and Tauri shell.

| Module | Role |
|---|---|
| `server/` | Desk server: REST API (Actix-Web), WebRTC, settings, file/terminal management (supports ServiceDaemon and SessionWorker modes). The AI diagnostic orchestrator lives in `server/src/diagnose/` (collect → redact → model → render), with model adapters in `server/src/diagnose/model/` (`openai.rs` / `anthropic.rs`). |
| `signal/` | Signaling server + TURN. |
| `signal-facade/` | Shared signaling protocol models. |
| `turn/` | TURN / STUN service (bundled with the signaling server). |
| `vite-project/` | React 19 + TanStack Query frontend — management UI and the web control client (includes the AI settings page and diagnostic panel). |
| `tauri-app/` | Tauri shell for locally-rendered Privacy Screen / Whiteboard on the controlled machine. |
| `agent-protocol/` | Device capability protocol (`desk-agent-protocol`): wire types + `DeviceAgent` trait + audit / diagnose / exec protocol. Pure protocol, no platform impl; the server is the sole source of truth for all trusted fields. |
| `mcp-server/` | Read-only MCP service (`desk-mcp-server`): `rmcp` SDK + stdio, a static whitelist of read-only tools (no exec/write/control). |
| `capture-engine/` | Screen / audio capture and encoding. |
| `input-injection/` | Mouse / keyboard injection and clipboard control. |
| `ipc-protocol/` | IPC message definitions for daemon ↔ worker. |
| `virtual-display/` | Virtual display (IddCx) userspace abstraction (`desk-virtual-display`). |
| `virtual-display-driver-ops/` | Virtual display driver install / uninstall wrapper. |
| `server-user/` | Server-side user / account models. |
| `utils/` | Common utilities. |
| `server-version/` | API version constants. |

## Adding a New REST API

1. Define models in `server/src/model/`.
2. Implement logic in `server/src/service/`.
3. Add route handlers with `utoipa` annotations in `server/src/controller/`.
4. Register routes in `server/src/main.rs`.
5. Run the OpenAPI update script to regenerate the frontend client.

## Adding a New Signaling Type

1. Add a new variant (with a unique integer value) to `SignalingType` in `signal-facade/src/model/signal.rs`.
2. Handle it in `handle_message` in `signal/src/service.rs` — add a forwarding branch or a dedicated match arm. Never add a `_ =>` catch-all (exhaustiveness is compiler-enforced).
3. Update the frontend: regenerate the client, then add an `onMessage` handler in the frontend RTC hook.
