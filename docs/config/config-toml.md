# config.toml Reference

Server settings are managed via `conf/config.toml`. The config file path can be overridden with `-c, --config-file-path <PATH>`.

## System `[system]`

- `enable_ipv6` — whether to enable IPv6 support.
- `port` — server listening port.
- `listen_addr_ipv4` — IPv4 listening address.
- `listen_addr_ipv6` — IPv6 listening address.
- `signaling_url` — URL of a standalone signaling server to connect out to (leave empty to use only the embedded signaling server).
- `signaling_token` — node access token for the remote signaling server (passed as `?token=` on the signaling WebSocket).
- `manager_url` — URL of an enterprise manager's signaling endpoint to connect out to.
- `manager_api_token` — access token for the manager (passed as `?token=` on the manager signaling WebSocket).

> When the manager fatally rejects this host's registration (its device limit is reached, or the host has no device identity), the desk-server pauses auto-reconnect and the **Desk Connection** settings page shows a banner explaining why, with a **Retry registration** button. Free a device slot from a control end, then retry.

- `local_signaling_token` — auto-generated, persisted token used by the local desk server (and other hosts) to authenticate with the co-located signaling server. Do not set by hand; it is a credential and is masked in logs.

## Log `[log]`

- `log_level` — logging level (`error`, `warn`, `info`, `debug`, `trace`).
- `traceback` — whether to enable Rust error backtraces.
- `log_retention_days` — log retention in days (default `7`).
- `log_cleanup_threshold_percent` — disk-usage threshold that triggers cleanup (default `90`).
- `log_cleanup_interval_hours` — interval in hours for the cleanup task (default `12`).
- `tokio_console_enabled` — enable the tokio-console subscriber (requires the `tokio_unstable` build flag, default `false`).

## User `[user]`

- `login_user_name` — initial login username.
- `login_password` — initial login password.

## TURN Server `[turn]`

- `realm` — TURN server realm for authentication.
- `interfaces` — network interface configuration (`udp` / `tcp` protocols, listen and external addresses).
- `static_auth_secret` — static authentication secret.
- `enable_stun` / `enable_turn` — toggle STUN and TURN relay respectively.
- `relay_min_port` / `relay_max_port` — relay port allocation range.
- `[turn.static_credentials]` — optional static username / password credential table.

## Desktop `[desk]` {#desktop-desk}

- `video_fps` — video frame rate (default `60`). Lowering reduces CPU and bandwidth usage.
- `video_quality` — video encoding quality (`0`–`63`, lower is better, default `22`).
- `video_encoder` / `audio_encoder` — optional; auto-selected when omitted. Video may be `X264` / `VP8` / `VP9` / `H264` / `AV1`; audio is `OPUS`.
- `video_device_name` — GDI device name of the monitor to capture (`\\.\DISPLAYn`); empty means "ask the browser to pick on first connection".
- `show_mouse` — whether to capture and display the mouse cursor.
- `enable_dirty_rect` — whether to enable dirty-rectangle incremental encoding.
- `[desk.private_screen]` — privacy screen settings (`enabled`, etc.).

## Virtual Display `[virtual_display]` {#virtual-display-virtual-display}

- `enabled` — whether to enable the virtual display (requires an installed IddCx driver; effective only in specific modes).
- `exclusive` / `prompt_ms` / `adaptive_*` — exclusive-mode and adaptive-resolution parameters.

## AI Settings

AI provider, base URL, model, and API key are configured via the **management console**, not the TOML file. API keys are strictly server-side secrets. See [AI Diagnostics](/features/ai-diagnostics).

## Recommended Development Config

```toml
[log]
log_level = "debug"
traceback = true

[desk]
video_fps = 30               # Reduce FPS during development to save resources
```
