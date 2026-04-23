# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

`lcxl-remote-desk` open-source WebRTC remote desktop solution. Backend in Rust (Actix-Web), frontend in React + TypeScript (Vite). The `server` binary can run in three modes: full (`default`), signaling-only (`signaling`), or desk-server-only (`desk-server`).

## Build & Run

```bash
# Backend
cargo run -p lcxl-remote-desk-server                  # default (full) mode
cargo run -p lcxl-remote-desk-server -- --help         # see all startup flags
cargo build --workspace --release
cargo test --workspace

cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings

# Frontend
cd vite-project && npm ci && npm run dev               # dev server (default :5174)
cd vite-project && npm run build                       # type-check + build

# After backend API changes, regenerate frontend client
# Windows: cd vite-project && .\update_openapi.ps1
# Linux/macOS: cd vite-project && ./update_openapi.sh
# (requires server running on :8081)
```

### Linux system dependencies

```bash
sudo apt install -y build-essential pkg-config libssl-dev libasound2-dev \
  libpipewire-0.3-dev libx11-dev libxcb1-dev libxcb-randr0-dev libxext-dev \
  clang libclang-dev cmake libvpx-dev
```

### API docs (while server is running)

Swagger UI: `http://localhost:8081/swagger-ui/` | OpenAPI spec: `http://localhost:8081/openapi.json`

## Module Overview

| Module | Role |
|---|---|
| `server/` | Desk server: REST API (Actix-Web), WebRTC, settings, file/terminal management |
| `signal/` | Signaling server + TURN (key file: `signal/src/service.rs`) |
| `vite-project/` | React 19 + TanStack Query frontend — both admin UI and web control client |
| `tauri-app/` | Tauri shell for privacy-screen / whiteboard features rendered locally on controlled machine |
| `signal-facade/` | Shared signaling protocol models (used by both `signal` and `manager`) |
| `utils/` | Common utilities |
| `turn/` | TURN server (bundled with signaling) |
| `server-version/` | API version constant |

## Adding a New API Endpoint

1. Define models in `server/src/model/`
2. Implement logic in `server/src/service/`
3. Add route handler in `server/src/controller/` with `utoipa` annotations
4. Register route in `server/src/main.rs`
5. Run the OpenAPI update script to regenerate frontend client

## Adding a New Signaling Type

1. Add the new variant with a unique integer value to `SignalingType` in `signal-facade/src/model/signal.rs`.
2. Handle it in `signal/src/service.rs` `handle_message` — add to forwarding union branch or write a dedicated match arm. **Never add a `_ =>` catch-all** (compiler enforces exhaustiveness).
3. Update frontend: run `/update_openapi`, then add `onMessage` handler in `vite-project/src/features/desk/hooks/useDeskRTC.ts`.

## Signaling Authentication (CRITICAL)

| Connection | Auth method |
|---|---|
| Desk Server → Local Signaling | `settings.system.local_signaling_token` (auto-generated, `default` mode only) |
| Desk Server → Remote Signaling | `?token=<settings.system.signaling_token>` in WebSocket URL |
| Desk Server → Manager | `?token=<settings.system.manager_api_token>` in WebSocket URL |
| Browser → Signaling / Manager | **No token param.** Actix-Session Cookie only. Use `Option<web::Query<VersionInfo>>` in extractors; exclude manager signaling routes from global session middleware. |

## Frontend Rules

- **i18n (mandatory):** All user-visible text must use `useTranslation()` / `t()` — no hardcoded strings. Every new key must be added simultaneously to `vite-project/src/locales/zh-CN/pages.ts` **and** `vite-project/src/locales/en-US/pages.ts`.
- **Generated code:** Files under `vite-project/src/services/` (Kubb output) are auto-generated — do not hand-edit them.
- **Tauri IPC:** Windows loaded via external HTTP URL lose `__TAURI_INTERNALS__`. Never call `invoke()` or `listen()` from frontend pages. Use REST API for frontend→Rust calls; use `window.eval()` + `dispatchEvent` for Rust→frontend events. Listen with native `window.addEventListener`.

## Code Style

- **Rust:** `rustfmt`, `snake_case` functions/modules, `PascalCase` types, `SCREAMING_SNAKE_CASE` constants.
- **TypeScript/React:** 4-space indent, `PascalCase` components, `useXxx` hooks, kebab-case filenames in `src/components/ui`.
- **Comments** must be written in **English**.
- **Git commits** must be in **English**, following Conventional Commits (`feat:`, `fix:`, `chore:`).

## Task Archival Workflow

After successfully completing a planned task, create an archive document in `agent_works/web/` with filename `yyyy-MM-dd_{kebab-case-title}.md`. Include: implementation plan, task list, execution summary. Strip any sensitive data.
