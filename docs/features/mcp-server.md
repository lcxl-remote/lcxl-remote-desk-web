# MCP Server

Running `--startup-mode mcp-stdio` turns the device into a [Model Context Protocol](https://modelcontextprotocol.io/) server, offering a **static whitelist of read-only tools** to local AI assistants.

## Design

- Built on the official `rmcp` SDK over **stdio**.
- Exposes a **static whitelist** of read-only tools — by construction, there are **no execute, write, or control tools** ("undefined means unreachable").
- The `lcxl_diagnose` tool's provider signature carries **no screenshot option**, so MCP clients structurally cannot capture the screen.

## Why a Separate Mode?

The MCP surface is intentionally narrower than the in-session diagnostic agent. Screen capturing and any control/execution tools are completely excluded to keep the attack surface minimal for local AI assistants.

## Running It

```bash
cargo run -- --startup-mode mcp-stdio
```

::: warning stdout is reserved
In `mcp-stdio` mode, stdin/stdout carry MCP JSON-RPC. The server **must never log to stdout** in this mode — doing so corrupts the protocol stream.
:::

## Connecting an AI Assistant

Point any MCP-capable client at the command above as a stdio server. The client will discover the read-only tool whitelist; no additional configuration grants write or execute access, because those tools do not exist.

See also the [AI Security Model](/security/ai-security-model) for how the MCP surface fits the overall trust boundary.
