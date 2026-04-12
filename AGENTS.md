# Repository Guidelines

## Project Structure & Module Organization
This repository is a Rust workspace with a Vite frontend.

- `server/`: main desktop service (Actix-Web, WebRTC, REST/OpenAPI) with integration tests in `server/tests/`.
- `signal/`, `turn/`, `signal-facade/`, `server-user/`, `server-version/`, `utils/`: supporting Rust crates.
- `vite-project/`: React + TypeScript UI (`src/features`, `src/components`, `src/services`).
- `tauri-app/src-tauri/`: desktop shell for privacy-screen and whiteboard features.
- `conf/config.toml`: runtime config; `openapi.json` + `vite-project/openapi.json`: API specs.

## Build, Test, and Development Commands
Run commands from repo root unless noted.

- `cargo run -p lcxl-remote-desk-server`: start backend in default mode.
- `cargo run -p lcxl-remote-desk-server -- --help`: view startup flags (`-m default|signaling|desk-server`).
- `cargo build --workspace --release`: build all Rust crates.
- `cargo test --workspace`: run Rust tests (including `server/tests/test_utils.rs`).
- `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings`: format and lint backend.
- `cd vite-project && npm ci && npm run dev`: start frontend at Vite dev server (default `5174`).
- `cd vite-project && npm run build`: type-check and build frontend.
- `cd vite-project && ./update_openapi.ps1`: refresh frontend API client from `http://localhost:8081/openapi.json`.

## Coding Style & Naming Conventions
- Rust: follow `rustfmt`, snake_case for modules/files/functions, PascalCase for types, `SCREAMING_SNAKE_CASE` for constants.
- TypeScript/React: 4-space indentation in current code, PascalCase for components, `useXxx` for hooks, kebab-case filenames in `src/components/ui`.
- Keep generated API artifacts under `vite-project/src/services/`; do not hand-edit generated hook/type files.

## Testing Guidelines
- Prefer crate-local unit tests and integration tests under `server/tests/`.
- Test names should describe behavior (example: `test_rejects_invalid_turn_secret`).
- For frontend changes, at minimum validate with `npm run build` and manual flow checks (see `vite-project/test_flow.mjs` when relevant).

## Commit & Pull Request Guidelines
- Follow Conventional Commits (`feat:`, `fix:`, `chore:`), consistent with recent history.
- Keep commits focused and functional; include config/schema updates in the same commit when required.
- PRs should include: concise description, affected modules (e.g., `server/service/signaling`), test/verification steps, linked issues, and UI screenshots for frontend changes.

## Security & Configuration Tips
- Never commit real credentials; use `conf/config.toml` placeholders for local dev.
- Review changes touching auth, signaling, TURN, or file-transfer paths with extra care.

## Signaling Authentication & Multi-Role Connection Architecture (CRITICAL)
This project features complex 4-way signaling connections. Handle logic strictly following this dual-track authentication spec:
- **Desk Server -> Local Signaling Server:** Start local connection only in `default` mode. Authenticate using auto-generated and persisted `settings.system.local_signaling_token`.
- **Desk Server -> Remote Signaling Server:** Authenticate by passing `token` (`settings.system.signaling_token`) as a WebSocket URL query parameter.
- **Desk Server -> Manager Server:** Authenticate by passing `token` (`settings.system.manager_api_token`) via WebSocket URL query parameters (validates against the manager database).
- **Browser -> Signaling / Manager Server:** Browsers connect to signaling **WITHOUT any Token query parameters**. They **MUST** fall back to Session (Actix-Session Cookie) authentication. Backend route extractors must use `Option<web::Query<VersionInfo>>` for compatibility, and manager signaling routes must be excluded from global Session interception middlewares.
