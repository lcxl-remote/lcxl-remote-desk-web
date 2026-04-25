# Service Mode Tauri ↔ Daemon Full IPC Architecture

## Implementation Plan

When `LcxlDeskService` is installed and running, the Tauri app should act as a pure UI shell rather than launching its own embedded HTTP server. Communication between the daemon (SYSTEM account, port 8082) and the Tauri shell happens over a single authenticated WebSocket connection (`/ws/tauri_ipc`).

### Target Architecture

```
Service Daemon (SYSTEM, 127.0.0.1:8082)
  ├── Full HTTP server (all routes + frontend static files)
  ├── /ws/tauri_ipc  ← Tauri-exclusive IPC WebSocket
  ├── signaling_proxy
  └── worker_manager

Tauri App (user session)
  ├── No embedded server
  ├── WS client → ws://127.0.0.1:8082/ws/tauri_ipc?token=<ipc_token>
  │     ├── Recv: PrivateScreen/Whiteboard/SecurityApproval/ServiceOp commands
  │     ├── Send: GUI event responses (approval results, private screen state)
  │     └── Recv auto-login token on connect
  └── Webview → http://127.0.0.1:8082?token=<auto_token>
```

## Task List

- [x] Step 1: Extract `configure_api_routes()` from `lib.rs` for reuse by daemon
- [x] Step 2: Implement `TauriIpcBridge` in `daemon/tauri_ipc.rs` (WS endpoint + protocol messages)
- [x] Step 3: Expand daemon to full HTTP server in `daemon/local_api.rs`
- [x] Step 4: Wire `TauriIpcBridge` into daemon startup (`daemon/mod.rs`)
- [x] Step 5: Add `ipc_client.rs` to Tauri app; add `run_tauri_service_shell()` branch to `lib.rs`
- [x] Step 6: Fix `install_service()` to copy `static/` directory unconditionally
- [x] Step 7: Fix trailing whitespace in `lib.rs`; run `cargo fmt --all` and `cargo test`

## Execution Summary

### Files Modified

| File | Change |
|------|--------|
| `server/src/lib.rs` | Extracted `configure_api_routes()` with individual `web::Data<Sender>` params; fixed duplicate `Arc` import |
| `server/src/daemon/tauri_ipc.rs` | New: `TauriIpcBridge`, `DaemonToTauriMsg`, `TauriToDaemonMsg`, WS session handler, `deny_all_pending_approvals()` on disconnect |
| `server/src/daemon/local_api.rs` | Expanded from stub to full HTTP server; registers all API routes + `/ws/tauri_ipc`; persists cookie signing key; uses `TauriIsAdminOverride` for `is_admin` field |
| `server/src/daemon/mod.rs` | Creates `TauriIpcBridge`, passes `ExternalChannels` and `tauri_login_token` to `run_local_api` |
| `server/src/daemon/windows_service.rs` | Added `copy_dir_recursive()` helper; moved `static/` copy outside `if !same_dir` block so it runs unconditionally on every install |
| `tauri-app/src-tauri/src/ipc_client.rs` | New: async WS client connecting to `/ws/tauri_ipc`; reconnects on disconnect; dispatches daemon commands to local GUI managers; forwards state events back over WS |
| `tauri-app/src-tauri/src/lib.rs` | Added `run_tauri_service_shell()` branch; token-holder pattern for deferred WebView open; simplified tray (show + quit) |
| `tauri-app/src-tauri/Cargo.toml` | Added `awc` and `futures-util` dependencies |

### Key Design Decisions

- **IPC auth**: Persistent `tauri_ipc_token` in settings config (generated once, reused). WS connection carries `?token=` query param; daemon validates with constant-time compare.
- **Security approval response path**: Tauri POSTs approval result to `POST /api/desk/security_approval/submit` (HTTP, not WS) to reuse the existing `PENDING_APPROVALS` global map. No second pending map needed.
- **`is_admin` source**: Tauri reports its own admin status via `Ready { is_admin }` on WS connect. Daemon (SYSTEM) stores it in `TauriIsAdminOverride` and returns it from `/api/server_info`.
- **Disconnect cleanup**: On Tauri WS disconnect, `deny_all_pending_approvals()` unblocks any waiting security approval callers.
- **Cookie signing key**: Persisted to settings on first boot; stable across daemon restarts so user sessions survive upgrades.
- **Portable mode**: `run_tauri_app()` path unchanged; `run_tauri_service_shell()` only activates when `is_service_running(SERVICE_NAME)` returns true.
- **Static file copy**: Runs unconditionally on `--install-service` so re-installs from the same directory always refresh frontend assets. Old `static/` is deleted first to avoid Vite hash file conflicts.
- **Token holder pattern**: `Arc<Mutex<Option<String>>>` bridges the async IPC client (receives token from daemon) and the sync Tauri setup thread (opens WebView URL once token arrives).
