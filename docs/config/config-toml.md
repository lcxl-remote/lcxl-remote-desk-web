# config.toml Reference

Server settings use a startup-mode-independent platform default: `%ProgramData%\LCXL Remote Desktop\config\config.toml` on Windows, `XDG_CONFIG_HOME/lcxl-remote-desk/config.toml` (or `~/.config/lcxl-remote-desk/config.toml`) for a non-root Linux user, `/etc/lcxl-remote-desk/config.toml` for Linux root, and `~/Library/Application Support/com.lcxl.remote-desk/config/config.toml` on macOS. `-c, --config-file-path <PATH>` explicitly selects another profile; services and LaunchAgents inherit it only when it was explicitly supplied.

## System `[system]`

- `enable_ipv6` — whether to enable IPv6 support.
- `port` — server listening port.
- `listen_addr_ipv4` — IPv4 listening address.
- `listen_addr_ipv6` — IPv6 listening address.
- `signaling_url` — URL of a standalone signaling server to connect out to (leave empty to use only the embedded signaling server).
- `signaling_token` — node access token for the remote signaling server (passed as `?token=` on the signaling WebSocket).
- `manager_url` — URL of an enterprise manager's signaling endpoint to connect out to.
- `manager_api_token` — access token for the manager (passed as `?token=` on the manager signaling WebSocket).
- `manager_enabled` — whether to keep the manager connection active. Leave unset (or `true`) to connect; set to `false` to disable the manager link **without clearing** `manager_url` / `manager_api_token`, so the address is retained for a later re-enable. Toggled from the **Desk Connection** settings page; this is a host-local switch (the manager cannot turn its own link off).
- `require_secure_signaling` — whether this host refuses to dial a **public** signaling server / manager over a plaintext scheme (`ws://` / `http://`). Leave unset (or `true`, the secure default) to enforce; set to `false` only as a deliberate escape hatch for a trusted-network deployment that runs a public endpoint without TLS. Loopback and private / LAN targets (a self-hosted signaling server on `192.168.x.x`, `127.0.0.1`, etc.) are always reachable over plaintext regardless of this switch, and the cloud-metadata address range is always blocked. Enforcement runs at connect time on the resolved IP, so a domain that resolves to a public address cannot bypass it. Toggled from the **Desk Connection** settings page. An omitted field fails secure to `true`.

> When the manager fatally rejects this host's registration (its device limit is reached, or the host has no device identity), the desk-server pauses auto-reconnect and the **Desk Connection** settings page shows a banner explaining why, with a **Retry registration** button. Free a device slot from a control end, then retry.

- `local_signaling_token` — auto-generated, persisted token used by the local desk server (and other hosts) to authenticate with the co-located signaling server. Do not set by hand; it is a credential and is masked in logs.

### Telemetry consent

`telemetry_consent` is owned by the dedicated **Telemetry** card rather than the
general system-settings form. Changing ports, addresses, IPv6 or auto-start
cannot change or clear it. A consent choice is persisted immediately, but the
OpenTelemetry exporters/layers are created at process startup; restart the
relevant server, daemon and worker processes before describing the new choice as
active at runtime.

### Local remote-access presentation

<code>host_access_indicator_enabled</code> controls whether the Tauri host shows
its persistent remote-access card, activity tray badge and first-session
notification. It defaults to <code>true</code>. Turning it off only hides these
indicators; it does not change approvals, permissions or established sessions.
This host-local preference is not remotely configurable by manager. See
[Remote Access Indicator](/features/host-access-indicator).

## Log `[log]`

- `log_level` — logging level (`error`, `warn`, `info`, `debug`, `trace`).
- `traceback` — whether to enable Rust error backtraces.
- `log_retention_days` — log retention in days (default `7`). `0` disables cleanup.
- `log_cleanup_threshold_percent` — disk-usage threshold that triggers cleanup (default `90`). Once the disk is fuller than this, rolled files inside the retention window are deleted oldest-first until usage drops back under it; today's and yesterday's files are always kept, because an appender may still hold them open. `0` disables this stage, leaving retention as the only criterion.
- `log_cleanup_interval_hours` — interval in hours for the cleanup task (default `12`). `0` disables cleanup.
- `tokio_console_enabled` — enable the tokio-console subscriber (requires the `tokio_unstable` build flag, default `false`).

Cleanup covers every component's rolled files in the log directory
(`desk-server.log.<date>`, `desk-daemon.log.<date>`, `desk-worker.log.<date>`,
`desk-mcp.log.<date>`, `desk-tauri.log.<date>`); files that belong to other
programs sharing the directory are left alone. Every file carries a date suffix,
including the one being written right now — retention is measured from it, so the
live file is never removed. Roll dates are UTC.

Logging never blocks the service: stdout and the log file are both written
through a dedicated writer thread behind a bounded, lossy queue. If a sink stops
draining — a full or hung disk, a console that stopped servicing writes for an
inactive desktop — records are dropped rather than backpressured onto the caller,
so capture, input injection and signaling keep running.

The sweep runs in the process that owns the log directory: the daemon, or the
server in a portable / signaling-only run. Session workers and the MCP stdio
helper never sweep — they are short-lived and hold a startup snapshot of the
settings, so a change to the values above would not reach them.

## User `[user]`

- `login_user_name` — initial login username.
- `login_password` — initial login password.

## TURN Server `[turn]`

- `realm` — TURN server realm for authentication.
- `interfaces` — the addresses this host relays on (see [Interface addresses](#interface-addresses)).
- `static_auth_secret` — static authentication secret.
- `enable_turn` — run the TURN service on this host (default `true`). One service provides both TURN relay and STUN, so turning it off stops both; connections then rely on whatever relay the signaling server offers.
- `relay_min_port` / `relay_max_port` — relay port allocation range.
- `[turn.static_credentials]` — optional static username / password credential table.

The TURN service follows these settings while the server runs: saving the TURN
settings page (or regenerating the secret) restarts the service immediately, with
no server restart. **A restart drops the connections currently being relayed** —
they reconnect through whatever candidates ICE can still find. Sessions that are
not going through this relay are unaffected.

With `enable_turn = true` but no `interfaces` configured, nothing is started (the
service has no address to serve on); the runtime status endpoint reports this as
`not-configured`, which is deliberately distinct from having switched the service
off.

### Interface addresses

Each entry has a `transport`, a `listen` address (what this host binds) and an
`external` address (what peers are told to dial):

```toml
[[turn.interfaces]]
transport = "udp"
listen = "0.0.0.0:3478"
external = "203.0.113.7:3478"

[[turn.interfaces]]          # IPv6 literals need their brackets
transport = "udp"
listen = "[::]:3478"
external = "[2001:db8::1]:3478"
```

Both addresses are `IP:port` pairs. Host names are **not** resolved — write the
address itself. An `external` value additionally has to be something a peer can
dial, so a wildcard address (`0.0.0.0`, `::`) or a zero port is rejected.

Only UDP is relayed. A `tcp` entry is neither listened on nor advertised, rather
than advertised and left unanswered.

An entry that fails any of these is reported and skipped; the remaining entries
are served normally. Each rejection is logged at startup and listed in the
runtime status endpoint's `rejected_interfaces`, naming the entry, the field and
what a working value looks like. If **every** entry is rejected there is nothing
to serve on, so the status is `not-configured` — with the rejections attached,
which is what distinguishes it from having configured no interfaces at all.

The **TURN settings** page shows the same information as a runtime status card:
whether the service is running, the interfaces it is actually serving, why it is
not running when it is not, and any entry that was refused. The card is what to
read after saving — the form shows the configuration, the card shows what became
of it.

A save returns before the ports are bound, so immediately afterwards the card
reads `starting` rather than running — that is the normal path, not a failure,
and only a state with a cause to report is called one. The card re-reads itself
while the host is still settling, so it becomes running, or names what went
wrong, without being refreshed by hand.

The page also has a collapsed **Advanced statistics lookup**. Given a known
client `IP:port` and interface, it shows relay and control bytes/packets for that
address and distinguishes a stopped runtime from an address with no record.
This TURN implementation cannot enumerate all sessions or force-close one, so
`/api/turn/session/statistics` is the only session endpoint the API exposes —
a per-address lookup, with no list or teardown counterpart.

## Desktop `[desk]` {#desktop-desk}

- `video_fps` — video frame rate (default `60`). Lowering reduces CPU and bandwidth usage.
- `video_quality` — video encoding quality (`0`–`63`, lower is better, default `22`).
- `video_encoder` / `audio_encoder` — optional; auto-selected when omitted. Video may be `X264` / `VP8` / `VP9` / `H264` / `AV1`; audio is `Opus`.
- `video_device_name` — GDI device name of the monitor to capture (`\\.\DISPLAYn`); empty means "ask the browser to pick on first connection".
- `show_mouse` — whether to capture and display the mouse cursor.
- `enable_dirty_rect` — whether to enable dirty-rectangle incremental encoding.
- `[desk.private_screen]` — privacy screen settings (`enabled`, etc.).

## Virtual Display `[virtual_display]` {#virtual-display-virtual-display}

- `enabled` — whether to enable the virtual display (requires an installed IddCx driver; effective only in specific modes).
- `exclusive` / `prompt_ms` / `adaptive_*` — exclusive-mode and adaptive-resolution parameters.

## Security `[security]`

Per-capability access control for inbound remote sessions. Each capability is
tri-state: unset means "ask the local user each time" (the file default), `true`
means "always allow", `false` means "always deny".

The onboarding wizard writes an explicit posture at install time (opening
capabilities for the owner), so a wizard-installed host does not start from the
all-ask file default. For a non-owner session redeemed via an
[access code](/guide/access-codes), these global settings are additionally met
with that code's capability ceiling and live approval.

- `allow_remote_control` — mouse / keyboard input.
- `allow_clipboard_sync` — clipboard synchronization.
- `allow_system_audio_capture` — capture and transmit the host's system audio.
- `allow_private_screen` — private (privacy) screen mode.
- `allow_whiteboard` — whiteboard overlay.
- `allow_terminal` — remote terminal access.
- `allow_file_browse` — directory listing and file metadata.
- `allow_file_delete` — file deletion; a delete also requires `allow_file_browse`.
- `allow_file_transfer` — file upload / download.
- `approval_timeout` — how long an approval prompt waits, in seconds. **Default `30`** — after which the host **server authoritatively cancels (denies) the request** rather than leaving it pending forever, enforced server-side even if the approval UI is closed or unreachable. Set to `0` to never time out (the prompt waits indefinitely). "Never" is stored as the value `0`, so it survives a restart.

### When a change takes effect

A capability change applies to sessions that are already running — there is no
need to disconnect the controller or restart the host. The new setting is in
force from that connection's **next** request for the capability.

What it does *not* do is retract an ability the controller already holds.
Setting `allow_remote_control` to `false` while someone is controlling the
desktop stops the next control request, but does not end the control they were
granted; the same goes for a transfer already in progress. To cut a session off
immediately, disconnect it (or use
[remote access lock](/features/remote-access-lock), which cancels everything
outright).

A "remember my choice" answer given while the setting was being changed is
discarded rather than applied — the change you just made stands, and the request
the user answered is still honored on its own.

## OSS global configuration

OSS saves AI gateways, search and global policies in the same `config.toml` as host settings, selected by the active profile or `--config-file-path`. Settings pages persist changes immediately. To edit the file manually, stop the service, edit, and restart. These nine sections and `[oss_config_metadata]` do not accept `LRD_*` overrides; other host settings keep their existing environment behavior.

| Section | Fields and meaning |
| --- | --- |
| `[ai_gateway]` | `wire_protocol`, `base_url`, `model`, `api_key`, `supports_image_input`; `profile_schema_version`, `reasoning_contract`, `anthropic_prefix_binding`, `request_options_json`, `output_limit_field`, `runtime_max_output_tokens`, `max_context_bytes`; `response_format`, `execution_mode`, `max_steps_per_turn`, `max_same_tool_calls_per_turn`, `exec_approval_timeout_secs` (seconds) |
| `[approval_gateway]` | `enabled`; its independent model settings are in `[approval_gateway.gateway]`, with the same fields as the main gateway. OSS has no price, currency or accounting settings |
| `[web_search]` | `schema_version`, `provider` (`duck_duck_go`, `brave`, `tavily`), optional `api_key` |
| `[context_management]` | `schema_version`, `strategy` (`window`, `checkpoint_summary`) |
| `[subagent_policy.limits]` | `maxUnfinishedPerRoot`: maximum unfinished subagents for each root conversation |
| `[goal_budget_policy]` | `schemaVersion`, `deviceUnavailableMaxMs` (milliseconds, cannot be disabled); `[goal_budget_policy.limits]` contains `activeTimeMs`, `deadlineMs` (milliseconds), `modelTokens`, `modelCalls`, `toolCalls`, `slices`, `stalledSlices` |
| `[schedule_budget_policy.maximum]` | `max_runs_per_utc_day`, `max_calls_per_run`, `max_model_tokens_per_run`, `max_runtime_seconds` (seconds) |
| `[usage_retention]` | `turn_days`, `agent_session_days` (days) |
| `[model_metrics]` | `enabled`, `detail_days`, `five_minute_days`, `hourly_days`, `mutable_days` (days), `detail_row_budget`, `event_row_budget`, `compact_row_budget`, `rollup_row_budget`, `series_per_bucket`, `storage_budget_bytes` (bytes, decimal string) |

AI gateway and approval gateway tests use their respective `runtime_max_output_tokens`, sharing the output limit with runtime calls. There is no separate probe output token setting.

Missing sections or fields use the existing business defaults. Unknown fields within these sections and invalid values reject loading. Revision fields (`revision`, `connection_revision`, `profile_revision`, `configuration_revision`) use decimal strings, for example `revision = "1"`. The service maintains revisions and the instance identity, fingerprints and revision records in `[oss_config_metadata]`; leave these fields managed by the service. Startup reconciles actual business changes after a stopped-service edit; whitespace and key ordering do not advance revisions. Gateway probes remain in SQLite; revalidate an edited gateway when prompted by the settings page.

`request_options_json` is a JSON object string and preserves nested `null`. Each optional goal limit accepts a positive integer or the string `"disabled"`; omission uses its default. The device-unavailable wait bound always requires a valid positive integer.

```toml
[ai_gateway]
wire_protocol = "anthropic_messages"
base_url = "https://model.example/v1"
model = "example-model"
api_key = "replace-with-your-key"
max_context_bytes = 131072
request_options_json = '{"thinking":{"type":"adaptive","display":null}}'

[approval_gateway]
enabled = false

[goal_budget_policy.limits]
modelTokens = "disabled"

[usage_retention]
turn_days = 30
agent_session_days = 30
```

The file contains API keys. Public settings responses still return only whether a key is set. Complete-document saves and the existing worker initialization retain all sections. Context policy stays pinned when each model seam first uses it; goals apply current budgets at their existing checkpoints without resetting counters; retention changes apply on the next cleanup cycle.

`desk_signal.db` retains sessions, tasks, permissions, probes and usage; `model-metrics.sqlite` retains metric records and runtime watermarks. During development, stop the service and manually rebuild incompatible runtime databases. The new format does not read, convert or migrate old configuration tables. Settings already saved in `config.toml` survive a database rebuild.


## Recommended Development Config

```toml
[log]
log_level = "debug"
traceback = true

[desk]
video_fps = 30               # Reduce FPS during development to save resources
```

## Purpose output limits

The Context management and Terminal completion pages save the following global sections. Settings submission manages revision. Values must be integers from 1 to 4294967295; effective output is the smaller of the purpose value and the model runtime limit.

```toml
[context_management]
schema_version = 2
revision = "0"
strategy = "checkpoint_summary"
summary_max_output_tokens = 16384

[terminal_completion]
revision = "0"
max_output_tokens = 512
```

### Thinking request profile

The current profile schema is `2`. Set `reasoning_contract` explicitly to `conservative`, `openai_chat`, `deepseek_chat` or `anthropic_messages`, matching `wire_protocol`. `anthropic_prefix_binding` defaults to `false`; enable it only for an Anthropic endpoint that binds signed thinking to the request prefix. Thinking effort and optional native clearing remain inside `request_options_json`; see [AI diagnostics](../features/ai-diagnostics.md#thinking-replay-clearing-and-short-references). Both the main and approval gateways use these fields.
