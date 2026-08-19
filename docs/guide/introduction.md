# Introduction

**LCXL Remote Desk** is an **AI-native**, open-source, high-performance remote desktop. It treats AI as a **first-class control plane alongside the browser**: beyond browser-based remote control, it ships a built-in diagnostic AI agent that reads device state and can propose owner-confirmed commands, while exposing only the read capabilities to external AI assistants via a read-only [MCP](https://modelcontextprotocol.io/) server.

The backend is written in Rust (Actix-Web); the frontend with React + Vite + Tailwind CSS.

::: warning Disclaimer
This project is currently in the early development stage. The codebase may be unstable, contain unfixed bugs, or have incomplete features.

**Security Warning**: Remote desktop technology involves deep access to computer systems. Ensure your network environment is secure when using this project. The author(s) shall not be held liable for any damages arising from the use of this project.
:::

## Why AI-Native?

The AI layer is **security-first and model-agnostic** (OpenAI-compatible and Anthropic APIs). The design rests on a few invariants:

- **The server is the sole authority on permissions** — control planes can never self-report their identity, scope, or risk.
- **The model defaults to suggesting rather than executing** — high-risk actions require explicit, server-mediated confirmation.
- **Data is strictly redacted before transmission, failing closed** — redaction failure blocks the request before the model is ever called.
- **Every call is audited** (metadata only) and **API keys stay server-side**.

See the [AI Security Model](/security/ai-security-model) for the full picture.

## Key Features

- **AI Diagnostics** — ask troubleshooting questions in plain language; the system collects and redacts read-only state for analysis. On the owner's own device, the agent may also request a supported-shell command, which remains blocked until the owner explicitly confirms that exact command.
- **Read-Only MCP Server** — `--startup-mode mcp-stdio` exposes a static whitelist of read-only tools to local AI assistants, with no execution or write permissions.
- **High-Performance Streaming** — WebRTC with AV1 / H.264 / VP8 / VP9 encoding and Opus audio.
- **Remote Terminal** — a built-in xterm.js terminal supporting full shell interactions.
- **File Management** — uploads, downloads, deletions, and a recycle-bin mechanism.
- **Clipboard Sync** — bidirectional text clipboard synchronization.
- **Remote Whiteboard** — draw and annotate on the remote screen (requires `tauri-app`).
- **Privacy Screen** — lock the local display and input during remote operations (requires `tauri-app`).
- **System Audio** — captures and synchronizes remote audio playback.
- **Multi-language** — UI and documentation available in English and Chinese.

## Where to Next?

- New here? Start with the [Quick Start](/guide/quick-start).
- Want to understand the moving parts? Read [Core Concepts](/guide/concepts) and [Startup Modes](/guide/startup-modes).
- Deploying for real? See [Deployment](/guide/deployment).
- Evaluating the security posture? Jump to the [Security](/security/ai-security-model) section.

## License

This project is licensed under the [Apache-2.0](https://github.com/lcxl-remote/lcxl-remote-desk-web/blob/main/LICENSE) License.
