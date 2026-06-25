# CLI Arguments

```bash
cargo run -- --help
```

## Arguments

- `-c, --config-file-path <PATH>` — path to configuration file (default: `conf/config`).
- `-s, --startup-mode <MODE>` — startup mode:
  - `default` — full mode with signaling and desk server.
  - `signaling` — signaling mode only (Signaling + TURN).
  - `desk-server` — desk server mode only.
  - `service-daemon` — system service daemon (SYSTEM / root) that manages session workers.
  - `session-worker` — worker process launched by the daemon inside the user's desktop session.
  - `mcp-stdio` — read-only MCP server over stdio.

See [Startup Modes](/guide/startup-modes) for what each mode does and when to use it.
